use crate::catalog::Catalog;
use crate::catalog::FileContext;
use crate::chunker::{
    append_chunk_to_file_with_progress, assemble_chunks_with_progress, decrypt_encrypted_frame,
    encrypted_chunk_plain_size, estimate_chunks, prepare_upload_streaming_with_progress,
    sha256_file, sha256_file_with_progress, PreparedChunk, ENCRYPTED_FRAME_OVERHEAD_BYTES,
    ENCRYPTED_MAGIC_BYTES, PREVIEW_BLOCK_BYTES,
};
use crate::error::{AppError, AppResult};
use crate::models::{
    ChunkRecord, HealthReport, LockStatus, PreviewInfo, PreviewText, RecoveryReport,
    RecoveryTestReport, ShareRecipient, UploadOptions, VaultFile, VaultFolder, VaultManifest,
};
use crate::security::{harden_private_tree, set_private_directory_permissions, MasterKeyStore};
use crate::telegram::{ResolvedRecipient, TelegramManager};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::time::{sleep, Duration};
use walkdir::WalkDir;
use zeroize::Zeroize;

const RUNNING: u8 = 0;
const PAUSED: u8 = 1;
const CANCELLED: u8 = 2;
const PREPARATION_FRACTION: f64 = 0.08;
const NETWORK_FRACTION: f64 = 0.91;
const DOWNLOAD_NETWORK_FRACTION: f64 = 0.90;
const PREVIEW_MIN_CACHE_BYTES: u64 = 128 * 1024 * 1024;
const PREVIEW_MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;
const PREVIEW_DEFAULT_CACHE_BYTES: u64 = 512 * 1024 * 1024;
const PREVIEW_DEFAULT_TTL_MINUTES: u64 = 15;
const PREVIEW_FULL_RESPONSE_LIMIT: u64 = 128 * 1024 * 1024;
const PREVIEW_TEXT_SOURCE_LIMIT: u64 = 64 * 1024 * 1024;
const PREVIEW_TEXT_OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;
const FREE_SPACE_RESERVE: u64 = 64 * 1024 * 1024;

struct WorkDirectoryGuard(PathBuf);

impl WorkDirectoryGuard {
    fn new(path: PathBuf) -> Self {
        Self(path)
    }
}

impl Drop for WorkDirectoryGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ProgressMeter {
    last_bytes: u64,
    last_sample: Instant,
    speed: f64,
}

#[derive(Clone)]
struct CachedPreviewBlock {
    path: PathBuf,
    size: u64,
    touched: Instant,
}

#[derive(Default)]
struct PreviewCacheState {
    entries: HashMap<u64, CachedPreviewBlock>,
    used: u64,
}

struct PreviewSession {
    token: String,
    file: FileContext,
    chunks: Vec<ChunkRecord>,
    local_path: Option<PathBuf>,
    kind: String,
    message: Option<String>,
    cache_dir: PathBuf,
    cache_limit: u64,
    ttl: Duration,
    created_at: chrono::DateTime<chrono::Utc>,
    last_access: Mutex<Instant>,
    cancelled: AtomicBool,
    io: AsyncMutex<()>,
    cache: Mutex<PreviewCacheState>,
}

#[derive(Clone)]
struct ShareTarget {
    file_id: String,
    account_id: String,
    chat_id: i64,
    username: String,
    display_name: String,
    expires: Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrivateManifestMetadata {
    name: String,
    folder_path: String,
    mime_type: String,
    category: String,
}

impl ProgressMeter {
    fn new(bytes: u64) -> Self {
        Self {
            last_bytes: bytes,
            last_sample: Instant::now(),
            speed: 0.0,
        }
    }

    fn observe(&mut self, bytes: u64) -> u64 {
        let elapsed = self.last_sample.elapsed().as_secs_f64();
        if elapsed >= 0.2 && bytes >= self.last_bytes {
            let instant = (bytes - self.last_bytes) as f64 / elapsed.max(0.001);
            self.speed = if self.speed == 0.0 {
                instant
            } else {
                self.speed * 0.65 + instant * 0.35
            };
            self.last_bytes = bytes;
            self.last_sample = Instant::now();
        }
        self.speed.max(0.0) as u64
    }
}

pub struct Core {
    pub catalog: Catalog,
    pub master: MasterKeyStore,
    pub telegram: TelegramManager,
    work_dir: PathBuf,
    controls: Mutex<HashMap<String, Arc<AtomicU8>>>,
    transfer_slots: Arc<Semaphore>,
    previews: Mutex<HashMap<String, Arc<PreviewSession>>>,
    share_targets: Mutex<HashMap<String, ShareTarget>>,
    locked: AtomicBool,
    last_activity: Mutex<Instant>,
}

impl Core {
    pub fn new(data_dir: &Path) -> AppResult<Arc<Self>> {
        fs::create_dir_all(data_dir)?;
        harden_private_tree(data_dir)?;
        let catalog = Catalog::new(data_dir.join("televault.sqlite3"))?;
        let locked = catalog.setting("app_lock_hash")?.is_some();
        let master = MasterKeyStore::load_or_create(data_dir)?;
        let telegram = TelegramManager::new(catalog.clone(), data_dir.join("sessions"))?;
        let work_dir = data_dir.join("work");
        let _ = fs::remove_dir_all(work_dir.join("uploads"));
        let _ = fs::remove_dir_all(work_dir.join("downloads"));
        let _ = fs::remove_dir_all(work_dir.join("shares"));
        let _ = fs::remove_dir_all(work_dir.join("previews"));
        fs::create_dir_all(&work_dir)?;
        set_private_directory_permissions(&work_dir)?;
        Ok(Arc::new(Self {
            catalog,
            master,
            telegram,
            work_dir,
            controls: Mutex::new(HashMap::new()),
            transfer_slots: Arc::new(Semaphore::new(4)),
            previews: Mutex::new(HashMap::new()),
            share_targets: Mutex::new(HashMap::new()),
            locked: AtomicBool::new(locked),
            last_activity: Mutex::new(Instant::now()),
        }))
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.work_dir.join("web-staging")
    }

    pub async fn account_avatar(&self, account_id: &str) -> AppResult<Option<String>> {
        self.catalog.account_credentials(account_id)?;
        let account_key = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(account_id.as_bytes()))
        };
        let account_dir = self.work_dir.join("avatars").join(account_key);
        let cached = cached_avatar(&account_dir);
        let cached_id = cached.as_ref().map(|(id, _)| *id);
        let photo = self
            .telegram
            .own_profile_photo(account_id, cached_id)
            .await?;

