use crate::core::Core;
use crate::error::AppError;
use crate::models::{LoginRequest, NewWatchFolder, UploadOptions, WatchFolder};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path as RoutePath, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
};
use axum::http::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

type WebResult<T> = Result<Json<T>, (StatusCode, String)>;
fn web_error(error: AppError) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, error.to_string())
}

pub async fn serve(core: Arc<Core>, static_root: PathBuf) {
    let index = static_root.join("index.html");
    let service = ServeDir::new(&static_root).not_found_service(ServeFile::new(index));
    let cors = CorsLayer::new()
        .allow_origin([
            HeaderValue::from_static("http://localhost:1420"),
            HeaderValue::from_static("http://127.0.0.1:1420"),
            HeaderValue::from_static("http://127.0.0.1:7468"),
            HeaderValue::from_static("tauri://localhost"),
            HeaderValue::from_static("http://tauri.localhost"),
            HeaderValue::from_static("https://tauri.localhost"),
        ])
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        .allow_headers(Any);
    let app = Router::new()
        .route("/api/lock/status", get(lock_status))
        .route("/api/lock/activity", post(record_activity))
        .route("/api/lock/unlock", post(unlock_app))
        .route("/api/lock/configure", post(configure_app_lock))
        .route("/api/lock/disable", post(disable_app_lock))
        .route("/api/lock/now", post(lock_app))
        .route("/api/dashboard", get(dashboard))
        .route("/api/accounts/{id}/avatar", get(account_avatar))
        .route("/api/accounts/{id}/disconnect", post(disconnect_account))
        .route("/api/accounts/{id}/remove", post(remove_account))
        .route("/api/stage", post(stage_files))
        .route("/api/uploads", post(queue_uploads))
        .route("/api/transfers/history/delete", post(dismiss_transfers))
        .route("/api/transfers/history/clear", post(clear_transfer_history))
        .route("/api/transfers/{id}/dismiss", post(dismiss_transfer))
        .route("/api/transfers/{id}/pause", post(pause_transfer))
        .route("/api/transfers/{id}/resume", post(resume_transfer))
        .route("/api/transfers/{id}/cancel", post(cancel_transfer))
        .route("/api/files/{id}/download", post(download_file))
        .route("/api/files/{id}/rename", post(rename_file))
        .route("/api/files/{id}/move", post(move_file))
        .route("/api/files/{id}/copy", post(copy_file))
        .route("/api/files/{id}/preview", post(start_preview))
        .route("/api/preview/{token}/content", get(preview_content))
        .route("/api/preview/{token}/text", post(preview_text))
        .route("/api/preview/{token}/stop", post(stop_preview))
        .route(
            "/api/files/{id}/share/recipient",
            post(lookup_share_recipient),
        )
        .route("/api/files/{id}/share/recent", get(recent_share_recipients))
        .route("/api/files/{id}/share", post(share_file))
        .route(
            "/api/folders/share/recipient",
            post(lookup_folder_share_recipient),
        )
        .route(
            "/api/folders/share/recent",
            post(recent_folder_share_recipients),
        )
        .route("/api/folders/share", post(share_folder))
        .route("/api/folders/create", post(create_folder))
        .route("/api/folders/download", post(download_folder))
        .route("/api/folders/delete", post(delete_folder))
        .route("/api/files/delete-many", post(delete_files))
        .route(
            "/api/files/delete-many/permanent",
            post(permanently_delete_files),
        )
        .route("/api/files/{id}/delete", post(delete_file))
        .route("/api/files/{id}/restore", post(restore_file))
        .route(
            "/api/files/{id}/delete-permanently",
            post(permanently_delete_file),
        )
        .route("/api/files/{id}/favorite", post(set_file_favorite))
        .route("/api/files/{id}/tags", post(set_file_tags))
        .route("/api/trash/empty", post(empty_trash))
        .route("/api/watch", post(add_watch_folder))
        .route("/api/watch/{id}", delete(remove_watch_folder))
        .route("/api/settings", post(update_settings))
        .route("/api/cache/clear", post(clear_preview_cache))
        .route("/api/recovery/restore", post(recover_vault))
        .route("/api/recovery/test", post(test_recovery))
        .route("/api/health/check", post(run_health_check))
        .route("/api/auth/start", post(start_login))
        .route("/api/auth/qr/start", post(start_qr_login))
        .route("/api/auth/qr/poll", post(poll_qr_login))
        .route("/api/auth/code", post(complete_login))
        .route("/api/auth/password", post(complete_password))
        .route("/api/recovery", get(export_recovery))
        .fallback_service(service)
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(
            Arc::clone(&core),
            require_unlocked,
        ))
        .layer(cors)
        .with_state(core);
    match tokio::net::TcpListener::bind("127.0.0.1:7468").await {
        Ok(listener) => {
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("TiVault web companion stopped: {error}");
            }
        }
        Err(error) => eprintln!("TiVault web companion could not start: {error}"),
    }
}

