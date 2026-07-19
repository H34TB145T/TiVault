use crate::catalog::Catalog;
use crate::error::{AppError, AppResult};
use crate::models::{AccountCredentials, LoginRequest, LoginResult};
use crate::security::{set_private_directory_permissions, TelegramCredentialStore};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use tdlib_rs::enums::{
    AuthorizationState, Chat as TdChat, ChatType, Chats as TdChats, File as TdFile, InputFile,
    InputMessageContent, Message as TdMessage, MessageContent, Messages as TdMessages, Update,
    User as TdUser, UserType,
};
use tdlib_rs::{functions, types};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tokio::time::{timeout, Duration, Instant};
use zeroize::Zeroize;

const AUTH_TIMEOUT: Duration = Duration::from_secs(45);
const FILE_OPERATION_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
const PREVIEW_RANGE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const RECOVERY_HISTORY_TIMEOUT: Duration = Duration::from_secs(45);

fn is_vault_manifest_document(caption: &str, filename: &str) -> bool {
    caption.starts_with("#TiVaultManifest")
        || caption.starts_with("#TeleVaultManifest")
        || filename.ends_with(".tvmanifest.json")
}

#[derive(Clone)]
struct ClientRoute {
    auth: watch::Sender<Option<AuthorizationState>>,
    updates: broadcast::Sender<Update>,
}

struct ConnectedClient {
    id: i32,
    session_path: PathBuf,
    me_id: RwLock<Option<i64>>,
    saved_messages_chat_id: RwLock<Option<i64>>,
    updates: broadcast::Sender<Update>,
}

struct PendingFlow {
    account: AccountCredentials,
    connection: Arc<ConnectedClient>,
    auth: watch::Receiver<Option<AuthorizationState>>,
    remove_session_on_failure: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedRecipient {
    pub chat_id: i64,
    pub username: String,
    pub display_name: String,
    pub initials: String,
    pub kind: String,
    pub verified: bool,
}

pub struct OwnProfilePhoto {
    pub id: i64,
    pub bytes: Option<Vec<u8>>,
}

pub struct TelegramManager {
    catalog: Catalog,
    credential_store: TelegramCredentialStore,
    sessions_dir: PathBuf,
    routes: Arc<StdRwLock<HashMap<i32, ClientRoute>>>,
    clients: Mutex<HashMap<String, Arc<ConnectedClient>>>,
    flows: Mutex<HashMap<String, PendingFlow>>,
    file_operations: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    receiver_stop: Arc<AtomicBool>,
    receiver_thread: StdMutex<Option<std::thread::JoinHandle<()>>>,
}

impl TelegramManager {
    pub fn new(catalog: Catalog, sessions_dir: PathBuf) -> AppResult<Self> {
        std::fs::create_dir_all(&sessions_dir)?;
        set_private_directory_permissions(&sessions_dir)?;
        let credential_store = TelegramCredentialStore;
        for (account_id, mut api_hash) in catalog.legacy_api_hashes()? {
            if credential_store
                .store_api_hash(&account_id, &api_hash)
                .is_ok()
            {
                catalog.clear_api_hash(&account_id)?;
            }
            api_hash.zeroize();
        }
        let routes = Arc::new(StdRwLock::new(HashMap::<i32, ClientRoute>::new()));
        let receiver_stop = Arc::new(AtomicBool::new(false));
        let receiver_thread =
            Self::start_receiver(Arc::clone(&routes), Arc::clone(&receiver_stop))?;

        Ok(Self {
            catalog,
            credential_store,
            sessions_dir,
            routes,
            clients: Mutex::new(HashMap::new()),
            flows: Mutex::new(HashMap::new()),
            file_operations: Mutex::new(HashMap::new()),
            receiver_stop,
            receiver_thread: StdMutex::new(Some(receiver_thread)),
        })
    }

    fn start_receiver(
        routes: Arc<StdRwLock<HashMap<i32, ClientRoute>>>,
        stop: Arc<AtomicBool>,
    ) -> AppResult<std::thread::JoinHandle<()>> {
        Ok(std::thread::Builder::new()
            .name("tivault-tdlib".into())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match tdlib_rs::receive() {
                        Some((update, client_id)) => {
                            let route = routes
                                .read()
                                .ok()
                                .and_then(|routes| routes.get(&client_id).cloned());
                            if let Some(route) = route {
                                if let Update::AuthorizationState(auth) = &update {
                                    route
                                        .auth
                                        .send_replace(Some(auth.authorization_state.clone()));
                                }
                                let _ = route.updates.send(update);
                            }
                        }
                        None => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            })?)
    }

    pub fn shutdown(&self) {
        self.receiver_stop.store(true, Ordering::Release);
        if let Ok(mut receiver) = self.receiver_thread.lock() {
            if let Some(receiver) = receiver.take() {
                let _ = receiver.join();
            }
        }
    }

    pub async fn disconnect_account(&self, account_id: &str) -> AppResult<()> {
        self.catalog.account_credentials(account_id)?;
        let connection = self.clients.lock().await.remove(account_id);
        if let Some(connection) = connection {
            let _ = timeout(Duration::from_secs(15), functions::close(connection.id)).await;
            if let Ok(mut routes) = self.routes.write() {
                routes.remove(&connection.id);
            }
        }
        self.catalog.set_account_connected(account_id, false)
    }

    pub async fn log_out_account(&self, account_id: &str) -> AppResult<PathBuf> {
        let credentials = self.account_credentials(account_id)?;
        let existing = { self.clients.lock().await.remove(account_id) };
        let connection = match existing {
            Some(connection) => connection,
            None => self.client(account_id).await?,
        };
        timeout(Duration::from_secs(45), functions::log_out(connection.id))
            .await
            .map_err(|_| AppError::Telegram("Telegram did not confirm logout in time".into()))?
            .map_err(Self::td_error)?;
        let _ = timeout(Duration::from_secs(15), functions::close(connection.id)).await;
        if let Ok(mut routes) = self.routes.write() {
            routes.remove(&connection.id);
        }
        Ok(PathBuf::from(credentials.session_path))
    }