        let bytes = match photo {
            None => {
                let _ = tokio::fs::remove_dir_all(&account_dir).await;
                return Ok(None);
            }
            Some(photo) => match photo.bytes {
                None => cached.map(|(_, bytes)| bytes).ok_or_else(|| {
                    AppError::Message("The cached Telegram profile photo is unavailable".into())
                })?,
                Some(bytes) => {
                    let mime = raster_image_mime(&bytes).ok_or_else(|| {
                        AppError::Message(
                            "Telegram returned an unsupported profile photo format".into(),
                        )
                    })?;
                    let _ = tokio::fs::remove_dir_all(&account_dir).await;
                    tokio::fs::create_dir_all(&account_dir).await?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        tokio::fs::set_permissions(&account_dir, fs::Permissions::from_mode(0o700))
                            .await?;
                    }
                    let path = account_dir.join(format!("{}.avatar", photo.id));
                    tokio::fs::write(&path, &bytes).await?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        tokio::fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                            .await?;
                    }
                    debug_assert!(mime.starts_with("image/"));
                    bytes
                }
            },
        };
        let mime = raster_image_mime(&bytes).ok_or_else(|| {
            AppError::Message("The cached Telegram profile photo is invalid".into())
        })?;
        Ok(Some(format!("data:{mime};base64,{}", BASE64.encode(bytes))))
    }

    pub fn shutdown(&self) {
        self.telegram.shutdown();
    }

    pub fn lock_status(&self) -> AppResult<LockStatus> {
        Ok(LockStatus {
            enabled: self.catalog.setting("app_lock_hash")?.is_some(),
            locked: self.locked.load(Ordering::SeqCst),
            keychain_backed: self.master.keychain_backed(),
        })
    }

    pub fn ensure_unlocked(&self) -> AppResult<()> {
        if self.locked.load(Ordering::SeqCst) {
            return Err(AppError::Message("TiVault is locked".into()));
        }
        Ok(())
    }

    pub fn record_activity(&self) -> AppResult<LockStatus> {
        self.ensure_unlocked()?;
        *self.last_activity.lock().unwrap() = Instant::now();
        self.lock_status()
    }

    pub fn configure_app_lock(&self, password: &str) -> AppResult<LockStatus> {
        self.ensure_unlocked()?;
        if password.chars().count() < 8 {
            return Err(AppError::Message(
                "Use an app-lock password with at least 8 characters".into(),
            ));
        }
        let mut salt_bytes = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut salt_bytes);
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|_| AppError::Crypto("Could not create an app-lock salt".into()))?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| AppError::Crypto("Could not protect the app-lock password".into()))?
            .to_string();
        self.catalog.set_setting("app_lock_hash", hash)?;
        self.lock_status()
    }

    pub fn unlock(&self, password: &str) -> AppResult<LockStatus> {
        let encoded = self
            .catalog
            .setting("app_lock_hash")?
            .ok_or_else(|| AppError::Message("App lock is not enabled".into()))?;
        let parsed = PasswordHash::new(&encoded)
            .map_err(|_| AppError::Crypto("The stored app-lock verifier is invalid".into()))?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| AppError::Message("Incorrect app-lock password".into()))?;
        self.locked.store(false, Ordering::SeqCst);
        *self.last_activity.lock().unwrap() = Instant::now();
        self.lock_status()
    }

    pub fn lock(&self) -> AppResult<LockStatus> {
        if self.catalog.setting("app_lock_hash")?.is_some() {
            self.locked.store(true, Ordering::SeqCst);
        }
        self.lock_status()
    }

    pub fn disable_app_lock(&self, password: &str) -> AppResult<LockStatus> {
        self.unlock(password)?;
        self.catalog.delete_setting("app_lock_hash")?;
        self.locked.store(false, Ordering::SeqCst);
        self.lock_status()
    }

    pub async fn lock_timeout_loop(self: Arc<Self>) {
        loop {
            sleep(Duration::from_secs(15)).await;
            if self.locked.load(Ordering::SeqCst)
                || self
                    .catalog
                    .setting("app_lock_hash")
                    .ok()
                    .flatten()
                    .is_none()
            {
                continue;
            }
            let minutes = self
                .catalog
                .setting("app_lock_timeout_minutes")
                .ok()
                .flatten()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(15)
                .clamp(1, 120);
            if self.last_activity.lock().unwrap().elapsed()
                >= Duration::from_secs(minutes.saturating_mul(60))
            {
                self.locked.store(true, Ordering::SeqCst);
            }
        }
    }

    pub async fn queue_paths(
        self: &Arc<Self>,
        options: UploadOptions,
    ) -> AppResult<Vec<VaultFile>> {
        if options.paths.is_empty() {
            return Err(AppError::Message("Choose at least one file".into()));
        }
        let paths = collect_upload_files(&options.paths)?;
        let folder_root = options.folder_root.as_deref().map(PathBuf::from);
        let destination_folder =
            normalize_vault_path(options.destination_folder.as_deref().unwrap_or(""))?;
        let account = self.catalog.account_credentials(&options.account_id)?;
        let mut queued = Vec::new();
        for raw in paths {
            let path = PathBuf::from(&raw);
            let metadata = path
                .metadata()
                .map_err(|e| AppError::Message(format!("Cannot open '{}': {e}", path.display())))?;
            if !metadata.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|x| x.to_str())
                .ok_or_else(|| AppError::Message("A selected filename is not valid UTF-8".into()))?
                .to_string();
            let id = uuid::Uuid::new_v4().to_string();
            let transfer_id = uuid::Uuid::new_v4().to_string();
            let imported_folder = folder_path_for_upload(&path, folder_root.as_deref());
            let folder_path = join_vault_paths(&destination_folder, &imported_folder);
            self.catalog.ensure_vault_folder_path(&folder_path)?;
            let file = VaultFile {
                id: id.clone(),
                name: name.clone(),
                folder_path,
                category: category_for(&name).into(),
                size: metadata.len(),
                mime_type: mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .to_string(),
                encrypted: options.encrypt,
                cached: true,
                chunk_count: estimate_chunks(metadata.len()),
                account_id: account.id.clone(),
                account_name: account.name.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                status: "uploading".into(),
                thumbnail: None,
                favorite: false,
                tags: Vec::new(),
                last_opened_at: None,
                deleted_at: None,
                purge_at: None,
            };
            self.catalog.insert_queued_file(
                &file,
                &raw,
                &transfer_id,
                if options.duplicate_policy == "keep" {
                    "keep"
                } else {
                    "skip"
                },
            )?;
            queued.push(file);
            self.spawn_upload(id, transfer_id);
        }
        if queued.is_empty() {
            return Err(AppError::Message("No regular files were selected".into()));
        }
        Ok(queued)
    }

    pub fn expand_upload_paths(&self, paths: Vec<String>) -> AppResult<Vec<String>> {
        collect_upload_files(&paths)
    }

    pub fn create_folder(&self, parent_path: &str, name: &str) -> AppResult<VaultFolder> {
        let path = new_vault_folder_path(parent_path, name)?;
        self.catalog.create_vault_folder(&path)
    }

    fn build_manifest(
        &self,
        file: &FileContext,
        chunks: Vec<ChunkRecord>,
    ) -> AppResult<VaultManifest> {
        let (name, folder_path, mime_type, category, private_metadata, metadata_nonce) =
            if file.encrypted {
                let private = PrivateManifestMetadata {
                    name: file.name.clone(),
                    folder_path: file.folder_path.clone(),
                    mime_type: file.mime_type.clone(),
                    category: file.category.clone(),
                };
                let (sealed, nonce) = self.master.seal_metadata(&serde_json::to_vec(&private)?)?;
                (
                    format!("encrypted-{}", &file.id[..12.min(file.id.len())]),
                    None,
                    "application/octet-stream".into(),
                    "Other".into(),
                    Some(sealed),
                    Some(nonce),
                )
            } else {
                (
                    file.name.clone(),
                    (!file.folder_path.is_empty()).then(|| file.folder_path.clone()),
                    file.mime_type.clone(),
                    file.category.clone(),
                    None,
                    None,
                )
            };
        Ok(VaultManifest {
            format: "televault-manifest-v2".into(),
            file_id: file.id.clone(),
            name,
            folder_path,
            original_size: file.size,
            mime_type,
            category,
            encrypted: file.encrypted,
            original_sha256: file
                .original_sha256
                .clone()
                .ok_or_else(|| AppError::Message("The file has no verified content hash".into()))?,
            wrapped_key: file.wrapped_key.clone(),
            key_nonce: file.key_nonce.clone(),
            private_metadata,
            metadata_nonce,
            chunks,
            created_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    async fn upload_manifest_document(
        &self,
        account_id: &str,
        manifest: &VaultManifest,
    ) -> AppResult<i64> {
        let work = self
            .work_dir
            .join("manifests")
            .join(uuid::Uuid::new_v4().to_string());
        let _guard = WorkDirectoryGuard::new(work.clone());
        fs::create_dir_all(&work)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&work, fs::Permissions::from_mode(0o700))?;
        }
        let path = work.join(format!("manifest-{}.tvmanifest.json", manifest.file_id));
        fs::write(&path, serde_json::to_vec_pretty(manifest)?)?;
        self.telegram
            .upload_document(
                account_id,
                &path,
                &format!("#TiVaultManifest v2 file={}", manifest.file_id),
                |_, _| true,
            )
            .await
    }

    pub async fn rename_file(&self, file_id: &str, new_name: &str) -> AppResult<VaultFile> {
        let name = validate_file_name(new_name)?;
        let mut file = self.catalog.file_context(file_id)?;
        let old_manifest = file.manifest_message_id;
        file.name = name;
        file.mime_type = mime_guess::from_path(&file.name)
            .first_or_octet_stream()
            .to_string();
        file.category = category_for(&file.name).into();
        let manifest = self.build_manifest(&file, self.catalog.chunks(file_id)?)?;
        let manifest_id = self
            .upload_manifest_document(&file.account_id, &manifest)
            .await?;
        if let Err(error) = self.catalog.update_file_metadata(
            file_id,
            &file.name,
            &file.folder_path,
            &file.mime_type,
            &file.category,
            manifest_id,
        ) {
            let _ = self
                .telegram
                .delete_messages(&file.account_id, &[manifest_id])
                .await;
            return Err(error);
        }
        if let Some(old) = old_manifest.filter(|old| *old != manifest_id) {
            let _ = self
                .telegram
                .delete_messages(&file.account_id, &[old])
                .await;
        }
        self.catalog
            .dashboard(self.master.is_ready(), self.master.keychain_backed())?
            .files
            .into_iter()
            .find(|item| item.id == file_id)
            .ok_or_else(|| AppError::Message("The renamed file could not be reloaded".into()))
    }

    pub async fn move_file(&self, file_id: &str, folder_path: &str) -> AppResult<VaultFile> {
        let folder_path = normalize_vault_path(folder_path)?;
        let mut file = self.catalog.file_context(file_id)?;
        let old_manifest = file.manifest_message_id;
        file.folder_path = folder_path;
        let manifest = self.build_manifest(&file, self.catalog.chunks(file_id)?)?;
        let manifest_id = self
            .upload_manifest_document(&file.account_id, &manifest)
            .await?;
        if let Err(error) = self.catalog.update_file_metadata(
            file_id,
            &file.name,
            &file.folder_path,
            &file.mime_type,
            &file.category,
            manifest_id,
        ) {
            let _ = self
                .telegram
                .delete_messages(&file.account_id, &[manifest_id])
                .await;
            return Err(error);
        }
        if let Some(old) = old_manifest.filter(|old| *old != manifest_id) {
            let _ = self
                .telegram
                .delete_messages(&file.account_id, &[old])
                .await;
        }
        self.catalog
            .dashboard(self.master.is_ready(), self.master.keychain_backed())?
            .files
            .into_iter()
            .find(|item| item.id == file_id)
            .ok_or_else(|| AppError::Message("The moved file could not be reloaded".into()))
    }

    pub async fn copy_file(
        &self,
        file_id: &str,
        new_name: &str,
        folder_path: &str,
    ) -> AppResult<VaultFile> {
        let source = self.catalog.file_context(file_id)?;
        let name = validate_file_name(new_name)?;
        let folder_path = normalize_vault_path(folder_path)?;
        let new_id = uuid::Uuid::new_v4().to_string();
        let mut copy = source.clone();
        copy.id = new_id.clone();
        copy.name = name.clone();
        copy.folder_path = folder_path.clone();
        copy.mime_type = mime_guess::from_path(&copy.name)
            .first_or_octet_stream()
            .to_string();
        copy.category = category_for(&copy.name).into();
        copy.manifest_message_id = None;
        copy.duplicate_policy = "keep".into();
        let manifest = self.build_manifest(&copy, self.catalog.chunks(file_id)?)?;
        let manifest_id = self
            .upload_manifest_document(&copy.account_id, &manifest)
            .await?;
        if let Err(error) =
            self.catalog
                .insert_copy(&source, &new_id, &copy.name, &copy.folder_path, manifest_id)
        {
            let _ = self
                .telegram
                .delete_messages(&copy.account_id, &[manifest_id])
                .await;
            return Err(error);
        }
        self.catalog
            .dashboard(self.master.is_ready(), self.master.keychain_backed())?
            .files
            .into_iter()
            .find(|item| item.id == new_id)
            .ok_or_else(|| AppError::Message("The copied file could not be reloaded".into()))
    }

    pub async fn recover_vault(&self, account_id: &str) -> AppResult<RecoveryReport> {
        self.catalog.account_credentials(account_id)?;
        let (manifest_ids, scanned_messages) =
            self.telegram.manifest_message_ids(account_id).await?;
        let work = self
            .work_dir
            .join("recovery")
            .join(uuid::Uuid::new_v4().to_string());
        let _guard = WorkDirectoryGuard::new(work.clone());
        fs::create_dir_all(&work)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&work, fs::Permissions::from_mode(0o700))?;
        }
        let mut report = RecoveryReport {
            scanned_messages,
            manifests_found: manifest_ids.len() as u64,
            restored: 0,
            skipped: 0,
            warnings: Vec::new(),
        };
        let mut encountered = HashSet::new();
        for message_id in manifest_ids {
            let path = work.join(format!("{message_id}.json"));
            let result: AppResult<()> = async {
                self.telegram
                    .download_document(account_id, message_id, &path, |_, _| true)
                    .await?;
                if path.metadata()?.len() > 2 * 1024 * 1024 {
                    return Err(AppError::Message("Manifest is larger than 2 MB".into()));
                }
                let manifest: VaultManifest = serde_json::from_slice(&fs::read(&path)?)?;
                validate_recovery_manifest(&manifest)?;
                if !encountered.insert(manifest.file_id.clone())
                    || self.catalog.file_exists(&manifest.file_id)?
                {
                    report.skipped += 1;
                    return Ok(());
                }
                let private = match (
                    manifest.private_metadata.as_deref(),
                    manifest.metadata_nonce.as_deref(),
                ) {
                    (Some(sealed), Some(nonce)) => Some(
                        serde_json::from_slice::<PrivateManifestMetadata>(
                            &self.master.open_metadata(sealed, nonce)?,
                        )
                        .map_err(|_| {
                            AppError::Crypto("Private manifest metadata is invalid".into())
                        })?,
                    ),
                    (None, None) => None,
                    _ => {
                        return Err(AppError::Crypto(
                            "Private manifest metadata is incomplete".into(),
                        ))
                    }
                };
                if manifest.encrypted {
                    let wrapped = manifest.wrapped_key.as_deref().ok_or_else(|| {
                        AppError::Crypto("Encrypted manifest has no wrapped key".into())
                    })?;
                    let nonce = manifest.key_nonce.as_deref().ok_or_else(|| {
                        AppError::Crypto("Encrypted manifest has no key nonce".into())
                    })?;
                    let mut test_key = self.master.unwrap_file_key(wrapped, nonce)?;
                    test_key.zeroize();
                }
                let name = validate_file_name(
                    private
                        .as_ref()
                        .map(|metadata| metadata.name.as_str())
                        .unwrap_or(&manifest.name),
                )?;
                let folder_path = normalize_vault_path(
                    private
                        .as_ref()
                        .map(|metadata| metadata.folder_path.as_str())
                        .or(manifest.folder_path.as_deref())
                        .unwrap_or(""),
                )?;
                let mime_type = private
                    .as_ref()
                    .map(|metadata| metadata.mime_type.as_str())
                    .unwrap_or(&manifest.mime_type);
                let category = private
                    .as_ref()
                    .map(|metadata| metadata.category.as_str())
                    .filter(|value| is_known_category(value))
                    .unwrap_or_else(|| category_for(&name));
                self.catalog.recover_manifest(
                    account_id,
                    message_id,
                    &manifest,
                    &name,
                    &folder_path,
                    mime_type,
                    category,
                )?;
                report.restored += 1;
                Ok(())
            }
            .await;
            let _ = fs::remove_file(&path);
            if let Err(error) = result {
                report.skipped += 1;
                if report.warnings.len() < 20 {
                    report
                        .warnings
                        .push(format!("Manifest {message_id}: {error}"));
                }
            }
        }
        Ok(report)
    }

    pub async fn test_recovery(
        &self,
        account_id: &str,
        recovery_key: &str,
    ) -> AppResult<RecoveryTestReport> {
        self.catalog.account_credentials(account_id)?;
        let mut report = RecoveryTestReport {
            checked_at: chrono::Utc::now().to_rfc3339(),
            key_valid: self.master.verify_recovery(recovery_key),
            files_sampled: 0,
            manifests_valid: 0,
            chunks_available: 0,
            warnings: Vec::new(),
        };
        if !report.key_valid {
            report
                .warnings
                .push("The recovery key does not match this vault".into());
            return Ok(report);
        }
        let files = self.catalog.sample_ready_files(account_id, 3)?;
        report.files_sampled = files.len() as u64;
        if files.is_empty() {
            report
                .warnings
                .push("No stored files are available to simulate a restore".into());
            return Ok(report);
        }
        let work = self
            .work_dir
            .join("recovery-test")
            .join(uuid::Uuid::new_v4().to_string());
        let _guard = WorkDirectoryGuard::new(work.clone());
        fs::create_dir_all(&work)?;
        for file in files {
            let Some(manifest_id) = file.manifest_message_id else {
                push_warning(
                    &mut report.warnings,
                    format!("{} has no recovery manifest", file.name),
                );
                continue;
            };
            let manifest_path = work.join(format!("{manifest_id}.json"));
            let result: AppResult<()> = async {
                let remote_size = self
                    .telegram
                    .document_message_size(account_id, manifest_id)
                    .await?;
                if remote_size == 0 || remote_size > 2 * 1024 * 1024 {
                    return Err(AppError::Message(
                        "Recovery manifest size is invalid".into(),
                    ));
                }
                self.telegram
                    .download_document(account_id, manifest_id, &manifest_path, |_, _| true)
                    .await?;
                let manifest: VaultManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
                validate_recovery_manifest(&manifest)?;
                if manifest.file_id != file.id {
                    return Err(AppError::Message(
                        "Recovery manifest belongs to another file".into(),
                    ));
                }
                match (
                    manifest.private_metadata.as_deref(),
                    manifest.metadata_nonce.as_deref(),
                ) {
                    (Some(sealed), Some(nonce)) => {
                        let _ = self.master.open_metadata(sealed, nonce)?;
                    }
                    (None, None) => {}
                    _ => {
                        return Err(AppError::Crypto(
                            "Private manifest metadata is incomplete".into(),
                        ))
                    }
                }
                if manifest.encrypted {
                    let mut key = self.master.unwrap_file_key(
                        manifest.wrapped_key.as_deref().ok_or_else(|| {
                            AppError::Crypto("Encrypted manifest has no wrapped key".into())
                        })?,
                        manifest.key_nonce.as_deref().ok_or_else(|| {
                            AppError::Crypto("Encrypted manifest has no key nonce".into())
                        })?,
                    )?;
                    key.zeroize();
                }
                report.manifests_valid += 1;
                if let Some(chunk) = manifest.chunks.first() {
                    let size = self
                        .telegram
                        .document_message_size(account_id, chunk.message_id)
                        .await?;
                    if size != chunk.size {
                        return Err(AppError::Message(
                            "A sampled Telegram part has the wrong size".into(),
                        ));
                    }
                    report.chunks_available += 1;
                }
                Ok(())
            }
            .await;
            let _ = fs::remove_file(&manifest_path);
            if let Err(error) = result {
                push_warning(&mut report.warnings, format!("{}: {error}", file.name));
            }
        }
        Ok(report)
    }

    pub async fn health_check(
        &self,
        account_id: &str,
        sample_count: u64,
    ) -> AppResult<HealthReport> {
        self.catalog.account_credentials(account_id)?;
        let files = self.catalog.sample_ready_files(account_id, sample_count)?;
        let mut report = HealthReport {
            checked_at: chrono::Utc::now().to_rfc3339(),
            account_id: account_id.into(),
            files_sampled: files.len() as u64,
            chunks_checked: 0,
            hashes_verified: 0,
            missing: 0,
            corrupted: 0,
            healthy: true,
            warnings: Vec::new(),
        };
        let work = self
            .work_dir
            .join("health")
            .join(uuid::Uuid::new_v4().to_string());
        let _guard = WorkDirectoryGuard::new(work.clone());
        fs::create_dir_all(&work)?;
        for file in files {
            if let Some(manifest_id) = file.manifest_message_id {
                if let Err(error) = self
                    .telegram
                    .document_message_size(account_id, manifest_id)
                    .await
                {
                    report.missing += 1;
                    push_warning(
                        &mut report.warnings,
                        format!("{} manifest: {error}", file.name),
                    );
                }
            } else {
                report.missing += 1;
                push_warning(
                    &mut report.warnings,
                    format!("{} has no manifest", file.name),
                );
            }
            let Some(chunk) = self.catalog.chunks(&file.id)?.into_iter().next() else {
                report.missing += 1;
                push_warning(
                    &mut report.warnings,
                    format!("{} has no recorded parts", file.name),
                );
                continue;
            };
            report.chunks_checked += 1;
            match self
                .telegram
                .document_message_size(account_id, chunk.message_id)
                .await
            {
                Ok(size) if size == chunk.size => {
                    if size <= 32 * 1024 * 1024 {
                        let path = work.join(format!("{}-{}.part", file.id, chunk.index));
                        match self
                            .telegram
                            .download_document(account_id, chunk.message_id, &path, |_, _| true)
                            .await
                        {
                            Ok(()) => {
                                let actual = sha256_file(&path)?;
                                if actual == chunk.sha256 {
                                    report.hashes_verified += 1;
                                } else {
                                    report.corrupted += 1;
                                    push_warning(
                                        &mut report.warnings,
                                        format!("{} has a corrupted sampled part", file.name),
                                    );
                                }
                                let _ = fs::remove_file(path);
                            }
                            Err(error) => {
                                report.missing += 1;
                                push_warning(
                                    &mut report.warnings,
                                    format!("{} part: {error}", file.name),
                                );
                            }
                        }
                    }
                }
                Ok(_) => {
                    report.corrupted += 1;
                    push_warning(
                        &mut report.warnings,
                        format!("{} has a sampled part with the wrong size", file.name),
                    );
                }
                Err(error) => {
                    report.missing += 1;
                    push_warning(&mut report.warnings, format!("{} part: {error}", file.name));
                }
            }
        }
        report.healthy = report.missing == 0 && report.corrupted == 0;
        self.catalog
            .set_setting("latest_health_report", serde_json::to_string(&report)?)?;
        self.catalog
            .set_setting("last_health_check_at", &report.checked_at)?;
        Ok(report)
    }

    pub async fn health_check_loop(self: Arc<Self>) {
        sleep(Duration::from_secs(60)).await;
        loop {
            let enabled = self
                .catalog
                .setting("health_checks_enabled")
                .ok()
                .flatten()
                .as_deref()
                == Some("true");
            let interval_days = self
                .catalog
                .setting("health_check_interval_days")
                .ok()
                .flatten()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(7)
                .clamp(1, 30);
            let last = self
                .catalog
                .setting("last_health_check_at")
                .ok()
                .flatten()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
                .map(|date| date.with_timezone(&chrono::Utc));
            let due = last
                .map(|date| {
                    chrono::Utc::now().signed_duration_since(date).num_days()
                        >= interval_days as i64
                })
                .unwrap_or(true);
            if enabled && due && !self.locked.load(Ordering::SeqCst) {
                if let Ok(dashboard) = self
                    .catalog
                    .dashboard(self.master.is_ready(), self.master.keychain_backed())
                {
                    for account in dashboard
                        .accounts
                        .into_iter()
                        .filter(|account| account.connected)
                    {
                        let _ = self.health_check(&account.id, 3).await;
                    }
                }
            }
            sleep(Duration::from_secs(60 * 60)).await;
        }
    }

    pub fn set_favorite(&self, file_id: &str, favorite: bool) -> AppResult<()> {
        self.catalog.set_favorite(file_id, favorite)
    }

    pub fn set_tags(&self, file_id: &str, tags: Vec<String>) -> AppResult<()> {
        let mut normalized = Vec::new();
        for tag in tags {
            let tag = tag.trim();
            if tag.is_empty() {
                continue;
            }
            if tag.chars().count() > 32 || tag.contains('\n') || tag.contains('\r') {
                return Err(AppError::Message(
                    "Tags must be 32 characters or fewer".into(),
                ));
            }
            if !normalized
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(tag))
            {
                normalized.push(tag.to_string());
            }
            if normalized.len() >= 20 {
                break;
            }
        }
        self.catalog.set_tags(file_id, &normalized)
    }

    pub fn start_preview(&self, file_id: &str) -> AppResult<PreviewInfo> {
        let file = self.catalog.file_context(file_id)?;
        if file.status != "ready" {
            return Err(AppError::Message(
                "Only stored files can be previewed".into(),
            ));
        }
        self.catalog.touch_file(file_id)?;
        let (kind, message) = preview_kind(&file);
        let chunks = self.catalog.chunks(file_id)?;
        let local_path = self
            .catalog
            .cached_path(file_id)?
            .map(PathBuf::from)
            .filter(|path| {
                path.metadata()
                    .map(|metadata| metadata.is_file() && metadata.len() == file.size)
                    .unwrap_or(false)
            });
        if local_path.is_none() && chunks.is_empty() {
            return Err(AppError::Message(
                "This file has no available local copy or Telegram parts".into(),
            ));
        }
        let configured = self
            .catalog
            .setting("preview_cache_limit")?
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(PREVIEW_DEFAULT_CACHE_BYTES)
            .clamp(PREVIEW_MIN_CACHE_BYTES, PREVIEW_MAX_CACHE_BYTES);
        let ttl_minutes = self
            .catalog
            .setting("preview_cache_ttl_minutes")?
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(PREVIEW_DEFAULT_TTL_MINUTES)
            .clamp(5, 60);
        let token = uuid::Uuid::new_v4().simple().to_string();
        let cache_dir = self.work_dir.join("previews").join(&token);
        fs::create_dir_all(&cache_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o700))?;
        }
        let session = Arc::new(PreviewSession {
            token: token.clone(),
            file,
            chunks,
            local_path,
            kind,
            message,
            cache_dir,
            cache_limit: configured,
            ttl: Duration::from_secs(ttl_minutes * 60),
            created_at: chrono::Utc::now(),
            last_access: Mutex::new(Instant::now()),
            cancelled: AtomicBool::new(false),
            io: AsyncMutex::new(()),
            cache: Mutex::new(PreviewCacheState::default()),
        });
        let info = preview_info(&session);
        self.previews.lock().unwrap().insert(token, session);
        Ok(info)
    }

    fn preview_session(&self, token: &str) -> AppResult<Arc<PreviewSession>> {
        let session = self
            .previews
            .lock()
            .unwrap()
            .get(token)
            .cloned()
            .ok_or_else(|| AppError::Message("This preview has expired".into()))?;
        if session.cancelled.load(Ordering::SeqCst)
            || session.last_access.lock().unwrap().elapsed() >= session.ttl
        {
            return Err(AppError::Message("This preview has expired".into()));
        }
        *session.last_access.lock().unwrap() = Instant::now();
        Ok(session)
    }

    pub fn preview_info_for_token(&self, token: &str) -> AppResult<PreviewInfo> {
        self.preview_session(token)
            .map(|session| preview_info(&session))
    }

    pub async fn preview_bytes(
        &self,
        token: &str,
        start: u64,
        requested: u64,
    ) -> AppResult<Vec<u8>> {
        let session = self.preview_session(token)?;
        if session.file.size == 0 || start >= session.file.size {
            return Err(AppError::Message(
                "Preview range is outside the file".into(),
            ));
        }
        let length = requested
            .min(PREVIEW_BLOCK_BYTES)
            .min(session.file.size - start);
        let _io = session.io.lock().await;
        if session.cancelled.load(Ordering::SeqCst) {
            return Err(AppError::Message("Preview cancelled".into()));
        }
        if let Some(path) = &session.local_path {
            let mut file = tokio::fs::File::open(path).await?;
            file.seek(std::io::SeekFrom::Start(start)).await?;
            let mut output = vec![0u8; length as usize];
            file.read_exact(&mut output).await?;
            return Ok(output);
        }

        let first_block = start / PREVIEW_BLOCK_BYTES;
        let final_offset = start + length;
        let last_block = (final_offset.saturating_sub(1)) / PREVIEW_BLOCK_BYTES;
        let mut output = Vec::with_capacity(length as usize);
        for block_index in first_block..=last_block {
            let block = self.preview_block(&session, block_index).await?;
            let block_start = block_index * PREVIEW_BLOCK_BYTES;
            let from = start.saturating_sub(block_start) as usize;
            let to = ((final_offset - block_start) as usize).min(block.len());
            if from >= to || to > block.len() {
                return Err(AppError::Message("Preview block mapping is invalid".into()));
            }
            output.extend_from_slice(&block[from..to]);
        }
        Ok(output)
    }

    pub async fn preview_full_bytes(&self, token: &str) -> AppResult<Vec<u8>> {
        let info = self.preview_info_for_token(token)?;
        if info.size > PREVIEW_FULL_RESPONSE_LIMIT {
            return Err(AppError::Message(
                "This preview requires byte-range streaming".into(),
            ));
        }
        let mut output = Vec::with_capacity(info.size as usize);
        let mut offset = 0u64;
        while offset < info.size {
            let bytes = self
                .preview_bytes(token, offset, PREVIEW_BLOCK_BYTES)
                .await?;
            if bytes.is_empty() {
                return Err(AppError::Message("Preview ended unexpectedly".into()));
            }
            offset += bytes.len() as u64;
            output.extend_from_slice(&bytes);
        }
        Ok(output)
    }

    async fn preview_block(
        &self,
        session: &Arc<PreviewSession>,
        block_index: u64,
    ) -> AppResult<Vec<u8>> {
        let cached = {
            let mut cache = session.cache.lock().unwrap();
            cache.entries.get_mut(&block_index).map(|entry| {
                entry.touched = Instant::now();
                entry.path.clone()
            })
        };
        if let Some(path) = cached {
            if let Ok(bytes) = tokio::fs::read(&path).await {
                return Ok(bytes);
            }
            let mut cache = session.cache.lock().unwrap();
            if let Some(entry) = cache.entries.remove(&block_index) {
                cache.used = cache.used.saturating_sub(entry.size);
            }
        }

        let block_start = block_index
            .checked_mul(PREVIEW_BLOCK_BYTES)
            .ok_or_else(|| AppError::Message("Preview block offset overflow".into()))?;
        let block_len = PREVIEW_BLOCK_BYTES.min(session.file.size.saturating_sub(block_start));
        if block_len == 0 {
            return Err(AppError::Message(
                "Preview block is outside the file".into(),
            ));
        }
        let bytes = if session.file.encrypted {
            self.download_encrypted_preview_block(session, block_start, block_len)
                .await?
        } else {
            self.download_plain_preview_block(session, block_start, block_len)
                .await?
        };
        self.store_preview_block(session, block_index, &bytes)
            .await?;
        Ok(bytes)
    }

    async fn download_plain_preview_block(
        &self,
        session: &PreviewSession,
        start: u64,
        length: u64,
    ) -> AppResult<Vec<u8>> {
        let mut output = Vec::with_capacity(length as usize);
        let mut file_offset = 0u64;
        let mut cursor = start;
        let end = start + length;
        for chunk in &session.chunks {
            let chunk_end = file_offset + chunk.size;
            if cursor < chunk_end && end > file_offset {
                let remote_start = cursor.saturating_sub(file_offset);
                let take = (end.min(chunk_end) - cursor).min(32 * 1024 * 1024);
                let bytes = self
                    .telegram
                    .download_document_range(
                        &session.file.account_id,
                        chunk.message_id,
                        remote_start,
                        take,
                        || !session.cancelled.load(Ordering::SeqCst),
                    )
                    .await?;
                output.extend_from_slice(&bytes);
                cursor += bytes.len() as u64;
                if cursor >= end {
                    break;
                }
            }
            file_offset = chunk_end;
        }
        if output.len() != length as usize {
            return Err(AppError::Message(
                "Telegram parts do not cover the requested preview block".into(),
            ));
        }
        Ok(output)
    }

    async fn download_encrypted_preview_block(
        &self,
        session: &PreviewSession,
        start: u64,
        length: u64,
    ) -> AppResult<Vec<u8>> {
        let mut plain_base = 0u64;
        let mut selected = None;
        for chunk in &session.chunks {
            let plain_size = encrypted_chunk_plain_size(chunk.size)?;
            if start < plain_base + plain_size {
                selected = Some((chunk, plain_base, plain_size));
                break;
            }
            plain_base += plain_size;
        }
        let (chunk, chunk_plain_base, chunk_plain_size) = selected.ok_or_else(|| {
            AppError::Crypto("Encrypted Telegram parts do not cover this preview block".into())
        })?;
        let in_chunk = start - chunk_plain_base;
        if !in_chunk.is_multiple_of(PREVIEW_BLOCK_BYTES) || length > chunk_plain_size - in_chunk {
            return Err(AppError::Crypto(
                "Encrypted preview block is not aligned with its authenticated frame".into(),
            ));
        }
        let frame_index = in_chunk / PREVIEW_BLOCK_BYTES;
        let remote_offset = ENCRYPTED_MAGIC_BYTES
            + frame_index * (PREVIEW_BLOCK_BYTES + ENCRYPTED_FRAME_OVERHEAD_BYTES);
        let remote_length = length + ENCRYPTED_FRAME_OVERHEAD_BYTES;
        let frame = self
            .telegram
            .download_document_range(
                &session.file.account_id,
                chunk.message_id,
                remote_offset,
                remote_length,
                || !session.cancelled.load(Ordering::SeqCst),
            )
            .await?;
        let mut key = self.master.unwrap_file_key(
            session
                .file
                .wrapped_key
                .as_deref()
                .ok_or_else(|| AppError::Crypto("Missing wrapped file key".into()))?,
            session
                .file
                .key_nonce
                .as_deref()
                .ok_or_else(|| AppError::Crypto("Missing file key nonce".into()))?,
        )?;
        let decrypted = decrypt_encrypted_frame(&frame, &key);
        key.zeroize();
        let decrypted = decrypted?;
        if decrypted.len() != length as usize {
            return Err(AppError::Crypto(
                "Encrypted preview frame has an unexpected size".into(),
            ));
        }
        Ok(decrypted)
    }

    async fn store_preview_block(
        &self,
        session: &PreviewSession,
        block_index: u64,
        bytes: &[u8],
    ) -> AppResult<()> {
        let required = bytes.len() as u64;
        let mut evicted = Vec::new();
        {
            let mut cache = session.cache.lock().unwrap();
            while cache.used.saturating_add(required) > session.cache_limit {
                let Some((&oldest, _)) =
                    cache.entries.iter().min_by_key(|(_, entry)| entry.touched)
                else {
                    break;
                };
                if let Some(entry) = cache.entries.remove(&oldest) {
                    cache.used = cache.used.saturating_sub(entry.size);
                    evicted.push(entry.path);
                }
            }
        }
        for path in evicted {
            let _ = tokio::fs::remove_file(path).await;
        }
        let available = fs2::available_space(&session.cache_dir)?;
        if available < required.saturating_add(FREE_SPACE_RESERVE) {
            return Err(AppError::Message(
                "Not enough free disk space to buffer this preview safely".into(),
            ));
        }
        let path = session
            .cache_dir
            .join(format!("block-{block_index:012}.cache"));
        let temporary = path.with_extension("tmp");
        let mut file = tokio::fs::File::create(&temporary).await?;
        file.write_all(bytes).await?;
        file.flush().await?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        }
        tokio::fs::rename(&temporary, &path).await?;
        let mut cache = session.cache.lock().unwrap();
        if let Some(previous) = cache.entries.insert(
            block_index,
            CachedPreviewBlock {
                path,
                size: required,
                touched: Instant::now(),
            },
        ) {
            cache.used = cache.used.saturating_sub(previous.size);
        }
        cache.used = cache.used.saturating_add(required);
        Ok(())
    }

    pub async fn preview_text(&self, token: &str) -> AppResult<PreviewText> {
        let session = self.preview_session(token)?;
        match session.kind.as_str() {
            "text" => {
                let limit = PREVIEW_TEXT_OUTPUT_LIMIT.min(session.file.size);
                let mut bytes = Vec::with_capacity(limit as usize);
                let mut offset = 0u64;
                while offset < limit {
                    let block = self
                        .preview_bytes(token, offset, PREVIEW_BLOCK_BYTES.min(limit - offset))
                        .await?;
                    if block.is_empty() {
                        break;
                    }
                    offset += block.len() as u64;
                    bytes.extend_from_slice(&block);
                }
                Ok(PreviewText {
                    content: String::from_utf8_lossy(&bytes).into_owned(),
                    truncated: session.file.size > limit,
                })
            }
            "document" => self.preview_document_text(&session).await,
            _ => Err(AppError::Message(
                "This file type doesn't have a text preview".into(),
            )),
        }
    }

    async fn preview_document_text(&self, session: &Arc<PreviewSession>) -> AppResult<PreviewText> {
        if session.file.size > PREVIEW_TEXT_SOURCE_LIMIT {
            return Err(AppError::Message(
                "This document is too large for a safe inline preview; download it to open externally"
                    .into(),
            ));
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = session;
            return Err(AppError::Message(
                "Safe Word-document preview is currently available on macOS only".into(),
            ));
        }
        #[cfg(target_os = "macos")]
        {
            let extension = Path::new(&session.file.name)
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("docx")
                .to_ascii_lowercase();
            if !matches!(extension.as_str(), "doc" | "docx" | "rtf" | "rtfd" | "odt") {
                return Err(AppError::Message(
                    "This Office format cannot be safely converted for inline preview".into(),
                ));
            }
            let source = session.cache_dir.join(format!("document.{extension}"));
            if !source.exists() {
                let mut output = tokio::fs::File::create(&source).await?;
                let mut offset = 0u64;
                while offset < session.file.size {
                    let block = self
                        .preview_bytes(
                            &session.token,
                            offset,
                            PREVIEW_BLOCK_BYTES.min(session.file.size - offset),
                        )
                        .await?;
                    if block.is_empty() {
                        return Err(AppError::Message(
                            "Document preview ended unexpectedly".into(),
                        ));
                    }
                    output.write_all(&block).await?;
                    offset += block.len() as u64;
                }
                output.flush().await?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
                }
            }
            let mut child = tokio::process::Command::new("/usr/bin/textutil")
                .arg("-convert")
                .arg("txt")
                .arg("-stdout")
                .arg(&source)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| {
                    AppError::Message(format!("Unable to start safe document preview: {error}"))
                })?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| AppError::Message("Document converter returned no output".into()))?;
            let mut limited = stdout.take(PREVIEW_TEXT_OUTPUT_LIMIT + 1);
            let mut bytes = Vec::new();
            limited.read_to_end(&mut bytes).await?;
            let truncated = bytes.len() as u64 > PREVIEW_TEXT_OUTPUT_LIMIT;
            if truncated {
                bytes.truncate(PREVIEW_TEXT_OUTPUT_LIMIT as usize);
                let _ = child.kill().await;
            }
            let status = child.wait().await?;
            if !status.success() && !truncated {
                return Err(AppError::Message(
                    "macOS could not safely extract text from this document".into(),
                ));
            }
            Ok(PreviewText {
                content: String::from_utf8_lossy(&bytes).into_owned(),
                truncated,
            })
        }
    }

    pub async fn stop_preview(&self, token: &str) -> AppResult<()> {
        let session = self.previews.lock().unwrap().remove(token);
        if let Some(session) = session {
            session.cancelled.store(true, Ordering::SeqCst);
            let _io = session.io.lock().await;
            let _ = tokio::fs::remove_dir_all(&session.cache_dir).await;
        }
        Ok(())
    }

    pub async fn clear_preview_cache(&self) -> AppResult<u64> {
        let preview_root = self.work_dir.join("previews");
        let avatar_root = self.work_dir.join("avatars");
        let bytes = directory_size(&preview_root).saturating_add(directory_size(&avatar_root));
        let sessions = {
            let mut previews = self.previews.lock().unwrap();
            std::mem::take(&mut *previews)
                .into_values()
                .collect::<Vec<_>>()
        };
        for session in sessions {
            session.cancelled.store(true, Ordering::SeqCst);
            let _io = session.io.lock().await;
        }
        let _ = tokio::fs::remove_dir_all(&preview_root).await;
        let _ = tokio::fs::remove_dir_all(&avatar_root).await;
        tokio::fs::create_dir_all(&preview_root).await?;
        Ok(bytes)
    }

    pub async fn preview_cleanup_loop(self: Arc<Self>) {
        loop {
            sleep(Duration::from_secs(60)).await;
            let expired = {
                let previews = self.previews.lock().unwrap();
                previews
                    .iter()
                    .filter_map(|(token, session)| {
                        (session.last_access.lock().unwrap().elapsed() >= session.ttl)
                            .then_some(token.clone())
                    })
                    .collect::<Vec<_>>()
            };
            for token in expired {
                let _ = self.stop_preview(&token).await;
            }
            self.share_targets
                .lock()
                .unwrap()
                .retain(|_, target| target.expires > Instant::now());
        }
    }

    pub async fn lookup_share_recipient(
        &self,
        file_id: &str,
        username: &str,
    ) -> AppResult<ShareRecipient> {
        let file = self.catalog.file_context(file_id)?;
        let recipient = self
            .telegram
            .search_recipient(&file.account_id, username)
            .await?;
        Ok(self.register_share_recipient(&file, recipient))
    }

    pub async fn recent_share_recipients(&self, file_id: &str) -> AppResult<Vec<ShareRecipient>> {
        let file = self.catalog.file_context(file_id)?;
        let recipients = self.telegram.recent_recipients(&file.account_id, 6).await?;
        Ok(recipients
            .into_iter()
            .map(|recipient| self.register_share_recipient(&file, recipient))
            .collect())
    }

    fn folder_share_files(&self, folder_path: &str) -> AppResult<Vec<FileContext>> {
        let folder_path = normalize_vault_path(folder_path)?;
        let files = self
            .catalog
            .ready_file_ids_in_folder(&folder_path)?
            .into_iter()
            .map(|id| self.catalog.file_context(&id))
            .collect::<AppResult<Vec<_>>>()?;
        let first = files.first().ok_or_else(|| {
            AppError::Message("This folder does not contain any ready files to share".into())
        })?;
        if files.iter().any(|file| file.account_id != first.account_id) {
            return Err(AppError::Message(
                "This folder contains files from multiple Telegram accounts; share each account's files separately"
                    .into(),
            ));
        }
        Ok(files)
    }

    pub async fn lookup_folder_share_recipient(
        &self,
        folder_path: &str,
        username: &str,
    ) -> AppResult<ShareRecipient> {
        let files = self.folder_share_files(folder_path)?;
        let anchor = &files[0];
        let recipient = self
            .telegram
            .search_recipient(&anchor.account_id, username)
            .await?;
        Ok(self.register_share_recipient(anchor, recipient))
    }

    pub async fn recent_folder_share_recipients(
        &self,
        folder_path: &str,
    ) -> AppResult<Vec<ShareRecipient>> {
        let files = self.folder_share_files(folder_path)?;
        let anchor = &files[0];
        let recipients = self
            .telegram
            .recent_recipients(&anchor.account_id, 6)
            .await?;
        Ok(recipients
            .into_iter()
            .map(|recipient| self.register_share_recipient(anchor, recipient))
            .collect())
    }

    fn register_share_recipient(
        &self,
        file: &FileContext,
        recipient: ResolvedRecipient,
    ) -> ShareRecipient {
        let ResolvedRecipient {
            chat_id,
            username,
            display_name,
            initials,
            kind,
            verified,
        } = recipient;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let expires = Instant::now() + Duration::from_secs(5 * 60);
        self.share_targets.lock().unwrap().insert(
            token.clone(),
            ShareTarget {
                file_id: file.id.clone(),
                account_id: file.account_id.clone(),
                chat_id,
                username: username.clone(),
                display_name: display_name.clone(),
                expires,
            },
        );
        ShareRecipient {
            token,
            username,
            display_name,
            initials,
            kind,
            verified,
            expires_at: (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
        }
    }

    pub fn spawn_share(
        self: &Arc<Self>,
        file_id: &str,
        recipient_token: &str,
        allow_decrypt: bool,
    ) -> AppResult<String> {
        let file = self.catalog.file_context(file_id)?;
        if file.encrypted && !allow_decrypt {
            return Err(AppError::Message(
                "Confirm that TiVault may send a decrypted copy of this encrypted file".into(),
            ));
        }
        let target = self
            .share_targets
            .lock()
            .unwrap()
            .remove(recipient_token)
            .ok_or_else(|| {
                AppError::Message("The recipient confirmation expired; search again".into())
            })?;
        if target.expires <= Instant::now()
            || target.account_id != file.account_id
            || target.file_id != file.id
        {
            return Err(AppError::Message(
                "The recipient confirmation expired; search again".into(),
            ));
        }
        self.spawn_share_to_target(file, target)
    }

    pub fn spawn_folder_share(
        self: &Arc<Self>,
        folder_path: &str,
        recipient_token: &str,
        allow_decrypt: bool,
    ) -> AppResult<Vec<String>> {
        let files = self.folder_share_files(folder_path)?;
        if files.iter().any(|file| file.encrypted) && !allow_decrypt {
            return Err(AppError::Message(
                "Confirm that TiVault may send readable copies of encrypted files in this folder"
                    .into(),
            ));
        }
        let target = self
            .share_targets
            .lock()
            .unwrap()
            .remove(recipient_token)
            .ok_or_else(|| {
                AppError::Message("The recipient confirmation expired; search again".into())
            })?;
        if target.expires <= Instant::now()
            || target.account_id != files[0].account_id
            || target.file_id != files[0].id
        {
            return Err(AppError::Message(
                "The recipient confirmation expired; search again".into(),
            ));
        }
        files
            .into_iter()
            .map(|file| {
                let mut file_target = target.clone();
                file_target.file_id = file.id.clone();
                self.spawn_share_to_target(file, file_target)
            })
            .collect()
    }

    fn spawn_share_to_target(
        self: &Arc<Self>,
        file: FileContext,
        target: ShareTarget,
    ) -> AppResult<String> {
        let recipient_label = if target.username.is_empty() {
            target.display_name.clone()
        } else {
            format!("@{}", target.username)
        };
        let transfer_id = self
            .catalog
            .create_share_transfer(&file, &recipient_label)?;
        let control = Arc::new(AtomicU8::new(RUNNING));
        self.controls
            .lock()
            .unwrap()
            .insert(transfer_id.clone(), Arc::clone(&control));
        let core = Arc::clone(self);
        let spawned_transfer_id = transfer_id.clone();
        let spawned_file_id = file.id.clone();
        tauri::async_runtime::spawn(async move {
            let permits = core.profile_permits();
            let _slot = match core
                .transfer_slots
                .clone()
                .acquire_many_owned(permits)
                .await
            {
                Ok(slot) => slot,
                Err(_) => return,
            };
            if let Err(error) = core
                .process_share(
                    &spawned_file_id,
                    &spawned_transfer_id,
                    &target,
                    Arc::clone(&control),
                )
                .await
            {
                let _ = core
                    .catalog
                    .fail_transfer(&spawned_transfer_id, &error.to_string());
            }
            core.controls.lock().unwrap().remove(&spawned_transfer_id);
        });
        Ok(transfer_id)
    }

    async fn process_share(
        &self,
        file_id: &str,
        transfer_id: &str,
        target: &ShareTarget,
        control: Arc<AtomicU8>,
    ) -> AppResult<()> {
        let file = self.catalog.file_context(file_id)?;
        let work = self.work_dir.join("shares").join(transfer_id);
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&work, fs::Permissions::from_mode(0o700))?;
        }
        let _work_guard = WorkDirectoryGuard::new(work.clone());
        self.wait_control(&control, transfer_id, "preparing")
            .await?;

        let local = self
            .catalog
            .cached_path(file_id)?
            .map(PathBuf::from)
            .filter(|path| {
                path.metadata()
                    .map(|metadata| metadata.is_file() && metadata.len() == file.size)
                    .unwrap_or(false)
            });
        let send_path = if let Some(path) = local {
            self.catalog.update_transfer(
                transfer_id,
                "preparing",
                0.02,
                0,
                0,
                None,
                Some("Verifying the local copy before sharing"),
            )?;
            let verify_path = path.clone();
            let expected = file.original_sha256.clone();
            let verify_catalog = self.catalog.clone();
            let verify_transfer = transfer_id.to_string();
            let verify_control = Arc::clone(&control);
            let total = file.size;
            let actual = tokio::task::spawn_blocking(move || {
                sha256_file_with_progress(&verify_path, &mut |processed, size| {
                    while verify_control.load(Ordering::SeqCst) == PAUSED {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if verify_control.load(Ordering::SeqCst) == CANCELLED {
                        return false;
                    }
                    let ratio = processed as f64 / size.max(1) as f64;
                    let _ = verify_catalog.update_transfer(
                        &verify_transfer,
                        "preparing",
                        0.25 * ratio,
                        (total as f64 * 0.25 * ratio) as u64,
                        0,
                        None,
                        Some("Verifying the local copy before sharing"),
                    );
                    true
                })
            })
            .await
            .map_err(|error| AppError::Message(error.to_string()))??;
            if expected.as_deref() != Some(actual.as_str()) {
                return Err(AppError::Message(
                    "The downloaded copy changed after it was verified; download it again before sharing"
                        .into(),
                ));
            }
            path
        } else {
            let chunks = self.catalog.chunks(file_id)?;
            if chunks.is_empty() {
                return Err(AppError::Message(
                    "No Telegram parts are recorded for this file".into(),
                ));
            }
            let largest_chunk = chunks.iter().map(|chunk| chunk.size).max().unwrap_or(0);
            let required = file
                .size
                .saturating_add(largest_chunk)
                .saturating_add(FREE_SPACE_RESERVE);
            if fs2::available_space(&work)? < required {
                return Err(AppError::Message(format!(
                    "Sharing this file safely requires about {} of temporary free space",
                    human_bytes(required)
                )));
            }
            let safe_name = Path::new(&file.name)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or("TiVault shared file");
            let destination = work.join(safe_name);
            let total_remote = chunks.iter().map(|chunk| chunk.size).sum::<u64>();
            let total_work = total_remote.saturating_add(file.size).max(1);
            let mut downloaded = 0u64;
            let mut assembled = 0u64;
            let mut meter = ProgressMeter::new(0);
            let mut key = if file.encrypted {
                Some(
                    self.master.unwrap_file_key(
                        file.wrapped_key
                            .as_deref()
                            .ok_or_else(|| AppError::Crypto("Missing wrapped file key".into()))?,
                        file.key_nonce
                            .as_deref()
                            .ok_or_else(|| AppError::Crypto("Missing file key nonce".into()))?,
                    )?,
                )
            } else {
                None
            };
            for (position, chunk) in chunks.iter().enumerate() {
                self.wait_control(&control, transfer_id, "downloading")
                    .await?;
                let chunk_path = work.join("incoming.part");
                let base_downloaded = downloaded;
                let base_assembled = assembled;
                let chunk_size = chunk.size;
                let message = format!(
                    "Preparing share: downloading part {} of {}",
                    position + 1,
                    chunks.len()
                );
                self.telegram
                    .download_document(
                        &file.account_id,
                        chunk.message_id,
                        &chunk_path,
                        |received, _| {
                            let network = base_downloaded + received.min(chunk_size);
                            let work_done = network.saturating_add(base_assembled);
                            let ratio = work_done as f64 / total_work as f64;
                            let speed = meter.observe(network);
                            let remaining = total_remote.saturating_sub(network);
                            let eta = (speed > 0).then_some(remaining / speed.max(1));
                            let _ = self.catalog.update_transfer(
                                transfer_id,
                                "downloading",
                                0.65 * ratio,
                                (file.size as f64 * 0.65 * ratio) as u64,
                                speed,
                                eta,
                                Some(&message),
                            );
                            control.load(Ordering::SeqCst) != CANCELLED
                        },
                    )
                    .await?;
                let actual = sha256_file(&chunk_path)?;
                if actual != chunk.sha256 {
                    if let Some(key) = key.as_mut() {
                        key.zeroize();
                    }
                    return Err(AppError::Message(format!(
                        "Integrity check failed for part {}",
                        position + 1
                    )));
                }
                downloaded += chunk.size;
                let append_input = chunk_path.clone();
                let append_output = destination.clone();
                let append_key = key;
                let append_control = Arc::clone(&control);
                let append_catalog = self.catalog.clone();
                let append_transfer = transfer_id.to_string();
                let append_base_downloaded = downloaded;
                let append_base_assembled = assembled;
                let append_total = total_work;
                let append_file_size = file.size;
                let encrypted = file.encrypted;
                let written = tokio::task::spawn_blocking(move || {
                    let mut worker_key = append_key;
                    let result = append_chunk_to_file_with_progress(
                        &append_input,
                        &append_output,
                        encrypted,
                        worker_key.as_ref(),
                        position == 0,
                        |processed| {
                            while append_control.load(Ordering::SeqCst) == PAUSED {
                                std::thread::sleep(Duration::from_millis(100));
                            }
                            if append_control.load(Ordering::SeqCst) == CANCELLED {
                                return false;
                            }
                            let work_done = append_base_downloaded
                                .saturating_add(append_base_assembled)
                                .saturating_add(processed);
                            let ratio = work_done as f64 / append_total as f64;
                            let _ = append_catalog.update_transfer(
                                &append_transfer,
                                "preparing",
                                0.65 * ratio,
                                (append_file_size as f64 * 0.65 * ratio) as u64,
                                0,
                                None,
                                Some("Authenticating and reassembling the share copy"),
                            );
                            true
                        },
                    );
                    if let Some(key) = worker_key.as_mut() {
                        key.zeroize();
                    }
                    result
                })
                .await
                .map_err(|error| AppError::Message(error.to_string()))??;
                assembled += written;
                let _ = fs::remove_file(&chunk_path);
            }
            if let Some(key) = key.as_mut() {
                key.zeroize();
            }
            if assembled != file.size {
                return Err(AppError::Message(
                    "The reconstructed share copy has an unexpected size".into(),
                ));
            }
            let verify_path = destination.clone();
            let expected = file.original_sha256.clone();
            let verify_catalog = self.catalog.clone();
            let verify_transfer = transfer_id.to_string();
            let verify_control = Arc::clone(&control);
            let size = file.size;
            let hash = tokio::task::spawn_blocking(move || {
                sha256_file_with_progress(&verify_path, &mut |processed, total| {
                    if verify_control.load(Ordering::SeqCst) == CANCELLED {
                        return false;
                    }
                    let ratio = processed as f64 / total.max(1) as f64;
                    let _ = verify_catalog.update_transfer(
                        &verify_transfer,
                        "preparing",
                        0.65 + 0.13 * ratio,
                        (size as f64 * (0.65 + 0.13 * ratio)) as u64,
                        0,
                        None,
                        Some("Verifying the reconstructed share copy"),
                    );
                    true
                })
            })
            .await
            .map_err(|error| AppError::Message(error.to_string()))??;
            if expected.as_deref() != Some(hash.as_str()) {
                return Err(AppError::Message(
                    "The reconstructed share copy failed its final integrity check".into(),
                ));
            }
            destination
        };

        self.wait_control(&control, transfer_id, "uploading")
            .await?;
        let mut meter = ProgressMeter::new(0);
        let recipient_label = if target.username.is_empty() {
            target.display_name.clone()
        } else {
            format!("@{}", target.username)
        };
        let caption = if file.folder_path.is_empty() {
            "Sent securely from TiVault".to_string()
        } else {
            format!(
                "Sent securely from TiVault · Folder: {}",
                file.folder_path
            )
        };
        let sent_message_id = self
            .telegram
            .send_document_to_chat(
                &file.account_id,
                target.chat_id,
                &send_path,
                &caption,
                |sent, total| {
                    let ratio = sent as f64 / total.max(1) as f64;
                    let speed = meter.observe(sent);
                    let remaining = total.saturating_sub(sent);
                    let eta = (speed > 0).then_some(remaining / speed.max(1));
                    let message = format!("Sending to {recipient_label}");
                    let _ = self.catalog.update_transfer(
                        transfer_id,
                        "uploading",
                        0.78 + 0.22 * ratio,
                        (file.size as f64 * (0.78 + 0.22 * ratio)) as u64,
                        speed,
                        eta,
                        Some(&message),
                    );
                    control.load(Ordering::SeqCst) != CANCELLED
                },
            )
            .await?;
        if control.load(Ordering::SeqCst) == CANCELLED {
            let _ = self
                .telegram
                .delete_chat_messages(&file.account_id, target.chat_id, &[sent_message_id])
                .await;
            return Err(AppError::Message("Transfer cancelled".into()));
        }
        self.catalog.update_transfer(
            transfer_id,
            "complete",
            1.0,
            file.size,
            0,
            Some(0),
            Some(&format!(
                "Sent to {}{}",
                recipient_label,
                if file.encrypted {
                    " as a decrypted copy"
                } else {
                    ""
                }
            )),
        )?;
        Ok(())
    }

    pub fn spawn_upload(self: &Arc<Self>, file_id: String, transfer_id: String) {
        let control = Arc::new(AtomicU8::new(RUNNING));
        self.controls
            .lock()
            .unwrap()
            .insert(transfer_id.clone(), Arc::clone(&control));
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let permits = core.profile_permits();
            let _slot = match core
                .transfer_slots
                .clone()
                .acquire_many_owned(permits)
                .await
            {
                Ok(slot) => slot,
                Err(_) => return,
            };
            let retries = core.automatic_retry_count();
            let mut attempt = 0u64;
            loop {
                match core
                    .process_upload(&file_id, &transfer_id, Arc::clone(&control))
                    .await
                {
                    Ok(()) => break,
                    Err(error)
                        if attempt < retries
                            && is_retryable_transfer_error(&error)
                            && control.load(Ordering::SeqCst) != CANCELLED =>
                    {
                        attempt += 1;
                        let delay = retry_delay_seconds(&error, attempt);
                        if core
                            .wait_before_retry(&transfer_id, &control, attempt, retries, delay)
                            .await
                            .is_err()
                        {
                            let _ = core
                                .catalog
                                .fail_transfer(&transfer_id, "Transfer cancelled");
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = core.catalog.fail_transfer(&transfer_id, &error.to_string());
                        break;
                    }
                }
            }
            core.controls.lock().unwrap().remove(&transfer_id);
        });
    }

    async fn process_upload(
        &self,
        file_id: &str,
        transfer_id: &str,
        control: Arc<AtomicU8>,
    ) -> AppResult<()> {
        let file = self.catalog.file_context(file_id)?;
        let source = PathBuf::from(
            file.source_path
                .as_ref()
                .ok_or_else(|| AppError::Message("The local source file is unavailable".into()))?,
        );
        self.catalog.update_transfer(
            transfer_id,
            "preparing",
            0.01,
            0,
            0,
            None,
            Some("Hashing and preparing chunks"),
        )?;
        self.wait_control(&control, transfer_id, "preparing")
            .await?;
        if file.duplicate_policy == "skip"
            && self
                .catalog
                .has_duplicate_size(&file.account_id, file.size, file_id)?
        {
            let check_path = source.clone();
            let check_catalog = self.catalog.clone();
            let check_transfer_id = transfer_id.to_string();
            let check_control = Arc::clone(&control);
            let hash = tokio::task::spawn_blocking(move || {
                let mut last_report = Instant::now() - Duration::from_secs(1);
                sha256_file_with_progress(&check_path, &mut |processed, total| {
                    if last_report.elapsed() >= Duration::from_millis(250) || processed >= total {
                        let ratio = processed as f64 / total.max(1) as f64;
                        let _ = check_catalog.update_transfer(
                            &check_transfer_id,
                            "preparing",
                            PREPARATION_FRACTION * ratio,
                            0,
                            0,
                            None,
                            Some(&format!("Checking for duplicates: {:.0}%", ratio * 100.0)),
                        );
                        last_report = Instant::now();
                    }
                    check_control.load(Ordering::SeqCst) != CANCELLED
                })
            })
            .await
            .map_err(|error| {
                AppError::Message(format!("Duplicate detection stopped: {error}"))
            })??;
            if let Some(existing_name) =
                self.catalog
                    .duplicate_by_hash(&file.account_id, file.size, &hash, file_id)?
            {
                self.catalog
                    .mark_duplicate_skipped(file_id, transfer_id, &existing_name)?;
                return Ok(());
            }
        }
        let work = self.work_dir.join("uploads").join(file_id);
        let _ = fs::remove_dir_all(&work);
        let _work_guard = WorkDirectoryGuard::new(work.clone());
        let master = self.master.clone();
        let source_clone = source.clone();
        let work_clone = work.clone();
        let encrypted = file.encrypted;
        let prep_catalog = self.catalog.clone();
        let prep_transfer_id = transfer_id.to_string();
        let prep_control = Arc::clone(&control);
        let producer_abort = Arc::new(AtomicBool::new(false));
        let prep_abort = Arc::clone(&producer_abort);
        let network_started = Arc::new(AtomicBool::new(false));
        let prep_network_started = Arc::clone(&network_started);
        let upload_bytes = file.size.max(1);
        let mut sent = 0u64;
        let mut chunk_records = Vec::new();
        let existing = self
            .catalog
            .chunks(file_id)?
            .into_iter()
            .map(|chunk| (chunk.index, chunk))
            .collect::<HashMap<_, _>>();
        let total_parts = estimate_chunks(file.size);
        let mut consumer_error = None;
        let (chunk_tx, mut chunk_rx) =
            tokio::sync::mpsc::channel::<(PreparedChunk, std::sync::mpsc::Sender<()>)>(1);
        let producer = tokio::task::spawn_blocking(move || {
            let mut last_report = Instant::now() - Duration::from_secs(1);
            prepare_upload_streaming_with_progress(
                &source_clone,
                &work_clone,
                encrypted,
                &master,
                |processed, total| {
                    while prep_control.load(Ordering::SeqCst) == PAUSED {
                        if prep_abort.load(Ordering::SeqCst) {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if prep_control.load(Ordering::SeqCst) == CANCELLED
                        || prep_abort.load(Ordering::SeqCst)
                    {
                        return false;
                    }
                    if !prep_network_started.load(Ordering::SeqCst)
                        && (last_report.elapsed() >= Duration::from_millis(250)
                            || processed >= total)
                    {
                        let ratio = processed as f64 / total.max(1) as f64;
                        let message = format!("Preparing chunks: {:.0}%", ratio * 100.0);
                        let _ = prep_catalog.update_transfer(
                            &prep_transfer_id,
                            "preparing",
                            PREPARATION_FRACTION * ratio,
                            0,
                            0,
                            None,
                            Some(&message),
                        );
                        last_report = Instant::now();
                    }
                    true
                },
                |chunk| {
                    let (done_tx, done_rx) = std::sync::mpsc::channel();
                    if prep_control.load(Ordering::SeqCst) == CANCELLED
                        || prep_abort.load(Ordering::SeqCst)
                        || chunk_tx.blocking_send((chunk, done_tx)).is_err()
                    {
                        return false;
                    }
                    done_rx.recv().is_ok()
                        && prep_control.load(Ordering::SeqCst) != CANCELLED
                        && !prep_abort.load(Ordering::SeqCst)
                },
            )
        });

        // The producer waits for each part to be uploaded and removed before creating
        // the next one, bounding temporary disk usage to a single sub-1 GB part.
        while let Some((chunk, chunk_done)) = chunk_rx.recv().await {
            network_started.store(true, Ordering::SeqCst);
            let chunk_result: AppResult<()> = async {
                self.wait_control(&control, transfer_id, "uploading")
                    .await?;
                if let Some(record) = existing
                    .get(&chunk.index)
                    .filter(|record| record.sha256 == chunk.sha256)
                {
                    sent += chunk.size;
                    chunk_records.push(record.clone());
                    self.catalog.update_transfer(
                        transfer_id,
                        "uploading",
                        PREPARATION_FRACTION
                            + NETWORK_FRACTION * sent.min(upload_bytes) as f64
                                / upload_bytes as f64,
                        proportional(sent, upload_bytes, file.size),
                        0,
                        None,
                        Some("Reused a previously confirmed Telegram part"),
                    )?;
                    return Ok(());
                }

                let caption = format!(
                    "#TiVaultChunk v1 file={} part={}/{} encrypted={}",
                    file_id,
                    chunk.index + 1,
                    total_parts,
                    file.encrypted
                );
                self.catalog.update_transfer(
                    transfer_id,
                    "uploading",
                    PREPARATION_FRACTION
                        + NETWORK_FRACTION * sent.min(upload_bytes) as f64 / upload_bytes as f64,
                    proportional(sent, upload_bytes, file.size),
                    0,
                    None,
                    Some(&format!(
                        "Uploading part {} of {}",
                        chunk.index + 1,
                        total_parts
                    )),
                )?;

                let base_sent = sent;
                let chunk_size = chunk.size;
                let progress_catalog = self.catalog.clone();
                let progress_transfer_id = transfer_id.to_string();
                let progress_control = Arc::clone(&control);
                let progress_message =
                    format!("Uploading part {} of {}", chunk.index + 1, total_parts);
                let mut meter = ProgressMeter::new(base_sent);
                let message_id = self
                    .telegram
                    .upload_document(
                        &file.account_id,
                        &chunk.path,
                        &caption,
                        move |uploaded, _| {
                            let current = base_sent + uploaded.min(chunk_size);
                            let speed = meter.observe(current);
                            let remaining = upload_bytes.saturating_sub(current);
                            let eta = (speed > 0).then_some(remaining / speed.max(1));
                            let _ = progress_catalog.update_transfer(
                                &progress_transfer_id,
                                "uploading",
                                PREPARATION_FRACTION
                                    + NETWORK_FRACTION * current.min(upload_bytes) as f64
                                        / upload_bytes as f64,
                                proportional(current, upload_bytes, file.size),
                                speed,
                                eta,
                                Some(&progress_message),
                            );
                            progress_control.load(Ordering::SeqCst) != CANCELLED
                        },
                    )
                    .await?;
                sent += chunk.size;
                self.catalog.update_transfer(
                    transfer_id,
                    "uploading",
                    PREPARATION_FRACTION
                        + NETWORK_FRACTION * sent.min(upload_bytes) as f64 / upload_bytes as f64,
                    proportional(sent, upload_bytes, file.size),
                    0,
                    None,
                    Some("Telegram confirmed this part"),
                )?;
                let record = ChunkRecord {
                    index: chunk.index,
                    message_id,
                    size: chunk.size,
                    sha256: chunk.sha256.clone(),
                };
                self.catalog.add_chunk(file_id, &record)?;
                chunk_records.push(record);
                Ok(())
            }
            .await;

            let cleanup_result = if chunk.temporary {
                fs::remove_file(&chunk.path).map_err(AppError::from)
            } else {
                Ok(())
            };
            let _ = chunk_done.send(());
            if let Err(error) = chunk_result.and(cleanup_result) {
                producer_abort.store(true, Ordering::SeqCst);
                consumer_error = Some(error);
                break;
            }
        }
        drop(chunk_rx);

        let producer_result = producer
            .await
            .map_err(|e| AppError::Message(format!("Chunk preparation stopped: {e}")))?;
        if let Some(error) = consumer_error {
            let _ = producer_result;
            return Err(error);
        }
        let prepared = producer_result?;
        self.wait_control(&control, transfer_id, "uploading")
            .await?;
        self.catalog.set_prepared(
            file_id,
            &prepared.original_sha256,
            chunk_records.len() as u32,
            prepared.wrapped_key.as_deref(),
            prepared.key_nonce.as_deref(),
        )?;
        self.wait_control(&control, transfer_id, "uploading")
            .await?;
        let prepared_file = self.catalog.file_context(file_id)?;
        let manifest = self.build_manifest(&prepared_file, chunk_records)?;
        let manifest_id = self
            .upload_manifest_document(&file.account_id, &manifest)
            .await?;
        self.catalog
            .set_manifest_and_complete(file_id, transfer_id, manifest_id)?;
        Ok(())
    }

    pub fn spawn_download(self: &Arc<Self>, file_id: String) -> AppResult<()> {
        let file = self.catalog.file_context(&file_id)?;
        if file.status != "ready" {
            return Err(AppError::Message(
                "Only stored files can be downloaded".into(),
            ));
        }
        self.catalog.touch_file(&file_id)?;
        let transfer_id = self.catalog.create_download_transfer(&file)?;
        self.spawn_download_transfer(file_id, transfer_id);
        Ok(())
    }

    pub fn spawn_folder_download(self: &Arc<Self>, path: &str) -> AppResult<usize> {
        let path = normalize_vault_path(path)?;
        if path.is_empty() || !self.catalog.vault_folder_exists(&path)? {
            return Err(AppError::Message("This folder no longer exists".into()));
        }

        let destination_root = dirs::download_dir()
            .unwrap_or_else(|| self.work_dir.join("completed"))
            .join("TiVault");
        let mut folder_paths = self.catalog.folder_paths_in_tree(&path)?;
        if !folder_paths.iter().any(|folder| folder == &path) {
            folder_paths.insert(0, path.clone());
        }
        for folder in folder_paths {
            fs::create_dir_all(safe_folder_destination(&destination_root, &folder))?;
        }

        let file_ids = self.catalog.ready_file_ids_in_folder(&path)?;
        for file_id in &file_ids {
            self.spawn_download(file_id.clone())?;
        }
        Ok(file_ids.len())
    }

    fn spawn_download_transfer(self: &Arc<Self>, file_id: String, transfer_id: String) {
        let control = Arc::new(AtomicU8::new(RUNNING));
        self.controls
            .lock()
            .unwrap()
            .insert(transfer_id.clone(), Arc::clone(&control));
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let permits = core.profile_permits();
            let _slot = match core
                .transfer_slots
                .clone()
                .acquire_many_owned(permits)
                .await
            {
                Ok(slot) => slot,
                Err(_) => return,
            };
            let retries = core.automatic_retry_count();
            let mut attempt = 0u64;
            loop {
                match core
                    .process_download(&file_id, &transfer_id, Arc::clone(&control))
                    .await
                {
                    Ok(()) => break,
                    Err(error)
                        if attempt < retries
                            && is_retryable_transfer_error(&error)
                            && control.load(Ordering::SeqCst) != CANCELLED =>
                    {
                        attempt += 1;
                        let delay = retry_delay_seconds(&error, attempt);
                        if core
                            .wait_before_retry(&transfer_id, &control, attempt, retries, delay)
                            .await
                            .is_err()
                        {
                            let _ = core
                                .catalog
                                .fail_transfer(&transfer_id, "Transfer cancelled");
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = core.catalog.fail_transfer(&transfer_id, &error.to_string());
                        break;
                    }
                }
            }
            core.controls.lock().unwrap().remove(&transfer_id);
        });
    }

    async fn process_download(
        &self,
        file_id: &str,
        transfer_id: &str,
        control: Arc<AtomicU8>,
    ) -> AppResult<()> {
        let file = self.catalog.file_context(file_id)?;
        let chunks = self.catalog.chunks(file_id)?;
        if chunks.is_empty() {
            return Err(AppError::Message(
                "No Telegram chunks are recorded for this file".into(),
            ));
        }
        let work = self.work_dir.join("downloads").join(file_id);
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work)?;
        let total_remote: u64 = chunks.iter().map(|x| x.size).sum();
        let mut received = 0u64;
        let mut paths = Vec::new();
        let mut meter = ProgressMeter::new(0);
        for chunk in &chunks {
            self.wait_control(&control, transfer_id, "downloading")
                .await?;
            let path = work.join(format!("part-{:06}.download", chunk.index));
            let progress_message =
                format!("Downloading part {} of {}", chunk.index + 1, chunks.len());
            self.catalog.update_transfer(
                transfer_id,
                "downloading",
                DOWNLOAD_NETWORK_FRACTION * received as f64 / total_remote.max(1) as f64,
                proportional(received, total_remote, file.size),
                0,
                None,
                Some(&progress_message),
            )?;
            let base_received = received;
            let chunk_size = chunk.size;
            self.telegram
                .download_document(
                    &file.account_id,
                    chunk.message_id,
                    &path,
                    |downloaded, _| {
                        let current = base_received + downloaded.min(chunk_size);
                        let speed = meter.observe(current);
                        let remaining = total_remote.saturating_sub(current);
                        let eta = (speed > 0).then_some(remaining / speed.max(1));
                        let _ = self.catalog.update_transfer(
                            transfer_id,
                            "downloading",
                            DOWNLOAD_NETWORK_FRACTION * current as f64 / total_remote.max(1) as f64,
                            proportional(current, total_remote, file.size),
                            speed,
                            eta,
                            Some(&progress_message),
                        );
                        control.load(Ordering::SeqCst) != CANCELLED
                    },
                )
                .await?;
            let check_path = path.clone();
            let expected = chunk.sha256.clone();
            let actual = tokio::task::spawn_blocking(move || sha256_file(&check_path))
                .await
                .map_err(|e| AppError::Message(e.to_string()))??;
            if actual != expected {
                return Err(AppError::Message(format!(
                    "Integrity check failed for part {}",
                    chunk.index + 1
                )));
            }
            received += chunk.size;
            self.catalog.update_transfer(
                transfer_id,
                "downloading",
                DOWNLOAD_NETWORK_FRACTION * received as f64 / total_remote.max(1) as f64,
                proportional(received, total_remote, file.size),
                meter.observe(received),
                None,
                Some("Chunk integrity verified"),
            )?;
            paths.push(path);
        }
        let destination_dir = dirs::download_dir()
            .unwrap_or_else(|| self.work_dir.join("completed"))
            .join("TiVault");
        let destination_dir = safe_folder_destination(&destination_dir, &file.folder_path);
        fs::create_dir_all(&destination_dir)?;
        let destination = unique_destination(&destination_dir, &file.name);
        let key = if file.encrypted {
            Some(
                self.master.unwrap_file_key(
                    file.wrapped_key
                        .as_deref()
                        .ok_or_else(|| AppError::Crypto("Missing wrapped file key".into()))?,
                    file.key_nonce
                        .as_deref()
                        .ok_or_else(|| AppError::Crypto("Missing file key nonce".into()))?,
                )?,
            )
        } else {
            None
        };
        self.catalog.update_transfer(
            transfer_id,
            "preparing",
            DOWNLOAD_NETWORK_FRACTION,
            file.size,
            0,
            None,
            Some("Reassembling and verifying the file"),
        )?;
        let output_clone = destination.clone();
        let paths_clone = paths.clone();
        let encrypted = file.encrypted;
        let assembly_catalog = self.catalog.clone();
        let assembly_transfer_id = transfer_id.to_string();
        let assembly_control = Arc::clone(&control);
        let assembly_total = file.size;
        let hash = tokio::task::spawn_blocking(move || {
            let mut last_report = Instant::now() - Duration::from_secs(1);
            assemble_chunks_with_progress(
                &paths_clone,
                &output_clone,
                encrypted,
                key,
                assembly_total,
                true,
                |processed, total| {
                    while assembly_control.load(Ordering::SeqCst) == PAUSED {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if assembly_control.load(Ordering::SeqCst) == CANCELLED {
                        return false;
                    }
                    if last_report.elapsed() >= Duration::from_millis(250) || processed >= total {
                        let ratio = processed as f64 / total.max(1) as f64;
                        let message = format!("Reassembling file: {:.0}%", ratio * 100.0);
                        let _ = assembly_catalog.update_transfer(
                            &assembly_transfer_id,
                            "preparing",
                            DOWNLOAD_NETWORK_FRACTION + (1.0 - DOWNLOAD_NETWORK_FRACTION) * ratio,
                            assembly_total,
                            0,
                            None,
                            Some(&message),
                        );
                        last_report = Instant::now();
                    }
                    true
                },
            )
        })
        .await
        .map_err(|e| AppError::Message(e.to_string()))??;
        if file.original_sha256.as_deref() != Some(hash.as_str()) {
            let _ = fs::remove_file(&destination);
            return Err(AppError::Message(
                "Final integrity verification failed".into(),
            ));
        }
        self.catalog
            .set_cached(file_id, &destination.to_string_lossy())?;
        self.catalog.update_transfer(
            transfer_id,
            "complete",
            1.0,
            file.size,
            0,
            Some(0),
            Some("Saved to Downloads/TiVault"),
        )?;
        let _ = fs::remove_dir_all(work);
        Ok(())
    }

    async fn wait_control(
        &self,
        control: &AtomicU8,
        transfer_id: &str,
        resume_state: &str,
    ) -> AppResult<()> {
        loop {
            match control.load(Ordering::SeqCst) {
                RUNNING => {
                    self.catalog
                        .set_transfer_state(transfer_id, resume_state, None)?;
                    return Ok(());
                }
                PAUSED => sleep(Duration::from_millis(250)).await,
                CANCELLED => return Err(AppError::Message("Transfer cancelled".into())),
                _ => return Err(AppError::Message("Transfer control is invalid".into())),
            }
        }
    }

    pub fn set_control(&self, transfer_id: &str, state: u8) -> bool {
        if let Some(control) = self.controls.lock().unwrap().get(transfer_id) {
            control.store(state, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub fn pause(&self, transfer_id: &str) -> AppResult<()> {
        self.set_control(transfer_id, PAUSED);
        self.catalog
            .set_transfer_state(transfer_id, "paused", Some("Paused by you"))
    }
    pub fn cancel(self: &Arc<Self>, transfer_id: &str) -> AppResult<()> {
        let file_id = self.catalog.transfer_file_id(transfer_id)?;
        let direction = self.catalog.transfer_direction(transfer_id)?;
        let state = self.catalog.transfer_state(transfer_id)?;
        if state == "complete" {
            return self.catalog.remove_transfer(transfer_id);
        }

        self.set_control(transfer_id, CANCELLED);
        self.catalog.remove_transfer(transfer_id)?;
        if direction == "upload" {
            self.catalog.set_file_status(&file_id, "cancelled")?;
        }

        let core = Arc::clone(self);
        let transfer_id = transfer_id.to_string();
        tauri::async_runtime::spawn(async move {
            // The worker owns any open chunk file. Wait until it has observed
            // cancellation and exited before deleting its work directory.
            while core.controls.lock().unwrap().contains_key(&transfer_id) {
                sleep(Duration::from_millis(100)).await;
            }

            let work_path = match direction.as_str() {
                "upload" => core.work_dir.join("uploads").join(&file_id),
                "share" => core.work_dir.join("shares").join(&transfer_id),
                _ => core.work_dir.join("downloads").join(&file_id),
            };
            let _ = fs::remove_dir_all(work_path);
            if direction != "upload" {
                return;
            }

            let Ok(file) = core.catalog.file_context(&file_id) else {
                return;
            };
            let ids = core
                .catalog
                .message_ids_for_delete(&file_id)
                .unwrap_or_default();
            if ids.is_empty()
                || core
                    .telegram
                    .delete_messages(&file.account_id, &ids)
                    .await
                    .is_ok()
            {
                let _ = core.catalog.permanent_delete_local(&file_id);
            }
        });
        Ok(())
    }

    pub fn dismiss_transfer_history(self: &Arc<Self>, transfer_ids: &[String]) -> AppResult<usize> {
        let mut removed = 0;
        let mut seen = HashSet::new();
        for transfer_id in transfer_ids {
            if !seen.insert(transfer_id.as_str()) {
                continue;
            }
            match self.catalog.transfer_state(transfer_id)?.as_str() {
                "complete" => self.catalog.remove_transfer(transfer_id)?,
                "failed" => self.cancel(transfer_id)?,
                _ => {
                    return Err(AppError::Message(
                        "Only completed or failed transfers can be removed from history".into(),
                    ))
                }
            }
            removed += 1;
        }
        Ok(removed)
    }

    pub fn clear_transfer_history(self: &Arc<Self>) -> AppResult<usize> {
        let ids = self.catalog.history_transfer_ids()?;
        self.dismiss_transfer_history(&ids)
    }

    pub fn resume(self: &Arc<Self>, transfer_id: &str) -> AppResult<()> {
        if self.set_control(transfer_id, RUNNING) {
            self.catalog
                .set_transfer_state(transfer_id, "queued", Some("Resuming"))?;
            return Ok(());
        }
        let file_id = self.catalog.transfer_file_id(transfer_id)?;
        let direction = self.catalog.transfer_direction(transfer_id)?;
        if direction == "share" {
            return Err(AppError::Message(
                "A failed share must be started again so the recipient can be reconfirmed".into(),
            ));
        }
        if direction == "upload" {
            self.spawn_upload(file_id, transfer_id.to_string());
        } else {
            self.catalog
                .set_transfer_state(transfer_id, "queued", Some("Resuming"))?;
            self.spawn_download_transfer(file_id, transfer_id.to_string());
        }
        Ok(())
    }

    pub async fn move_to_trash(&self, file_id: &str) -> AppResult<()> {
        if self.catalog.active_transfer_count_for_file(file_id)? > 0 {
            return Err(AppError::Message(
                "Cancel this file's active transfers before deleting it".into(),
            ));
        }
        let preview_tokens = {
            let previews = self.previews.lock().unwrap();
            previews
                .iter()
                .filter_map(|(token, session)| {
                    (session.file.id == file_id).then_some(token.clone())
                })
                .collect::<Vec<_>>()
        };
        for token in preview_tokens {
            let _ = self.stop_preview(&token).await;
        }
        let retention_days = self
            .catalog
            .setting("recycle_retention_days")?
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(30)
            .clamp(7, 30);
        if let Some(cached_path) = self.catalog.trash_file(file_id, retention_days)? {
            let path = PathBuf::from(cached_path);
            if path.starts_with(&self.work_dir) {
                let _ = fs::remove_file(path);
            }
        }
        let _ = fs::remove_dir_all(self.work_dir.join("uploads").join(file_id));
        let _ = fs::remove_dir_all(self.work_dir.join("downloads").join(file_id));
        Ok(())
    }

    pub async fn move_many_to_trash(&self, file_ids: &[String]) -> AppResult<usize> {
        let mut deleted = 0;
        let mut seen = HashSet::new();
        for file_id in file_ids {
            if seen.insert(file_id.as_str()) {
                self.move_to_trash(file_id).await?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    pub async fn move_folder_to_trash(&self, path: &str) -> AppResult<usize> {
        let path = normalize_vault_path(path)?;
        if path.is_empty() {
            return Err(AppError::Message("The vault root cannot be deleted".into()));
        }
        if self.catalog.active_file_count_in_folder(&path)? > 0 {
            return Err(AppError::Message(
                "Wait for or cancel the folder's active transfers before deleting it".into(),
            ));
        }
        let file_ids = self.catalog.ready_file_ids_in_folder(&path)?;
        let deleted = self.move_many_to_trash(&file_ids).await?;
        self.catalog.delete_vault_folder_tree(&path)?;
        Ok(deleted)
    }

    pub fn restore_from_trash(&self, file_id: &str) -> AppResult<()> {
        self.catalog.restore_file(file_id)
    }

    pub async fn permanently_delete(&self, file_id: &str) -> AppResult<()> {
        let file = self.catalog.file_context(file_id)?;
        if file.status != "trashed" {
            return Err(AppError::Message(
                "Move the file to the Recycle Bin before deleting it permanently".into(),
            ));
        }
        let ids = self.catalog.message_ids_for_delete(file_id)?;
        if !ids.is_empty() {
            self.telegram
                .delete_messages(&file.account_id, &ids)
                .await?;
        }
        let _ = fs::remove_dir_all(self.work_dir.join("uploads").join(file_id));
        let _ = fs::remove_dir_all(self.work_dir.join("downloads").join(file_id));
        self.catalog.permanent_delete_local(file_id)
    }

    pub async fn permanently_delete_many(&self, file_ids: &[String]) -> AppResult<usize> {
        let mut deleted = 0;
        let mut seen = HashSet::new();
        for file_id in file_ids {
            if seen.insert(file_id.as_str()) {
                self.permanently_delete(file_id).await?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    pub async fn empty_trash(&self) -> AppResult<usize> {
        let ids = self.catalog.trashed_file_ids()?;
        self.permanently_delete_many(&ids).await
    }

    pub async fn trash_cleanup_loop(self: Arc<Self>) {
        sleep(Duration::from_secs(20)).await;
        loop {
            if !self.locked.load(Ordering::SeqCst) {
                if let Ok(ids) = self.catalog.expired_trash_ids() {
                    for id in ids {
                        let _ = self.permanently_delete(&id).await;
                    }
                }
            }
            sleep(Duration::from_secs(6 * 60 * 60)).await;
        }
    }

    pub async fn disconnect_account(&self, account_id: &str) -> AppResult<()> {
        if self.catalog.active_transfer_count_for_account(account_id)? > 0 {
            return Err(AppError::Message(
                "Pause or cancel this account's active transfers before disconnecting".into(),
            ));
        }
        self.telegram.disconnect_account(account_id).await
    }

    pub async fn remove_account(&self, account_id: &str) -> AppResult<()> {
        if self.catalog.active_transfer_count_for_account(account_id)? > 0 {
            return Err(AppError::Message(
                "Cancel this account's active transfers before removing it".into(),
            ));
        }
        let session_path = self.telegram.log_out_account(account_id).await?;
        if session_path.exists() {
            fs::remove_dir_all(&session_path)?;
        }
        let account_key = hex::encode(sha2::Sha256::digest(account_id.as_bytes()));
        let _ = fs::remove_dir_all(self.work_dir.join("avatars").join(account_key));
        self.telegram.forget_account_credentials(account_id)?;
        self.catalog.remove_account_local(account_id)
    }

    fn profile_permits(&self) -> u32 {
        match self
            .catalog
            .setting("speed_profile")
            .ok()
            .flatten()
            .as_deref()
        {
            Some("low-impact") => 4,
            Some("maximum") => 1,
            _ => 2,
        }
    }

    pub async fn watch_loop(self: Arc<Self>) {
        let mut stable: HashMap<String, (u64, i64, u8)> = HashMap::new();
        loop {
            if let Ok(watches) = self.catalog.watch_folders() {
                for watch in watches.into_iter().filter(|w| w.enabled) {
                    for entry in WalkDir::new(&watch.path)
                        .follow_links(false)
                        .into_iter()
                        .filter_map(Result::ok)
                        .filter(|e| e.file_type().is_file())
                    {
                        let path = entry.path();
                        if ignored_watch_file(path) {
                            continue;
                        }
                        let Ok(meta) = entry.metadata() else { continue };
                        let modified = meta
                            .modified()
                            .ok()
                            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        let key = format!("{}:{}", watch.id, path.display());
                        let item = stable
                            .entry(key.clone())
                            .or_insert((meta.len(), modified, 0));
                        if item.0 == meta.len() && item.1 == modified {
                            item.2 = item.2.saturating_add(1);
                        } else {
                            *item = (meta.len(), modified, 0);
                        }
                        if item.2 >= 2
                            && !self
                                .catalog
                                .watch_seen(
                                    &watch.id,
                                    &path.to_string_lossy(),
                                    meta.len(),
                                    modified,
                                )
                                .unwrap_or(true)
                        {
                            let options = UploadOptions {
                                paths: vec![path.to_string_lossy().to_string()],
                                folder_root: Some(watch.path.clone()),
                                destination_folder: None,
                                encrypt: watch.encrypt,
                                account_id: watch.account_id.clone(),
                                duplicate_policy: "skip".into(),
                            };
                            if self.queue_paths(options).await.is_ok() {
                                let _ = self.catalog.mark_watch_seen(
                                    &watch.id,
                                    &path.to_string_lossy(),
                                    meta.len(),
                                    modified,
                                );
                            }
                            stable.remove(&key);
                        }
                    }
                }
            }
            sleep(Duration::from_secs(8)).await;
        }
    }

    fn automatic_retry_count(&self) -> u64 {
        self.catalog
            .setting("automatic_retry_count")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(3)
            .clamp(0, 5)
    }

    async fn wait_before_retry(
        &self,
        transfer_id: &str,
        control: &AtomicU8,
        attempt: u64,
        total_retries: u64,
        delay_seconds: u64,
    ) -> AppResult<()> {
        let mut remaining = delay_seconds.max(1);
        while remaining > 0 {
            if control.load(Ordering::SeqCst) == CANCELLED {
                return Err(AppError::Message("Transfer cancelled".into()));
            }
            if control.load(Ordering::SeqCst) == PAUSED {
                self.catalog.set_transfer_state(
                    transfer_id,
                    "paused",
                    Some("Paused before automatic retry"),
                )?;
                while control.load(Ordering::SeqCst) == PAUSED {
                    sleep(Duration::from_millis(250)).await;
                }
                continue;
            }
            self.catalog.set_transfer_state(
                transfer_id,
                "waiting",
                Some(&format!(
                    "Automatic retry {attempt} of {total_retries} in {remaining}s"
                )),
            )?;
            for _ in 0..4 {
                sleep(Duration::from_millis(250)).await;
                if control.load(Ordering::SeqCst) != RUNNING {
                    break;
                }
            }
            if control.load(Ordering::SeqCst) == RUNNING {
                remaining -= 1;
            }
        }
        self.catalog
            .set_transfer_state(transfer_id, "queued", Some("Retrying automatically"))
    }
}

fn is_retryable_transfer_error(error: &AppError) -> bool {
    match error {
        AppError::Telegram(message) => {
            let upper = message.to_ascii_uppercase();
            !upper.contains("CODE 400")
                && !upper.contains("UNAUTHORIZED")
                && !upper.contains("AUTH_KEY")
                && !upper.contains("CHAT NOT FOUND")
        }
        AppError::Io(_) => true,
        _ => false,
    }
}

fn retry_delay_seconds(error: &AppError, attempt: u64) -> u64 {
    if let AppError::Telegram(message) = error {
        let upper = message.to_ascii_uppercase();
        if let Some(start) = upper.find("FLOOD_WAIT") {
            let digits = upper[start..]
                .chars()
                .skip_while(|character| !character.is_ascii_digit())
                .take_while(|character| character.is_ascii_digit())
                .collect::<String>();
            if let Ok(seconds) = digits.parse::<u64>() {
                return seconds.clamp(1, 60 * 60);
            }
        }
    }
    2u64.saturating_pow(attempt.min(6) as u32).clamp(2, 60)
}

fn preview_kind(file: &FileContext) -> (String, Option<String>) {
    let extension = Path::new(&file.name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if file.mime_type.starts_with("image/") {
        return ("image".into(), None);
    }
    if file.mime_type.starts_with("video/") {
        return (
            "video".into(),
            Some(
                "Playback is streamed in authenticated 8 MB blocks with a bounded local cache."
                    .into(),
            ),
        );
    }
    if file.mime_type.starts_with("audio/") {
        return ("audio".into(), None);
    }
    if file.mime_type == "application/pdf" || extension == "pdf" {
        return ("pdf".into(), None);
    }
    if file.mime_type.starts_with("text/")
        || matches!(
            extension.as_str(),
            "txt"
                | "md"
                | "csv"
                | "json"
                | "xml"
                | "yaml"
                | "yml"
                | "log"
                | "rs"
                | "js"
                | "ts"
                | "tsx"
                | "css"
                | "html"
        )
    {
        return ("text".into(), None);
    }
    if matches!(extension.as_str(), "doc" | "docx" | "rtf" | "rtfd" | "odt") {
        return (
            "document".into(),
            Some("TiVault extracts plain text locally with macOS; document scripts and macros are never executed.".into()),
        );
    }
    let message = if matches!(extension.as_str(), "xls" | "xlsx" | "ppt" | "pptx") {
        "Spreadsheet and presentation previews are not rendered inline because doing so safely would require executing a full office-document engine. Download to open them in your trusted system application."
    } else {
        "This file type has no safe inline preview. Download it to open it with a trusted application."
    };
    ("unsupported".into(), Some(message.into()))
}

fn preview_info(session: &PreviewSession) -> PreviewInfo {
    PreviewInfo {
        token: session.token.clone(),
        url: format!(
            "http://127.0.0.1:7468/api/preview/{}/content",
            session.token
        ),
        kind: session.kind.clone(),
        mime_type: session.file.mime_type.clone(),
        size: session.file.size,
        cache_limit: session.cache_limit,
        expires_at: (session.created_at + chrono::Duration::from_std(session.ttl).unwrap())
            .to_rfc3339(),
        message: session.message.clone(),
    }
}

fn directory_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

fn cached_avatar(directory: &Path) -> Option<(i64, Vec<u8>)> {
    let entry = fs::read_dir(directory).ok()?.flatten().find(|entry| {
        entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            == Some("avatar")
    })?;
    let id = entry
        .path()
        .file_stem()
        .and_then(|stem| stem.to_str())?
        .parse::<i64>()
        .ok()?;
    let bytes = fs::read(entry.path()).ok()?;
    (bytes.len() <= 5 * 1024 * 1024 && raster_image_mime(&bytes).is_some()).then_some((id, bytes))
}

fn raster_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn category_for(name: &str) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "avif" | "svg" => "Photos",
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" => "Videos",
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" => "Audio",
        "pdf" | "doc" | "docx" | "txt" | "md" | "rtf" | "xls" | "xlsx" | "ppt" | "pptx" | "csv" => {
            "Documents"
        }
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" => "Archives",
        "exe" | "msi" | "dmg" | "pkg" | "appimage" | "deb" | "rpm" | "apk" => "Applications",
        _ => "Other",
    }
}

fn is_known_category(value: &str) -> bool {
    matches!(
        value,
        "Photos" | "Videos" | "Audio" | "Documents" | "Archives" | "Applications" | "Other"
    )
}

fn validate_file_name(value: &str) -> AppResult<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > 255
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || matches!(value, "." | "..")
    {
        return Err(AppError::Message("Enter a valid filename".into()));
    }
    Ok(value.to_string())
}

fn validate_recovery_manifest(manifest: &VaultManifest) -> AppResult<()> {
    if !matches!(
        manifest.format.as_str(),
        "televault-manifest-v1" | "televault-manifest-v2"
    ) || uuid::Uuid::parse_str(&manifest.file_id).is_err()
        || manifest.chunks.len() > 100_000
        || manifest.original_sha256.len() != 64
        || !manifest
            .original_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(AppError::Message(
            "Manifest identity or hash is invalid".into(),
        ));
    }
    for (index, chunk) in manifest.chunks.iter().enumerate() {
        if chunk.index as usize != index
            || chunk.message_id == 0
            || chunk.sha256.len() != 64
            || !chunk.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(AppError::Message("Manifest chunk map is invalid".into()));
        }
    }
    if manifest.original_size > 0 && manifest.chunks.is_empty() {
        return Err(AppError::Message("Manifest contains no file chunks".into()));
    }
    Ok(())
}

fn push_warning(warnings: &mut Vec<String>, warning: String) {
    if warnings.len() < 20 {
        warnings.push(warning);
    }
}

fn collect_upload_files(inputs: &[String]) -> AppResult<Vec<String>> {
    let mut files = std::collections::BTreeSet::new();
    for raw in inputs {
        let path = PathBuf::from(raw);
        let metadata = path.symlink_metadata().map_err(|error| {
            AppError::Message(format!("Cannot open '{}': {error}", path.display()))
        })?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_file() {
            files.insert(path.to_string_lossy().to_string());
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&path).follow_links(false) {
            let entry = entry.map_err(|error| {
                AppError::Message(format!("Cannot read folder '{}': {error}", path.display()))
            })?;
            if entry.file_type().is_file() {
                files.insert(entry.path().to_string_lossy().to_string());
            }
        }
    }
    if files.is_empty() {
        return Err(AppError::Message(
            "The selected folder does not contain any regular files".into(),
        ));
    }
    Ok(files.into_iter().collect())
}

fn folder_path_for_upload(file: &Path, root: Option<&Path>) -> String {
    let Some(root) = root else {
        return String::new();
    };
    let Some(root_name) = root.file_name().and_then(|name| name.to_str()) else {
        return String::new();
    };
    let mut parts = vec![root_name.to_string()];
    if let Ok(relative) = file.strip_prefix(root) {
        if let Some(parent) = relative.parent() {
            parts.extend(parent.components().filter_map(|component| match component {
                std::path::Component::Normal(name) => name.to_str().map(str::to_string),
                _ => None,
            }));
        }
    }
    parts.join("/")
}

fn normalize_vault_path(path: &str) -> AppResult<String> {
    if path.is_empty() {
        return Ok(String::new());
    }
    let mut components = Vec::new();
    for component in path.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains('\\')
            || component.chars().any(char::is_control)
        {
            return Err(AppError::Message("The folder path is invalid".into()));
        }
        components.push(component);
    }
    Ok(components.join("/"))
}

fn new_vault_folder_path(parent_path: &str, name: &str) -> AppResult<String> {
    let parent = normalize_vault_path(parent_path)?;
    let name = name.trim();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
        || name.chars().count() > 255
    {
        return Err(AppError::Message(
            "Use a folder name without slashes or control characters".into(),
        ));
    }
    Ok(join_vault_paths(&parent, name))
}

fn join_vault_paths(parent: &str, child: &str) -> String {
    match (parent.is_empty(), child.is_empty()) {
        (true, _) => child.to_string(),
        (_, true) => parent.to_string(),
        _ => format!("{parent}/{child}"),
    }
}

fn safe_folder_destination(base: &Path, folder_path: &str) -> PathBuf {
    folder_path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

fn proportional(value: u64, total: u64, original: u64) -> u64 {
    ((value.min(total) as f64 / total.max(1) as f64) * original as f64) as u64
}
fn unique_destination(dir: &Path, name: &str) -> PathBuf {
    let clean = Path::new(name)
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("download");
    let first = dir.join(clean);
    if !first.exists() {
        return first;
    }
    let stem = Path::new(clean)
        .file_stem()
        .and_then(|x| x.to_str())
        .unwrap_or("download");
    let ext = Path::new(clean).extension().and_then(|x| x.to_str());
    for i in 2..10000 {
        let candidate = dir.join(match ext {
            Some(x) => format!("{stem} ({i}).{x}"),
            None => format!("{stem} ({i})"),
        });
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{}-{}", uuid::Uuid::new_v4(), clean))
}
fn ignored_watch_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.starts_with('.')
        || [".part", ".crdownload", ".download", ".tmp", ".temp"]
            .iter()
            .any(|x| name.ends_with(x))
}

#[cfg(test)]
mod tests {
    use super::{
        cached_avatar, collect_upload_files, folder_path_for_upload, is_retryable_transfer_error,
        join_vault_paths, new_vault_folder_path, raster_image_mime, retry_delay_seconds,
        safe_folder_destination, WorkDirectoryGuard,
    };
    use crate::error::AppError;
    use std::fs;
    use std::path::Path;

    #[test]
    fn folder_upload_collects_nested_files_in_stable_order() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("projects").join("notes");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.path().join("projects").join("photo.jpg"), b"photo").unwrap();
        fs::write(nested.join("readme.txt"), b"notes").unwrap();

        let files =
            collect_upload_files(&[root.path().join("projects").to_string_lossy().to_string()])
                .unwrap();

        assert_eq!(files.len(), 2);
        assert!(Path::new(&files[0]).ends_with(Path::new("notes").join("readme.txt")));
        assert!(Path::new(&files[1]).ends_with("photo.jpg"));
    }

    #[test]
    fn retry_policy_honors_flood_wait_and_rejects_permanent_telegram_errors() {
        let flood = AppError::Telegram("FLOOD_WAIT_17 (Telegram code 429)".into());
        assert!(is_retryable_transfer_error(&flood));
        assert_eq!(retry_delay_seconds(&flood, 1), 17);
        assert!(!is_retryable_transfer_error(&AppError::Telegram(
            "Chat not found (Telegram code 400)".into()
        )));
        assert!(!is_retryable_transfer_error(&AppError::Message(
            "Transfer cancelled".into()
        )));
    }

    #[test]
    fn folder_upload_rejects_empty_folders() {
        let root = tempfile::tempdir().unwrap();
        let error = collect_upload_files(&[root.path().to_string_lossy().to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not contain any regular files"));
    }

    #[test]
    fn folder_upload_preserves_virtual_parent_paths() {
        let root = std::path::Path::new("/Users/example/Project Files");
        assert_eq!(
            folder_path_for_upload(
                std::path::Path::new("/Users/example/Project Files/docs/readme.txt"),
                Some(root),
            ),
            "Project Files/docs"
        );
        assert_eq!(
            folder_path_for_upload(
                std::path::Path::new("/Users/example/Project Files/photo.jpg"),
                Some(root),
            ),
            "Project Files"
        );
    }

    #[test]
    fn download_folder_paths_cannot_escape_the_destination() {
        assert_eq!(
            safe_folder_destination(
                std::path::Path::new("/Downloads/TiVault"),
                "../Project/docs",
            ),
            std::path::Path::new("/Downloads/TiVault/Project/docs")
        );
    }

    #[test]
    fn created_and_uploaded_folders_join_the_current_vault_location() {
        assert_eq!(
            new_vault_folder_path("Projects/2026", "Design Assets").unwrap(),
            "Projects/2026/Design Assets"
        );
        assert_eq!(
            join_vault_paths("Projects/2026", "Camera Uploads/July"),
            "Projects/2026/Camera Uploads/July"
        );
        assert!(new_vault_folder_path("Projects", "../Secrets").is_err());
        assert!(new_vault_folder_path("Projects", "Bad/Name").is_err());
    }

    #[test]
    fn upload_work_directory_is_removed_when_the_guard_drops() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("upload-work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("part-000000.tvchunk"), b"temporary data").unwrap();

        {
            let _guard = WorkDirectoryGuard::new(work.clone());
            assert!(work.exists());
        }

        assert!(!work.exists());
    }

    #[test]
    fn avatar_cache_accepts_bounded_raster_images_and_rejects_svg() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("42.avatar"), b"\xff\xd8\xffavatar").unwrap();
        assert_eq!(cached_avatar(root.path()).unwrap().0, 42);
        assert_eq!(raster_image_mime(b"\xff\xd8\xffavatar"), Some("image/jpeg"));

        fs::remove_file(root.path().join("42.avatar")).unwrap();
        fs::write(
            root.path().join("43.avatar"),
            br#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#,
        )
        .unwrap();
        assert!(cached_avatar(root.path()).is_none());
        assert!(raster_image_mime(b"<svg></svg>").is_none());
    }
}