async fn require_unlocked(
    State(core): State<Arc<Core>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let public =
        !path.starts_with("/api/") || matches!(path, "/api/lock/status" | "/api/lock/unlock");
    if public || core.ensure_unlocked().is_ok() {
        next.run(request).await
    } else {
        Response::builder()
            .status(StatusCode::LOCKED)
            .header(CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from("TiVault is locked"))
            .unwrap()
    }
}

#[derive(Deserialize)]
struct LockPasswordRequest {
    password: String,
}

async fn lock_status(State(core): State<Arc<Core>>) -> WebResult<crate::models::LockStatus> {
    core.lock_status().map(Json).map_err(web_error)
}

async fn record_activity(State(core): State<Arc<Core>>) -> WebResult<crate::models::LockStatus> {
    core.record_activity().map(Json).map_err(web_error)
}

async fn unlock_app(
    State(core): State<Arc<Core>>,
    Json(request): Json<LockPasswordRequest>,
) -> WebResult<crate::models::LockStatus> {
    core.unlock(&request.password).map(Json).map_err(web_error)
}

async fn configure_app_lock(
    State(core): State<Arc<Core>>,
    Json(request): Json<LockPasswordRequest>,
) -> WebResult<crate::models::LockStatus> {
    core.configure_app_lock(&request.password)
        .map(Json)
        .map_err(web_error)
}

async fn disable_app_lock(
    State(core): State<Arc<Core>>,
    Json(request): Json<LockPasswordRequest>,
) -> WebResult<crate::models::LockStatus> {
    core.disable_app_lock(&request.password)
        .map(Json)
        .map_err(web_error)
}

async fn lock_app(State(core): State<Arc<Core>>) -> WebResult<crate::models::LockStatus> {
    core.lock().map(Json).map_err(web_error)
}

async fn dashboard(State(core): State<Arc<Core>>) -> WebResult<crate::models::Dashboard> {
    core.catalog
        .dashboard(core.master.is_ready(), core.master.keychain_backed())
        .map(Json)
        .map_err(web_error)
}

async fn account_avatar(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Option<String>> {
    core.account_avatar(&id).await.map(Json).map_err(web_error)
}

async fn stage_files(
    State(core): State<Arc<Core>>,
    mut multipart: Multipart,
) -> WebResult<Vec<String>> {
    let directory = core.staging_dir();
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|e| web_error(e.into()))?;
    let mut paths = Vec::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let original = field.file_name().unwrap_or("upload.bin");
        let safe = Path::new(original)
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("upload.bin");
        let destination = unique_staging_path(&directory, safe);
        let mut output = tokio::fs::File::create(&destination)
            .await
            .map_err(|e| web_error(e.into()))?;
        while let Some(bytes) = field
            .chunk()
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        {
            output
                .write_all(&bytes)
                .await
                .map_err(|e| web_error(e.into()))?;
        }
        output.flush().await.map_err(|e| web_error(e.into()))?;
        paths.push(destination.to_string_lossy().to_string());
    }
    Ok(Json(paths))
}