    fn td_error(error: types::Error) -> AppError {
        AppError::Telegram(format!("{} (Telegram code {})", error.message, error.code))
    }

    fn validate_request(request: &LoginRequest, phone_required: bool) -> AppResult<()> {
        if request.api_id <= 0 || request.api_hash.trim().len() < 8 {
            return Err(AppError::Message(
                "Enter a valid Telegram API ID and API hash".into(),
            ));
        }
        if phone_required && !request.phone.trim().starts_with('+') {
            return Err(AppError::Message(
                "Enter the phone number in international format, beginning with +".into(),
            ));
        }
        Ok(())
    }

    fn credentials_for_request(
        &self,
        request: &LoginRequest,
        phone_required: bool,
    ) -> AppResult<(AccountCredentials, bool)> {
        if let Some(account_id) = request.account_id.as_deref() {
            let account = self.account_credentials(account_id)?;
            if account.api_id <= 0 || account.api_hash.trim().len() < 8 {
                return Err(AppError::Message(
                    "The saved Telegram API credentials are invalid".into(),
                ));
            }
            if phone_required && !account.phone.starts_with('+') {
                return Err(AppError::Message(
                    "The saved phone number is not valid".into(),
                ));
            }
            return Ok((account, false));
        }

        Self::validate_request(request, phone_required)?;
        let account_id = uuid::Uuid::new_v4().to_string();
        Ok((
            AccountCredentials {
                id: account_id.clone(),
                name: request.name.trim().to_string(),
                phone: request.phone.trim().to_string(),
                api_id: request.api_id,
                api_hash: request.api_hash.trim().to_string(),
                session_path: self
                    .sessions_dir
                    .join(format!("{account_id}.tdlib"))
                    .to_string_lossy()
                    .into_owned(),
            },
            true,
        ))
    }

    fn account_credentials(&self, account_id: &str) -> AppResult<AccountCredentials> {
        let mut credentials = self.catalog.account_credentials(account_id)?;
        match self.credential_store.api_hash(account_id) {
            Ok(Some(api_hash)) => {
                if !credentials.api_hash.is_empty() {
                    credentials.api_hash.zeroize();
                    self.catalog.clear_api_hash(account_id)?;
                }
                credentials.api_hash = api_hash;
            }
            Ok(None) if !credentials.api_hash.is_empty() => {
                if self
                    .credential_store
                    .store_api_hash(account_id, &credentials.api_hash)
                    .is_ok()
                {
                    self.catalog.clear_api_hash(account_id)?;
                }
            }
            Ok(None) => {
                return Err(AppError::Crypto(
                    "This account's Telegram API hash is missing. Reconnect the account.".into(),
                ));
            }
            Err(error) if !credentials.api_hash.is_empty() => {
                let _ = error;
            }
            Err(error) => return Err(error),
        }
        Ok(credentials)
    }

    pub fn forget_account_credentials(&self, account_id: &str) -> AppResult<()> {
        self.credential_store.remove_api_hash(account_id)
    }

    async fn wait_for_state<F>(
        auth: &mut watch::Receiver<Option<AuthorizationState>>,
        duration: Duration,
        description: &str,
        accept: F,
    ) -> AppResult<AuthorizationState>
    where
        F: Fn(&AuthorizationState) -> bool,
    {
        let deadline = Instant::now() + duration;
        loop {
            if let Some(state) = auth.borrow().clone() {
                if accept(&state) {
                    return Ok(state);
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AppError::Message(format!(
                    "Telegram did not respond while {description}."
                )));
            }
            timeout(remaining, auth.changed())
                .await
                .map_err(|_| {
                    AppError::Message(format!("Telegram did not respond while {description}."))
                })?
                .map_err(|_| {
                    AppError::Telegram(
                        "Telegram closed the authorization connection unexpectedly".into(),
                    )
                })?;
        }
    }

    async fn create_connection(
        &self,
        account: &AccountCredentials,
        remove_data_on_error: bool,
    ) -> AppResult<(
        Arc<ConnectedClient>,
        watch::Receiver<Option<AuthorizationState>>,
        AuthorizationState,
    )> {
        let session_path = PathBuf::from(&account.session_path);
        if session_path.is_file() {
            return Err(AppError::Message(format!(
                "The old Telegram session for '{}' cannot be reused. Remove and reconnect this account.",
                account.name
            )));
        }
        std::fs::create_dir_all(&session_path)?;
        set_private_directory_permissions(&session_path)?;
        let files_path = session_path.join("files");
        std::fs::create_dir_all(&files_path)?;
        set_private_directory_permissions(&files_path)?;

        let client_id = tdlib_rs::create_client();
        let (auth_tx, mut auth_rx) = watch::channel(None);
        let (updates, _) = broadcast::channel(256);
        self.routes
            .write()
            .map_err(|_| AppError::Message("Telegram state lock was poisoned".into()))?
            .insert(
                client_id,
                ClientRoute {
                    auth: auth_tx,
                    updates: updates.clone(),
                },
            );

        let connection = Arc::new(ConnectedClient {
            id: client_id,
            session_path: session_path.clone(),
            me_id: RwLock::new(None),
            saved_messages_chat_id: RwLock::new(None),
            updates,
        });

        // TDLib starts emitting authorization updates only after the first
        // request. Do not make initialization depend on this cosmetic logging
        // request's response: packaged macOS launches can deliver the auth
        // update before the response future is scheduled.
        tokio::spawn(async move {
            let _ = functions::set_log_verbosity_level(1, client_id).await;
        });

        let mut state =
            match Self::wait_for_state(&mut auth_rx, AUTH_TIMEOUT, "starting Telegram", |_| true)
                .await
            {
                Ok(state) => state,
                Err(error) => {
                    self.discard_connection(&connection, remove_data_on_error)
                        .await;
                    return Err(error);
                }
            };
        if matches!(state, AuthorizationState::WaitTdlibParameters) {
            if let Err(error) = functions::set_tdlib_parameters(
                false,
                session_path.to_string_lossy().into_owned(),
                files_path.to_string_lossy().into_owned(),
                String::new(),
                true,
                true,
                true,
                false,
                account.api_id,
                account.api_hash.clone(),
                "en-GB".into(),
                "TiVault Desktop".into(),
                "macOS".into(),
                env!("CARGO_PKG_VERSION").into(),
                client_id,
            )
            .await
            {
                self.discard_connection(&connection, remove_data_on_error)
                    .await;
                return Err(Self::td_error(error));
            }

            state = match Self::wait_for_state(
                &mut auth_rx,
                AUTH_TIMEOUT,
                "initializing Telegram",
                |state| !matches!(state, AuthorizationState::WaitTdlibParameters),
            )
            .await
            {
                Ok(state) => state,
                Err(error) => {
                    self.discard_connection(&connection, remove_data_on_error)
                        .await;
                    return Err(error);
                }
            };
        }

        Ok((connection, auth_rx, state))
    }

