mod catalog;
mod chunker;
mod commands;
mod core;
mod error;
mod models;
mod security;
mod telegram;
mod web;

use commands::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().plugin(tauri_plugin_process::init());
    let builder = if option_env!("TELEVAULT_UPDATE_PUBLIC_KEY").is_some()
        && option_env!("TELEVAULT_UPDATE_ENDPOINT").is_some()
    {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    } else {
        builder
    };
    let app = builder
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let core = core::Core::new(&data_dir)
                .map_err(|error| Box::<dyn std::error::Error>::from(error.to_string()))?;
            app.manage(AppState(core.clone()));
            tauri::async_runtime::spawn(core.clone().watch_loop());
            tauri::async_runtime::spawn(core.clone().preview_cleanup_loop());
            tauri::async_runtime::spawn(core.clone().lock_timeout_loop());
            tauri::async_runtime::spawn(core.clone().trash_cleanup_loop());
            tauri::async_runtime::spawn(core.clone().health_check_loop());
            let resource_web = app.path().resource_dir()?.join("web-dist");
            let development_web =
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web-dist");
            let web_root = if resource_web.exists() {
                resource_web
            } else {
                development_web
            };
            tauri::async_runtime::spawn(web::serve(core, web_root));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_lock_status,
            commands::record_activity,
            commands::unlock_app,
            commands::configure_app_lock,
            commands::disable_app_lock,
            commands::lock_app,
            commands::get_dashboard,
            commands::get_account_avatar,
            commands::queue_uploads,
            commands::clear_preview_cache,
            commands::recover_vault,
            commands::test_recovery,
            commands::run_health_check,
            commands::set_file_favorite,
            commands::set_file_tags,
            commands::rename_file,
            commands::move_file,
            commands::copy_file,
            commands::expand_upload_paths,
            commands::dismiss_transfer,
            commands::dismiss_transfers,
            commands::clear_transfer_history,
            commands::pause_transfer,
            commands::resume_transfer,
            commands::cancel_transfer,
            commands::download_file,
            commands::start_preview,
            commands::preview_text,
            commands::stop_preview,
            commands::lookup_share_recipient,
            commands::recent_share_recipients,
            commands::share_file,
            commands::lookup_folder_share_recipient,
            commands::recent_folder_share_recipients,
            commands::share_folder,
            commands::create_folder,
            commands::download_folder,
            commands::delete_folder,
            commands::delete_file,
            commands::delete_files,
            commands::restore_file,
            commands::permanently_delete_file,
            commands::permanently_delete_files,
            commands::empty_trash,
            commands::disconnect_account,
            commands::remove_account,
            commands::reveal_cached_file,
            commands::add_watch_folder,
            commands::remove_watch_folder,
            commands::update_settings,
            commands::start_telegram_login,
            commands::start_telegram_qr_login,
            commands::poll_telegram_qr_login,
            commands::complete_telegram_login,
            commands::complete_telegram_password,
            commands::export_recovery_key
        ])
        .build(tauri::generate_context!())
        .expect("error while building TiVault");

    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            app_handle.state::<AppState>().0.shutdown();
        }
    });
}