async fn queue_uploads(
    State(core): State<Arc<Core>>,
    Json(options): Json<UploadOptions>,
) -> WebResult<Vec<crate::models::VaultFile>> {
    core.queue_paths(options).await.map(Json).map_err(web_error)
}
async fn dismiss_transfer(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.dismiss_transfer_history(&[id]).map_err(web_error)?;
    Ok(Json(json!({"ok":true})))
}
async fn dismiss_transfers(
    State(core): State<Arc<Core>>,
    Json(ids): Json<Vec<String>>,
) -> WebResult<usize> {
    core.dismiss_transfer_history(&ids)
        .map(Json)
        .map_err(web_error)
}
async fn clear_transfer_history(State(core): State<Arc<Core>>) -> WebResult<usize> {
    core.clear_transfer_history().map(Json).map_err(web_error)
}
async fn pause_transfer(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.pause(&id)
        .map(|_| Json(json!({"ok":true})))
        .map_err(web_error)
}
async fn resume_transfer(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.resume(&id)
        .map(|_| Json(json!({"ok":true})))
        .map_err(web_error)
}
async fn cancel_transfer(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.cancel(&id)
        .map(|_| Json(json!({"ok":true})))
        .map_err(web_error)
}
async fn download_file(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.spawn_download(id)
        .map(|_| Json(json!({"ok":true})))
        .map_err(web_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameFileRequest {
    new_name: String,
}

async fn rename_file(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
    Json(request): Json<RenameFileRequest>,
) -> WebResult<crate::models::VaultFile> {
    core.rename_file(&id, &request.new_name)
        .await
        .map(Json)
        .map_err(web_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveFileRequest {
    folder_path: String,
}

async fn move_file(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
    Json(request): Json<MoveFileRequest>,
) -> WebResult<crate::models::VaultFile> {
    core.move_file(&id, &request.folder_path)
        .await
        .map(Json)
        .map_err(web_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CopyFileRequest {
    new_name: String,
    folder_path: String,
}

async fn copy_file(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
    Json(request): Json<CopyFileRequest>,
) -> WebResult<crate::models::VaultFile> {
    core.copy_file(&id, &request.new_name, &request.folder_path)
        .await
        .map(Json)
        .map_err(web_error)
}

async fn start_preview(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<crate::models::PreviewInfo> {
    core.start_preview(&id).map(Json).map_err(web_error)
}

async fn preview_text(
    State(core): State<Arc<Core>>,
    RoutePath(token): RoutePath<String>,
) -> WebResult<crate::models::PreviewText> {
    core.preview_text(&token).await.map(Json).map_err(web_error)
}

async fn stop_preview(
    State(core): State<Arc<Core>>,
    RoutePath(token): RoutePath<String>,
) -> WebResult<Value> {
    core.stop_preview(&token).await.map_err(web_error)?;
    Ok(Json(json!({"ok":true})))
}

async fn preview_content(
    State(core): State<Arc<Core>>,
    RoutePath(token): RoutePath<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    const RESPONSE_LIMIT: u64 = 8 * 1024 * 1024;
    let info = core.preview_info_for_token(&token).map_err(web_error)?;
    if info.kind == "unsupported" || info.kind == "document" || info.kind == "text" {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            info.message
                .unwrap_or_else(|| "This file has no binary inline preview".into()),
        ));
    }
    if info.size == 0 {
        return Err((StatusCode::NO_CONTENT, "This file is empty".into()));
    }
    let mime = HeaderValue::from_str(&info.mime_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    if method == Method::HEAD {
        return Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, mime)
            .header(CONTENT_LENGTH, info.size)
            .header(ACCEPT_RANGES, "bytes")
            .header(CACHE_CONTROL, "private, no-store")
            .header("x-content-type-options", "nosniff")
            .body(Body::empty())
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()));
    }

    let range_header = headers.get("range").and_then(|value| value.to_str().ok());
    let full_response = range_header.is_none()
        && matches!(info.kind.as_str(), "image" | "pdf")
        && info.size <= 128 * 1024 * 1024;
    let (status, start, requested) = if full_response {
        (StatusCode::OK, 0, info.size)
    } else if let Some(range) = range_header {
        let (start, length) = parse_preview_range(range, info.size, RESPONSE_LIMIT)?;
        (StatusCode::PARTIAL_CONTENT, start, length)
    } else {
        (
            StatusCode::PARTIAL_CONTENT,
            0,
            info.size.min(RESPONSE_LIMIT),
        )
    };
    let bytes = if full_response {
        core.preview_full_bytes(&token).await.map_err(web_error)?
    } else {
        core.preview_bytes(&token, start, requested)
            .await
            .map_err(web_error)?
    };
    let end = start + bytes.len() as u64 - 1;
    let mut response = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, mime)
        .header(CONTENT_LENGTH, bytes.len())
        .header(ACCEPT_RANGES, "bytes")
        .header(CACHE_CONTROL, "private, no-store")
        .header("x-content-type-options", "nosniff")
        .header("content-security-policy", "default-src 'none'; sandbox");
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(CONTENT_RANGE, format!("bytes {start}-{end}/{}", info.size));
    }
    response
        .body(Body::from(bytes))
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

fn parse_preview_range(
    value: &str,
    size: u64,
    response_limit: u64,
) -> Result<(u64, u64), (StatusCode, String)> {
    let range = value
        .strip_prefix("bytes=")
        .filter(|range| !range.contains(','))
        .ok_or_else(|| {
            (
                StatusCode::RANGE_NOT_SATISFIABLE,
                "Invalid preview range".into(),
            )
        })?;
    let (start_raw, end_raw) = range.split_once('-').ok_or_else(|| {
        (
            StatusCode::RANGE_NOT_SATISFIABLE,
            "Invalid preview range".into(),
        )
    })?;
    let (start, requested_end) = if start_raw.is_empty() {
        let suffix = end_raw
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "Invalid preview range".into(),
                )
            })?;
        (size.saturating_sub(suffix.min(size)), size - 1)
    } else {
        let start = start_raw.parse::<u64>().map_err(|_| {
            (
                StatusCode::RANGE_NOT_SATISFIABLE,
                "Invalid preview range".into(),
            )
        })?;
        let end = if end_raw.is_empty() {
            size - 1
        } else {
            end_raw.parse::<u64>().map_err(|_| {
                (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "Invalid preview range".into(),
                )
            })?
        };
        (start, end)
    };
    if start >= size || requested_end < start {
        return Err((
            StatusCode::RANGE_NOT_SATISFIABLE,
            "Preview range is outside the file".into(),
        ));
    }
    let end = requested_end
        .min(size - 1)
        .min(start.saturating_add(response_limit - 1));
    Ok((start, end - start + 1))
}