    fn remove_route(&self, client_id: i32) {
        if let Ok(mut routes) = self.routes.write() {
            routes.remove(&client_id);
        }
    }

    async fn discard_connection(&self, connection: &ConnectedClient, remove_data: bool) {
        let _ = timeout(Duration::from_secs(5), functions::close(connection.id)).await;
        self.remove_route(connection.id);
        if remove_data {
            let _ = std::fs::remove_dir_all(&connection.session_path);
        }
    }

    async fn load_identity(connection: &ConnectedClient) -> AppResult<types::User> {
        let TdUser::User(user) = functions::get_me(connection.id)
            .await
            .map_err(Self::td_error)?;
        *connection.me_id.write().await = Some(user.id);
        Ok(user)
    }

    async fn finish_login(&self, mut flow: PendingFlow) -> AppResult<()> {
        let user = Self::load_identity(&flow.connection).await?;
        if !user.phone_number.is_empty() {
            flow.account.phone = format!("+{}", user.phone_number.trim_start_matches('+'));
        }
        if self
            .credential_store
            .store_api_hash(&flow.account.id, &flow.account.api_hash)
            .is_ok()
        {
            flow.account.api_hash.zeroize();
        }
        self.catalog.insert_account(&flow.account)?;
        self.clients
            .lock()
            .await
            .insert(flow.account.id.clone(), flow.connection);
        Ok(())
    }

    fn result_for_state(flow_id: &str, state: &AuthorizationState) -> AppResult<LoginResult> {
        let mut result = LoginResult {
            flow_id: flow_id.into(),
            status: String::new(),
            hint: None,
            qr_url: None,
            expires_at: None,
        };
        match state {
            AuthorizationState::WaitCode(_) => result.status = "code_sent".into(),
            AuthorizationState::WaitOtherDeviceConfirmation(qr) => {
                result.status = "qr_pending".into();
                result.qr_url = Some(qr.link.clone());
            }
            AuthorizationState::WaitPassword(password) => {
                result.status = "password_required".into();
                if !password.password_hint.is_empty() {
                    result.hint = Some(password.password_hint.clone());
                }
            }
            AuthorizationState::WaitPremiumPurchase(_) => {
                return Err(AppError::Telegram(
                    "Telegram requires a Premium purchase before this account can sign in through a third-party app. Complete that requirement in the official Telegram app, then try again."
                        .into(),
                ));
            }
            AuthorizationState::WaitEmailAddress(_) | AuthorizationState::WaitEmailCode(_) => {
                return Err(AppError::Telegram(
                    "Telegram requires email verification for this sign-in. Complete or review the login in the official Telegram app, then retry with TiVault's QR code."
                        .into(),
                ));
            }
            AuthorizationState::WaitRegistration(_) => {
                return Err(AppError::Telegram(
                    "This phone number is not registered with Telegram. Create the account in the official Telegram app first."
                        .into(),
                ));
            }
            AuthorizationState::LoggingOut
            | AuthorizationState::Closing
            | AuthorizationState::Closed => {
                return Err(AppError::Telegram(
                    "Telegram closed this sign-in attempt. Start again.".into(),
                ));
            }
            AuthorizationState::WaitTdlibParameters
            | AuthorizationState::WaitPhoneNumber
            | AuthorizationState::Ready => {
                return Err(AppError::Telegram(
                    "Telegram returned an unexpected sign-in state".into(),
                ));
            }
        }
        Ok(result)
    }

    async fn finish_or_store(
        &self,
        flow_id: &str,
        flow: PendingFlow,
        state: AuthorizationState,
    ) -> AppResult<LoginResult> {
        if matches!(state, AuthorizationState::Ready) {
            let connection = Arc::clone(&flow.connection);
            let remove_session_on_failure = flow.remove_session_on_failure;
            if let Err(error) = self.finish_login(flow).await {
                self.discard_connection(&connection, remove_session_on_failure)
                    .await;
                return Err(error);
            }
            return Ok(LoginResult {
                flow_id: flow_id.into(),
                status: "connected".into(),
                hint: None,
                qr_url: None,
                expires_at: None,
            });
        }

        match Self::result_for_state(flow_id, &state) {
            Ok(result) => {
                self.flows.lock().await.insert(flow_id.into(), flow);
                Ok(result)
            }
            Err(error) => {
                self.discard_connection(&flow.connection, flow.remove_session_on_failure)
                    .await;
                Err(error)
            }
        }
    }

    pub async fn start_login(&self, request: LoginRequest) -> AppResult<LoginResult> {
        let (account, remove_session_on_failure) = self.credentials_for_request(&request, true)?;
        let flow_id = uuid::Uuid::new_v4().to_string();
        let (connection, auth, state) = self
            .create_connection(&account, remove_session_on_failure)
            .await?;
        let mut flow = PendingFlow {
            account,
            connection,
            auth,
            remove_session_on_failure,
        };

        if matches!(state, AuthorizationState::Ready) {
            return self.finish_or_store(&flow_id, flow, state).await;
        }
        if !matches!(state, AuthorizationState::WaitPhoneNumber) {
            return self.finish_or_store(&flow_id, flow, state).await;
        }

        if let Err(error) = functions::set_authentication_phone_number(
            flow.account.phone.clone(),
            None,
            flow.connection.id,
        )
        .await
        {
            self.discard_connection(&flow.connection, flow.remove_session_on_failure)
                .await;
            return Err(Self::td_error(error));
        }
        let state = match Self::wait_for_state(
            &mut flow.auth,
            AUTH_TIMEOUT,
            "sending the Telegram login code",
            |state| !matches!(state, AuthorizationState::WaitPhoneNumber),
        )
        .await
        {
            Ok(state) => state,
            Err(error) => {
                self.discard_connection(&flow.connection, flow.remove_session_on_failure)
                    .await;
                return Err(error);
            }
        };
        self.finish_or_store(&flow_id, flow, state).await
    }

    pub async fn start_qr_login(&self, request: LoginRequest) -> AppResult<LoginResult> {
        let (account, remove_session_on_failure) = self.credentials_for_request(&request, false)?;
        let flow_id = uuid::Uuid::new_v4().to_string();
        let (connection, auth, state) = self
            .create_connection(&account, remove_session_on_failure)
            .await?;
        let mut flow = PendingFlow {
            account,
            connection,
            auth,
            remove_session_on_failure,
        };

        if matches!(state, AuthorizationState::Ready) {
            return self.finish_or_store(&flow_id, flow, state).await;
        }
        if !matches!(state, AuthorizationState::WaitPhoneNumber) {
            return self.finish_or_store(&flow_id, flow, state).await;
        }

        if let Err(error) =
            functions::request_qr_code_authentication(Vec::new(), flow.connection.id).await
        {
            self.discard_connection(&flow.connection, flow.remove_session_on_failure)
                .await;
            return Err(Self::td_error(error));
        }
        let state = match Self::wait_for_state(
            &mut flow.auth,
            AUTH_TIMEOUT,
            "creating the Telegram QR code",
            |state| !matches!(state, AuthorizationState::WaitPhoneNumber),
        )
        .await
        {
            Ok(state) => state,
            Err(error) => {
                self.discard_connection(&flow.connection, flow.remove_session_on_failure)
                    .await;
                return Err(error);
            }
        };
        self.finish_or_store(&flow_id, flow, state).await
    }

    pub async fn poll_qr_login(&self, flow_id: &str) -> AppResult<LoginResult> {
        let flow = self.flows.lock().await.remove(flow_id).ok_or_else(|| {
            AppError::Message("This QR sign-in attempt has expired. Start again.".into())
        })?;
        let state = flow.auth.borrow().clone().ok_or_else(|| {
            AppError::Telegram("Telegram has not reported a sign-in state yet".into())
        })?;
        self.finish_or_store(flow_id, flow, state).await
    }

    pub async fn complete_login(&self, flow_id: &str, code: &str) -> AppResult<LoginResult> {
        let mut flow = self.flows.lock().await.remove(flow_id).ok_or_else(|| {
            AppError::Message("This sign-in attempt has expired. Start again.".into())
        })?;
        let current = flow.auth.borrow().clone();
        if !matches!(current, Some(AuthorizationState::WaitCode(_))) {
            self.flows.lock().await.insert(flow_id.into(), flow);
            return Err(AppError::Message(
                "Telegram is not waiting for a login code".into(),
            ));
        }

        if let Err(error) =
            functions::check_authentication_code(code.trim().into(), flow.connection.id).await
        {
            self.flows.lock().await.insert(flow_id.into(), flow);
            return Err(Self::td_error(error));
        }
        let state = match Self::wait_for_state(
            &mut flow.auth,
            AUTH_TIMEOUT,
            "checking the Telegram login code",
            |state| !matches!(state, AuthorizationState::WaitCode(_)),
        )
        .await
        {
            Ok(state) => state,
            Err(error) => {
                self.flows.lock().await.insert(flow_id.into(), flow);
                return Err(error);
            }
        };
        self.finish_or_store(flow_id, flow, state).await
    }

    pub async fn complete_password(&self, flow_id: &str, password: &str) -> AppResult<LoginResult> {
        let mut flow = self.flows.lock().await.remove(flow_id).ok_or_else(|| {
            AppError::Message("This sign-in attempt has expired. Start again.".into())
        })?;
        let current = flow.auth.borrow().clone();
        if !matches!(current, Some(AuthorizationState::WaitPassword(_))) {
            self.flows.lock().await.insert(flow_id.into(), flow);
            return Err(AppError::Message(
                "Telegram is not waiting for a two-step verification password".into(),
            ));
        }

        if let Err(error) =
            functions::check_authentication_password(password.into(), flow.connection.id).await
        {
            self.flows.lock().await.insert(flow_id.into(), flow);
            return Err(Self::td_error(error));
        }
        let state = match Self::wait_for_state(
            &mut flow.auth,
            AUTH_TIMEOUT,
            "checking the two-step verification password",
            |state| !matches!(state, AuthorizationState::WaitPassword(_)),
        )
        .await
        {
            Ok(state) => state,
            Err(error) => {
                self.flows.lock().await.insert(flow_id.into(), flow);
                return Err(error);
            }
        };
        self.finish_or_store(flow_id, flow, state).await
    }

    async fn client(&self, account_id: &str) -> AppResult<Arc<ConnectedClient>> {
        // Keep initialization under one lock so a folder upload cannot open the
        // same TDLib database twice while several files begin concurrently.
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(account_id).cloned() {
            return Ok(client);
        }

        let credentials = self.account_credentials(account_id)?;
        let (connection, _auth, state) = self.create_connection(&credentials, false).await?;
        if !matches!(state, AuthorizationState::Ready) {
            self.discard_connection(&connection, false).await;
            let _ = self.catalog.set_account_connected(account_id, false);
            return Err(AppError::Telegram(format!(
                "Account '{}' needs to be connected again",
                credentials.name
            )));
        }
        if let Err(error) = Self::load_identity(&connection).await {
            self.discard_connection(&connection, false).await;
            return Err(error);
        }
        let _ = functions::optimize_storage(
            0,
            0,
            0,
            0,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            0,
            connection.id,
        )
        .await;
        clients.insert(account_id.into(), Arc::clone(&connection));
        Ok(connection)
    }