#[derive(Deserialize)]
struct RecipientLookupRequest {
    username: String,
}

async fn lookup_share_recipient(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
    Json(request): Json<RecipientLookupRequest>,
) -> WebResult<crate::models::ShareRecipient> {
    core.lookup_share_recipient(&id, &request.username)
        .await
        .map(Json)
        .map_err(web_error)
}

async fn recent_share_recipients(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Vec<crate::models::ShareRecipient>> {
    core.recent_share_recipients(&id)
        .await
        .map(Json)
        .map_err(web_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShareFileRequest {
    recipient_token: String,
    allow_decrypt: bool,
}

async fn share_file(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
    Json(request): Json<ShareFileRequest>,
) -> WebResult<String> {
    core.spawn_share(&id, &request.recipient_token, request.allow_decrypt)
        .map(Json)
        .map_err(web_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderRecipientLookupRequest {
    path: String,
    username: String,
}

async fn lookup_folder_share_recipient(
    State(core): State<Arc<Core>>,
    Json(request): Json<FolderRecipientLookupRequest>,
) -> WebResult<crate::models::ShareRecipient> {
    core.lookup_folder_share_recipient(&request.path, &request.username)
        .await
        .map(Json)
        .map_err(web_error)
}

#[derive(Deserialize)]
struct FolderSharePathRequest {
    path: String,
}

async fn recent_folder_share_recipients(
    State(core): State<Arc<Core>>,
    Json(request): Json<FolderSharePathRequest>,
) -> WebResult<Vec<crate::models::ShareRecipient>> {
    core.recent_folder_share_recipients(&request.path)
        .await
        .map(Json)
        .map_err(web_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShareFolderRequest {
    path: String,
    recipient_token: String,
    allow_decrypt: bool,
}

async fn share_folder(
    State(core): State<Arc<Core>>,
    Json(request): Json<ShareFolderRequest>,
) -> WebResult<Vec<String>> {
    core.spawn_folder_share(
        &request.path,
        &request.recipient_token,
        request.allow_decrypt,
    )
    .map(Json)
    .map_err(web_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateFolderRequest {
    parent_path: String,
    name: String,
}

async fn create_folder(
    State(core): State<Arc<Core>>,
    Json(request): Json<CreateFolderRequest>,
) -> WebResult<crate::models::VaultFolder> {
    core.create_folder(&request.parent_path, &request.name)
        .map(Json)
        .map_err(web_error)
}

#[derive(Deserialize)]
struct FolderPathRequest {
    path: String,
}

async fn download_folder(
    State(core): State<Arc<Core>>,
    Json(request): Json<FolderPathRequest>,
) -> WebResult<usize> {
    core.spawn_folder_download(&request.path)
        .map(Json)
        .map_err(web_error)
}

async fn delete_folder(
    State(core): State<Arc<Core>>,
    Json(request): Json<FolderPathRequest>,
) -> WebResult<usize> {
    core.move_folder_to_trash(&request.path)
        .await
        .map(Json)
        .map_err(web_error)
}

async fn delete_file(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.move_to_trash(&id).await.map_err(web_error)?;
    Ok(Json(json!({"ok":true})))
}
async fn delete_files(
    State(core): State<Arc<Core>>,
    Json(ids): Json<Vec<String>>,
) -> WebResult<usize> {
    core.move_many_to_trash(&ids)
        .await
        .map(Json)
        .map_err(web_error)
}

async fn restore_file(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.restore_from_trash(&id).map_err(web_error)?;
    Ok(Json(json!({"ok":true})))
}

async fn permanently_delete_file(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.permanently_delete(&id).await.map_err(web_error)?;
    Ok(Json(json!({"ok":true})))
}

async fn permanently_delete_files(
    State(core): State<Arc<Core>>,
    Json(ids): Json<Vec<String>>,
) -> WebResult<usize> {
    core.permanently_delete_many(&ids)
        .await
        .map(Json)
        .map_err(web_error)
}

async fn empty_trash(State(core): State<Arc<Core>>) -> WebResult<usize> {
    core.empty_trash().await.map(Json).map_err(web_error)
}

#[derive(Deserialize)]
struct FavoriteRequest {
    favorite: bool,
}

async fn set_file_favorite(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
    Json(request): Json<FavoriteRequest>,
) -> WebResult<Value> {
    core.set_favorite(&id, request.favorite)
        .map_err(web_error)?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
struct TagsRequest {
    tags: Vec<String>,
}

async fn set_file_tags(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
    Json(request): Json<TagsRequest>,
) -> WebResult<Value> {
    core.set_tags(&id, request.tags).map_err(web_error)?;
    Ok(Json(json!({"ok":true})))
}

async fn disconnect_account(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.disconnect_account(&id).await.map_err(web_error)?;
    Ok(Json(json!({"ok":true})))
}

async fn remove_account(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.remove_account(&id).await.map_err(web_error)?;
    Ok(Json(json!({"ok":true})))
}
async fn add_watch_folder(
    State(core): State<Arc<Core>>,
    Json(folder): Json<NewWatchFolder>,
) -> WebResult<WatchFolder> {
    if !Path::new(&folder.path).is_dir() {
        return Err((StatusCode::BAD_REQUEST, "Choose an existing folder".into()));
    }
    let complete = WatchFolder {
        id: uuid::Uuid::new_v4().to_string(),
        path: folder.path,
        enabled: folder.enabled,
        encrypt: folder.encrypt,
        account_id: folder.account_id,
        uploaded_count: 0,
    };
    core.catalog
        .add_watch_folder(&complete)
        .map_err(web_error)?;
    Ok(Json(complete))
}
async fn remove_watch_folder(
    State(core): State<Arc<Core>>,
    RoutePath(id): RoutePath<String>,
) -> WebResult<Value> {
    core.catalog
        .remove_watch_folder(&id)
        .map(|_| Json(json!({"ok":true})))
        .map_err(web_error)
}
async fn update_settings(
    State(core): State<Arc<Core>>,
    Json(settings): Json<Value>,
) -> WebResult<crate::models::Dashboard> {
    let object = settings.as_object().ok_or((
        StatusCode::BAD_REQUEST,
        "Settings payload is invalid".into(),
    ))?;
    for (key, value) in object {
        let pair = match key.as_str() {
            "speedProfile" => Some(("speed_profile", value.as_str().unwrap_or("balanced").into())),
            "cacheLimitGb" => Some((
                "cache_limit",
                (value.as_u64().unwrap_or(25) * 1024 * 1024 * 1024).to_string(),
            )),
            "previewCacheLimitMb" => Some((
                "preview_cache_limit",
                (value.as_u64().unwrap_or(512).clamp(128, 512) * 1024 * 1024).to_string(),
            )),
            "previewCacheTtlMinutes" => Some((
                "preview_cache_ttl_minutes",
                value.as_u64().unwrap_or(15).clamp(5, 60).to_string(),
            )),
            "appLockTimeoutMinutes" => Some((
                "app_lock_timeout_minutes",
                value.as_u64().unwrap_or(15).clamp(1, 120).to_string(),
            )),
            "recycleRetentionDays" => Some((
                "recycle_retention_days",
                match value.as_u64().unwrap_or(30) {
                    7 => "7",
                    14 => "14",
                    _ => "30",
                }
                .into(),
            )),
            "automaticRetryCount" => Some((
                "automatic_retry_count",
                value.as_u64().unwrap_or(3).clamp(0, 5).to_string(),
            )),
            "notificationsEnabled" => Some((
                "notifications_enabled",
                value.as_bool().unwrap_or(false).to_string(),
            )),
            "healthChecksEnabled" => Some((
                "health_checks_enabled",
                value.as_bool().unwrap_or(true).to_string(),
            )),
            "healthCheckIntervalDays" => Some((
                "health_check_interval_days",
                value.as_u64().unwrap_or(7).clamp(1, 30).to_string(),
            )),
            "encryptByDefault" => Some((
                "encrypt_by_default",
                value.as_bool().unwrap_or(true).to_string(),
            )),
            "hideEncryptedNames" => Some((
                "hide_encrypted_names",
                value.as_bool().unwrap_or(true).to_string(),
            )),
            _ => None,
        };
        if let Some((key, value)) = pair {
            core.catalog.set_setting(key, value).map_err(web_error)?
        }
    }
    core.catalog
        .dashboard(core.master.is_ready(), core.master.keychain_backed())
        .map(Json)
        .map_err(web_error)
}

async fn clear_preview_cache(State(core): State<Arc<Core>>) -> WebResult<u64> {
    core.clear_preview_cache()
        .await
        .map(Json)
        .map_err(web_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoverVaultRequest {
    account_id: String,
}

async fn recover_vault(
    State(core): State<Arc<Core>>,
    Json(request): Json<RecoverVaultRequest>,
) -> WebResult<crate::models::RecoveryReport> {
    core.recover_vault(&request.account_id)
        .await
        .map(Json)
        .map_err(web_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryTestRequest {
    account_id: String,
    recovery_key: String,
}

async fn test_recovery(
    State(core): State<Arc<Core>>,
    Json(request): Json<RecoveryTestRequest>,
) -> WebResult<crate::models::RecoveryTestReport> {
    core.test_recovery(&request.account_id, &request.recovery_key)
        .await
        .map(Json)
        .map_err(web_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HealthCheckRequest {
    account_id: String,
    sample_count: u64,
}

async fn run_health_check(
    State(core): State<Arc<Core>>,
    Json(request): Json<HealthCheckRequest>,
) -> WebResult<crate::models::HealthReport> {
    core.health_check(&request.account_id, request.sample_count)
        .await
        .map(Json)
        .map_err(web_error)
}
async fn start_login(
    State(core): State<Arc<Core>>,
    Json(request): Json<LoginRequest>,
) -> WebResult<crate::models::LoginResult> {
    core.telegram
        .start_login(request)
        .await
        .map(Json)
        .map_err(web_error)
}
async fn start_qr_login(
    State(core): State<Arc<Core>>,
    Json(request): Json<LoginRequest>,
) -> WebResult<crate::models::LoginResult> {
    core.telegram
        .start_qr_login(request)
        .await
        .map(Json)
        .map_err(web_error)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlowRequest {
    flow_id: String,
}
async fn poll_qr_login(
    State(core): State<Arc<Core>>,
    Json(request): Json<FlowRequest>,
) -> WebResult<crate::models::LoginResult> {
    core.telegram
        .poll_qr_login(&request.flow_id)
        .await
        .map(Json)
        .map_err(web_error)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeRequest {
    flow_id: String,
    code: String,
}
async fn complete_login(
    State(core): State<Arc<Core>>,
    Json(request): Json<CodeRequest>,
) -> WebResult<crate::models::LoginResult> {
    core.telegram
        .complete_login(&request.flow_id, &request.code)
        .await
        .map(Json)
        .map_err(web_error)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PasswordRequest {
    flow_id: String,
    password: String,
}
async fn complete_password(
    State(core): State<Arc<Core>>,
    Json(request): Json<PasswordRequest>,
) -> WebResult<crate::models::LoginResult> {
    core.telegram
        .complete_password(&request.flow_id, &request.password)
        .await
        .map(Json)
        .map_err(web_error)
}
async fn export_recovery(State(core): State<Arc<Core>>) -> WebResult<Value> {
    Ok(Json(json!({"key":core.master.export_recovery()})))
}

fn unique_staging_path(directory: &Path, name: &str) -> PathBuf {
    let candidate = directory.join(format!("{}-{name}", uuid::Uuid::new_v4()));
    candidate
}

#[cfg(test)]
mod tests {
    use super::parse_preview_range;

    #[test]
    fn preview_ranges_are_bounded_and_support_suffixes() {
        assert_eq!(parse_preview_range("bytes=10-19", 100, 8).unwrap(), (10, 8));
        assert_eq!(parse_preview_range("bytes=-12", 100, 32).unwrap(), (88, 12));
        assert_eq!(parse_preview_range("bytes=95-", 100, 32).unwrap(), (95, 5));
        assert!(parse_preview_range("bytes=100-", 100, 32).is_err());
        assert!(parse_preview_range("bytes=0-1,4-5", 100, 32).is_err());
    }
}