    async fn saved_messages_chat_id(connection: &ConnectedClient) -> AppResult<i64> {
        if let Some(chat_id) = *connection.saved_messages_chat_id.read().await {
            return Ok(chat_id);
        }

        let user_id = if let Some(user_id) = *connection.me_id.read().await {
            user_id
        } else {
            Self::load_identity(connection).await?.id
        };
        let TdChat::Chat(chat) = functions::create_private_chat(user_id, true, connection.id)
            .await
            .map_err(Self::td_error)?;
        *connection.saved_messages_chat_id.write().await = Some(chat.id);
        Ok(chat.id)
    }

    pub async fn own_profile_photo(
        &self,
        account_id: &str,
        cached_photo_id: Option<i64>,
    ) -> AppResult<Option<OwnProfilePhoto>> {
        const AVATAR_LIMIT: u64 = 5 * 1024 * 1024;
        const AVATAR_TIMEOUT: Duration = Duration::from_secs(30);

        let connection = self.client(account_id).await?;
        let user = Self::load_identity(&connection).await?;
        let Some(photo) = user.profile_photo else {
            return Ok(None);
        };
        if cached_photo_id == Some(photo.id) {
            return Ok(Some(OwnProfilePhoto {
                id: photo.id,
                bytes: None,
            }));
        }

        let mut file = photo.small;
        if file.local.is_downloading_completed && !Path::new(&file.local.path).is_file() {
            let _ = functions::delete_file(file.id, connection.id).await;
            file.local.is_downloading_completed = false;
            file.local.path.clear();
        }
        if !file.local.is_downloading_completed {
            let downloaded = timeout(
                AVATAR_TIMEOUT,
                functions::download_file(file.id, 1, 0, 0, true, connection.id),
            )
            .await
            .map_err(|_| AppError::Telegram("Telegram profile photo download timed out".into()))?
            .map_err(Self::td_error)?;
            let TdFile::File(downloaded) = downloaded;
            file = downloaded;
        }
        if !file.local.is_downloading_completed || file.local.path.is_empty() {
            return Err(AppError::Telegram(
                "Telegram did not provide the profile photo".into(),
            ));
        }
        let path = PathBuf::from(&file.local.path);
        let size = tokio::fs::metadata(&path).await?.len();
        if size == 0 || size > AVATAR_LIMIT {
            let _ = functions::delete_file(file.id, connection.id).await;
            return Err(AppError::Message(
                "The Telegram profile photo is empty or unexpectedly large".into(),
            ));
        }
        let bytes = tokio::fs::read(&path).await?;
        let _ = functions::delete_file(file.id, connection.id).await;
        Ok(Some(OwnProfilePhoto {
            id: photo.id,
            bytes: Some(bytes),
        }))
    }

    async fn file_operation_lock(&self, account_id: &str, message_id: i64) -> Arc<Mutex<()>> {
        let key = format!("{account_id}:{message_id}");
        let mut operations = self.file_operations.lock().await;
        operations
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn search_recipient(
        &self,
        account_id: &str,
        username: &str,
    ) -> AppResult<ResolvedRecipient> {
        let username = username.trim().trim_start_matches('@');
        if username.len() < 5
            || username.len() > 32
            || !username
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(AppError::Message(
                "Enter an exact public Telegram username, such as @username".into(),
            ));
        }
        let connection = self.client(account_id).await?;
        let TdChat::Chat(chat) = functions::search_public_chat(username.into(), connection.id)
            .await
            .map_err(Self::td_error)?;
        let user_id = match chat.r#type {
            ChatType::Private(private) => private.user_id,
            _ => {
                return Err(AppError::Message(
                    "TiVault currently sends files only to individual Telegram users".into(),
                ))
            }
        };
        if Some(user_id) == *connection.me_id.read().await {
            return Err(AppError::Message(
                "Choose another Telegram user; this account is your Saved Messages vault".into(),
            ));
        }
        let TdUser::User(user) = functions::get_user(user_id, connection.id)
            .await
            .map_err(Self::td_error)?;
        if !user.have_access || matches!(user.r#type, UserType::Deleted | UserType::Unknown) {
            return Err(AppError::Message(
                "Telegram does not allow this account to message that user".into(),
            ));
        }
        let primary_username = user
            .usernames
            .as_ref()
            .and_then(|names| names.active_usernames.first())
            .cloned()
            .unwrap_or_else(|| username.to_string());
        let display_name = format!("{} {}", user.first_name, user.last_name)
            .trim()
            .to_string();
        let initials = [
            user.first_name.chars().next(),
            user.last_name.chars().next(),
        ]
        .into_iter()
        .flatten()
        .collect::<String>()
        .to_uppercase();
        Ok(ResolvedRecipient {
            chat_id: chat.id,
            username: primary_username,
            display_name: if display_name.is_empty() {
                chat.title
            } else {
                display_name
            },
            initials: if initials.is_empty() {
                "TG".into()
            } else {
                initials
            },
            kind: if matches!(user.r#type, UserType::Bot(_)) {
                "bot".into()
            } else {
                "user".into()
            },
            verified: user
                .verification_status
                .as_ref()
                .map(|status| status.is_verified)
                .unwrap_or(false),
        })
    }

    pub async fn recent_recipients(
        &self,
        account_id: &str,
        limit: usize,
    ) -> AppResult<Vec<ResolvedRecipient>> {
        let connection = self.client(account_id).await?;
        let TdChats::Chats(chats) = functions::get_chats(
            None,
            (limit.saturating_mul(4)).clamp(10, 100) as i32,
            connection.id,
        )
        .await
        .map_err(Self::td_error)?;
        let me = *connection.me_id.read().await;
        let mut recipients = Vec::new();
        for chat_id in chats.chat_ids {
            if recipients.len() >= limit {
                break;
            }
            let TdChat::Chat(chat) = match functions::get_chat(chat_id, connection.id).await {
                Ok(chat) => chat,
                Err(_) => continue,
            };
            let user_id = match chat.r#type {
                ChatType::Private(private) if Some(private.user_id) != me => private.user_id,
                _ => continue,
            };
            let TdUser::User(user) = match functions::get_user(user_id, connection.id).await {
                Ok(user) => user,
                Err(_) => continue,
            };
            if !user.have_access || matches!(user.r#type, UserType::Deleted | UserType::Unknown) {
                continue;
            }
            let display_name = format!("{} {}", user.first_name, user.last_name)
                .trim()
                .to_string();
            let initials = [
                user.first_name.chars().next(),
                user.last_name.chars().next(),
            ]
            .into_iter()
            .flatten()
            .collect::<String>()
            .to_uppercase();
            recipients.push(ResolvedRecipient {
                chat_id,
                username: user
                    .usernames
                    .as_ref()
                    .and_then(|names| names.active_usernames.first())
                    .cloned()
                    .unwrap_or_default(),
                display_name: if display_name.is_empty() {
                    chat.title
                } else {
                    display_name
                },
                initials: if initials.is_empty() {
                    "TG".into()
                } else {
                    initials
                },
                kind: if matches!(user.r#type, UserType::Bot(_)) {
                    "bot".into()
                } else {
                    "user".into()
                },
                verified: user
                    .verification_status
                    .as_ref()
                    .map(|status| status.is_verified)
                    .unwrap_or(false),
            });
        }
        Ok(recipients)
    }

    pub async fn upload_document<F>(
        &self,
        account_id: &str,
        path: &Path,
        caption: &str,
        on_progress: F,
    ) -> AppResult<i64>
    where
        F: FnMut(u64, u64) -> bool,
    {
        let connection = self.client(account_id).await?;
        let chat_id = Self::saved_messages_chat_id(&connection).await?;
        Self::upload_document_to_chat(connection, chat_id, path, caption, on_progress).await
    }

    pub async fn send_document_to_chat<F>(
        &self,
        account_id: &str,
        chat_id: i64,
        path: &Path,
        caption: &str,
        on_progress: F,
    ) -> AppResult<i64>
    where
        F: FnMut(u64, u64) -> bool,
    {
        let connection = self.client(account_id).await?;
        Self::upload_document_to_chat(connection, chat_id, path, caption, on_progress).await
    }

    async fn upload_document_to_chat<F>(
        connection: Arc<ConnectedClient>,
        chat_id: i64,
        path: &Path,
        caption: &str,
        mut on_progress: F,
    ) -> AppResult<i64>
    where
        F: FnMut(u64, u64) -> bool,
    {
        let mut updates = connection.updates.subscribe();
        let content = InputMessageContent::InputMessageDocument(types::InputMessageDocument {
            document: InputFile::Local(types::InputFileLocal {
                path: path.to_string_lossy().into_owned(),
            }),
            thumbnail: None,
            disable_content_type_detection: true,
            caption: Some(types::FormattedText {
                text: caption.into(),
                entities: Vec::new(),
            }),
        });
        let TdMessage::Message(message) =
            functions::send_message(chat_id, None, None, None, content, connection.id)
                .await
                .map_err(Self::td_error)?;
        let upload_file = match &message.content {
            MessageContent::MessageDocument(document) => document.document.document.clone(),
            _ => {
                return Err(AppError::Telegram(
                    "Telegram did not create a document upload".into(),
                ))
            }
        };
        let file_id = upload_file.id;
        let total = upload_file
            .size
            .max(upload_file.expected_size)
            .max(
                path.metadata()
                    .map(|metadata| metadata.len() as i64)
                    .unwrap_or(0),
            )
            .max(1) as u64;
        let mut uploaded = upload_file.remote.uploaded_size.max(0) as u64;
        if !on_progress(uploaded.min(total), total) {
            if message.sending_state.is_some() {
                let _ = functions::delete_messages(chat_id, vec![message.id], true, connection.id)
                    .await;
            }
            return Err(AppError::Message("Transfer cancelled".into()));
        }
        if message.sending_state.is_none() {
            on_progress(total, total);
            return Ok(message.id);
        }

        let temporary_id = message.id;
        timeout(FILE_OPERATION_TIMEOUT, async {
            loop {
                if !on_progress(uploaded.min(total), total) {
                    let _ = functions::delete_messages(
                        chat_id,
                        vec![temporary_id],
                        true,
                        connection.id,
                    )
                    .await;
                    return Err(AppError::Message("Transfer cancelled".into()));
                }
                match timeout(Duration::from_millis(250), updates.recv()).await {
                    Err(_) => continue,
                    Ok(Ok(Update::File(update))) if update.file.id == file_id => {
                        uploaded = update.file.remote.uploaded_size.max(0) as u64;
                        if !on_progress(uploaded.min(total), total) {
                            continue;
                        }
                    }
                    Ok(Ok(Update::MessageSendSucceeded(sent)))
                        if sent.old_message_id == temporary_id =>
                    {
                        on_progress(total, total);
                        return Ok(sent.message.id);
                    }
                    Ok(Ok(Update::MessageSendFailed(failed)))
                        if failed.old_message_id == temporary_id =>
                    {
                        return Err(AppError::Telegram(format!(
                            "{} (Telegram code {})",
                            failed.error.message, failed.error.code
                        )))
                    }
                    Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                    Ok(Err(broadcast::error::RecvError::Closed)) => {
                        return Err(AppError::Telegram(
                            "Telegram closed while uploading the file".into(),
                        ))
                    }
                }
            }
        })
        .await
        .map_err(|_| AppError::Telegram("Telegram file upload timed out".into()))?
    }

    pub async fn download_document<F>(
        &self,
        account_id: &str,
        message_id: i64,
        destination: &Path,
        mut on_progress: F,
    ) -> AppResult<()>
    where
        F: FnMut(u64, u64) -> bool,
    {
        let operation = self.file_operation_lock(account_id, message_id).await;
        let _operation_guard = operation.lock().await;
        let connection = self.client(account_id).await?;
        let chat_id = Self::saved_messages_chat_id(&connection).await?;
        let TdMessage::Message(message) =
            functions::get_message(chat_id, message_id, connection.id)
                .await
                .map_err(Self::td_error)?;
        let mut file = match message.content {
            MessageContent::MessageDocument(document) => document.document.document,
            _ => {
                return Err(AppError::Telegram(format!(
                    "Telegram message {message_id} does not contain a TiVault file"
                )))
            }
        };
        let file_id = file.id;
        let TdFile::File(fresh) = functions::get_file(file_id, connection.id)
            .await
            .map_err(Self::td_error)?;
        file = fresh;
        if file.local.is_downloading_completed && !Path::new(&file.local.path).exists() {
            let _ = functions::delete_file(file_id, connection.id).await;
            file.local.is_downloading_completed = false;
            file.local.path.clear();
        }
        let mut updates = connection.updates.subscribe();
        let TdFile::File(started) =
            functions::download_file(file_id, 32, 0, 0, false, connection.id)
                .await
                .map_err(Self::td_error)?;
        file = started;
        let total = file.size.max(file.expected_size).max(1) as u64;
        file = timeout(FILE_OPERATION_TIMEOUT, async {
            loop {
                let downloaded = file.local.downloaded_size.max(0) as u64;
                if !on_progress(downloaded.min(total), total) {
                    let _ = functions::cancel_download_file(file_id, false, connection.id).await;
                    let _ = functions::delete_file(file_id, connection.id).await;
                    return Err(AppError::Message("Transfer cancelled".into()));
                }
                if file.local.is_downloading_completed {
                    return Ok(file);
                }
                match timeout(Duration::from_millis(250), updates.recv()).await {
                    Err(_) => continue,
                    Ok(Ok(Update::File(update))) if update.file.id == file_id => {
                        file = update.file;
                    }
                    Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                    Ok(Err(broadcast::error::RecvError::Closed)) => {
                        return Err(AppError::Telegram(
                            "Telegram closed while downloading the file".into(),
                        ))
                    }
                }
            }
        })
        .await
        .map_err(|_| AppError::Telegram("Telegram file download timed out".into()))??;
        if !file.local.is_downloading_completed || file.local.path.is_empty() {
            return Err(AppError::Telegram(format!(
                "Telegram message {message_id} could not be downloaded"
            )));
        }
        on_progress(total, total);
        let copy_result = tokio::fs::copy(&file.local.path, destination).await;
        let _ = functions::delete_file(file_id, connection.id).await;
        copy_result?;
        Ok(())
    }

    pub async fn download_document_range<F>(
        &self,
        account_id: &str,
        message_id: i64,
        offset: u64,
        length: u64,
        mut keep_going: F,
    ) -> AppResult<Vec<u8>>
    where
        F: FnMut() -> bool,
    {
        if length == 0 || length > 32 * 1024 * 1024 {
            return Err(AppError::Message("Preview range size is invalid".into()));
        }
        let operation = self.file_operation_lock(account_id, message_id).await;
        let _operation_guard = operation.lock().await;
        let connection = self.client(account_id).await?;
        let chat_id = Self::saved_messages_chat_id(&connection).await?;
        let TdMessage::Message(message) =
            functions::get_message(chat_id, message_id, connection.id)
                .await
                .map_err(Self::td_error)?;
        let mut file = match message.content {
            MessageContent::MessageDocument(document) => document.document.document,
            _ => {
                return Err(AppError::Telegram(format!(
                    "Telegram message {message_id} does not contain a TiVault file"
                )))
            }
        };
        let file_id = file.id;
        let TdFile::File(fresh) = functions::get_file(file_id, connection.id)
            .await
            .map_err(Self::td_error)?;
        file = fresh;
        let total = file.size.max(file.expected_size).max(0) as u64;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| AppError::Message("Preview range overflow".into()))?;
        if total > 0 && end > total {
            return Err(AppError::Message(
                "Preview range exceeds the Telegram part".into(),
            ));
        }

        if file.local.is_downloading_completed && !Path::new(&file.local.path).is_file() {
            let _ = functions::delete_file(file_id, connection.id).await;
            file.local.is_downloading_completed = false;
            file.local.path.clear();
        }
        let mut updates = connection.updates.subscribe();
        let TdFile::File(mut current) =
            functions::download_file(file_id, 32, offset as i64, 0, false, connection.id)
                .await
                .map_err(Self::td_error)?;
        let download_result = timeout(PREVIEW_RANGE_TIMEOUT, async {
            loop {
                if !keep_going() {
                    return Err(AppError::Message("Preview cancelled".into()));
                }
                let completed_file_covers_range = current.local.is_downloading_completed
                    && Path::new(&current.local.path)
                        .metadata()
                        .map(|metadata| metadata.is_file() && metadata.len() >= end)
                        .unwrap_or(false);
                let available_start = current.local.download_offset.max(0) as u64;
                let available_end = available_start
                    .saturating_add(current.local.downloaded_prefix_size.max(0) as u64);
                if completed_file_covers_range
                    || (available_start <= offset && available_end >= end)
                {
                    return Ok(current);
                }
                match timeout(Duration::from_millis(250), updates.recv()).await {
                    Err(_) => continue,
                    Ok(Ok(Update::File(update))) if update.file.id == file_id => {
                        current = update.file;
                    }
                    Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                    Ok(Err(broadcast::error::RecvError::Closed)) => {
                        return Err(AppError::Telegram(
                            "Telegram closed while buffering the preview".into(),
                        ));
                    }
                }
            }
        })
        .await
        .map_err(|_| AppError::Telegram("Telegram preview buffering timed out".into()));

        let current = match download_result {
            Ok(Ok(current)) => current,
            Ok(Err(error)) => {
                let _ = functions::cancel_download_file(file_id, false, connection.id).await;
                let _ = functions::delete_file(file_id, connection.id).await;
                return Err(error);
            }
            Err(error) => {
                let _ = functions::cancel_download_file(file_id, false, connection.id).await;
                let _ = functions::delete_file(file_id, connection.id).await;
                return Err(error);
            }
        };
        if !keep_going() {
            let _ = functions::cancel_download_file(file_id, false, connection.id).await;
            let _ = functions::delete_file(file_id, connection.id).await;
            return Err(AppError::Message("Preview cancelled".into()));
        }

        let read_result = async {
            if Path::new(&current.local.path)
                .metadata()
                .map(|metadata| metadata.is_file() && metadata.len() >= end)
                .unwrap_or(false)
            {
                let mut source = tokio::fs::File::open(&current.local.path).await?;
                source.seek(std::io::SeekFrom::Start(offset)).await?;
                let mut bytes = vec![0u8; length as usize];
                source.read_exact(&mut bytes).await?;
                return Ok(bytes);
            }

            match functions::read_file_part(file_id, offset as i64, length as i64, connection.id)
                .await
                .map_err(Self::td_error)?
            {
                tdlib_rs::enums::Data::Data(data) => {
                    let bytes = BASE64.decode(data.data).map_err(|_| {
                        AppError::Telegram("Telegram returned an invalid preview range".into())
                    })?;
                    if bytes.len() == length as usize {
                        Ok(bytes)
                    } else {
                        Err(AppError::Telegram(
                            "Telegram returned an incomplete preview range".into(),
                        ))
                    }
                }
            }
        }
        .await;

        let _ = functions::cancel_download_file(file_id, false, connection.id).await;
        let _ = functions::delete_file(file_id, connection.id).await;
        read_result
    }

    pub async fn manifest_message_ids(&self, account_id: &str) -> AppResult<(Vec<i64>, u64)> {
        let connection = self.client(account_id).await?;
        let chat_id = Self::saved_messages_chat_id(&connection).await?;
        let mut from_message_id = 0i64;
        let mut seen = std::collections::HashSet::new();
        let mut manifests = Vec::new();
        let mut scanned = 0u64;

        loop {
            let response = timeout(
                RECOVERY_HISTORY_TIMEOUT,
                functions::get_chat_history(chat_id, from_message_id, 0, 100, false, connection.id),
            )
            .await
            .map_err(|_| {
                AppError::Telegram(
                    "Timed out while scanning Saved Messages. Check the Telegram connection and try recovery again."
                        .into(),
                )
            })?
            .map_err(Self::td_error)?;
            let TdMessages::Messages(batch) = response;
            let messages = batch.messages.into_iter().flatten().collect::<Vec<_>>();
            if messages.is_empty() {
                break;
            }
            let mut oldest = from_message_id;
            let mut new_messages = 0usize;
            for message in messages {
                oldest = if oldest == 0 {
                    message.id
                } else {
                    oldest.min(message.id)
                };
                if !seen.insert(message.id) {
                    continue;
                }
                new_messages += 1;
                scanned += 1;
                if let MessageContent::MessageDocument(document) = message.content {
                    if is_vault_manifest_document(
                        &document.caption.text,
                        &document.document.file_name,
                    ) {
                        manifests.push(message.id);
                    }
                }
            }
            if new_messages == 0 || scanned >= 100_000 || oldest == 0 {
                break;
            }
            from_message_id = oldest;
        }
        Ok((manifests, scanned))
    }

    pub async fn document_message_size(&self, account_id: &str, message_id: i64) -> AppResult<u64> {
        let connection = self.client(account_id).await?;
        let chat_id = Self::saved_messages_chat_id(&connection).await?;
        let TdMessage::Message(message) =
            functions::get_message(chat_id, message_id, connection.id)
                .await
                .map_err(Self::td_error)?;
        let MessageContent::MessageDocument(document) = message.content else {
            return Err(AppError::Telegram(
                "The Telegram message is not a document".into(),
            ));
        };
        Ok(document
            .document
            .document
            .size
            .max(document.document.document.expected_size)
            .max(0) as u64)
    }

    pub async fn delete_messages(&self, account_id: &str, message_ids: &[i64]) -> AppResult<()> {
        let connection = self.client(account_id).await?;
        let chat_id = Self::saved_messages_chat_id(&connection).await?;
        Self::delete_chat_messages_with_connection(&connection, chat_id, message_ids).await
    }

    pub async fn delete_chat_messages(
        &self,
        account_id: &str,
        chat_id: i64,
        message_ids: &[i64],
    ) -> AppResult<()> {
        let connection = self.client(account_id).await?;
        Self::delete_chat_messages_with_connection(&connection, chat_id, message_ids).await
    }

    async fn delete_chat_messages_with_connection(
        connection: &ConnectedClient,
        chat_id: i64,
        message_ids: &[i64],
    ) -> AppResult<()> {
        for group in message_ids.chunks(100) {
            functions::delete_messages(chat_id, group.to_vec(), true, connection.id)
                .await
                .map_err(Self::td_error)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::is_vault_manifest_document;

    #[test]
    fn recovery_finds_legacy_and_rebranded_manifests() {
        assert!(is_vault_manifest_document(
            "#TeleVaultManifest v2 file=legacy",
            "manifest-legacy.tvmanifest.json"
        ));
        assert!(is_vault_manifest_document(
            "#TiVaultManifest v2 file=current",
            "manifest-current.tvmanifest.json"
        ));
        assert!(is_vault_manifest_document(
            "",
            "manifest-portable.tvmanifest.json"
        ));
        assert!(!is_vault_manifest_document("#TiVaultChunk v1", "chunk.bin"));
    }
}
