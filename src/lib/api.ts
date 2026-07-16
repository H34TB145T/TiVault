import { invoke } from "@tauri-apps/api/core";
import type { Dashboard, HealthReport, LockStatus, LoginRequest, LoginResult, NewWatchFolder, PreviewInfo, PreviewText, RecoveryReport, RecoveryTestReport, ShareRecipient, UploadOptions, VaultFile, VaultFolderRecord, WatchFolder } from "./types";

const isTauri = () => "__TAURI_INTERNALS__" in window;

const demoDashboard: Dashboard = {
  files: [
    { id: "demo-1", name: "Northern lights.jpg", category: "Photos", size: 14_806_318, mimeType: "image/jpeg", encrypted: false, cached: true, chunkCount: 1, accountId: "personal", accountName: "Personal", createdAt: new Date().toISOString(), status: "ready", thumbnail: "/assets/demo-aurora.svg", favorite: true, tags: ["travel"] },
    { id: "demo-2", name: "Documentary master.mkv", category: "Videos", size: 10_847_392_104, mimeType: "video/x-matroska", encrypted: true, cached: false, chunkCount: 6, accountId: "personal", accountName: "Personal", createdAt: new Date(Date.now() - 86400000).toISOString(), status: "ready", favorite: false, tags: [] },
    { id: "demo-3", name: "Project archive.zip", category: "Archives", size: 1_352_921_088, mimeType: "application/zip", encrypted: true, cached: true, chunkCount: 1, accountId: "personal", accountName: "Personal", createdAt: new Date(Date.now() - 172800000).toISOString(), status: "ready", favorite: false, tags: ["project"] },
    { id: "demo-4", name: "TiVault specification.pdf", category: "Documents", size: 3_801_220, mimeType: "application/pdf", encrypted: false, cached: true, chunkCount: 1, accountId: "personal", accountName: "Personal", createdAt: new Date(Date.now() - 604800000).toISOString(), status: "ready", favorite: false, tags: [] }
  ],
  folders: [],
  transfers: [
    { id: "transfer-1", fileId: "demo-2", fileName: "Documentary master.mkv", direction: "upload", state: "uploading", progress: 0.68, transferred: 7_376_226_630, total: 10_847_392_104, speed: 12_800_000, etaSeconds: 271, encrypted: true }
  ],
  accounts: [{ id: "personal", name: "Personal", phone: "+44 •••• 2841", connected: true, color: "#2f7cff", initials: "RK", fileCount: 4, storedBytes: 12_218_920_730 }],
  watchFolders: [],
  cacheUsed: 1_371_528_626,
  cacheLimit: 25 * 1024 ** 3,
  previewCacheLimit: 512 * 1024 ** 2,
  previewCacheTtlMinutes: 15,
  storedBytes: 12_218_920_730,
  encryptionReady: true,
  keychainBacked: true,
  appLockEnabled: false,
  appLockTimeoutMinutes: 15,
  speedProfile: "balanced",
  recycleRetentionDays: 30,
  automaticRetryCount: 3,
  notificationsEnabled: false,
  healthChecksEnabled: true,
  healthCheckIntervalDays: 7,
  automaticUpdatesConfigured: false
};

function webData(): Dashboard {
  const saved = localStorage.getItem("televault.web.demo");
  if (!saved) return demoDashboard;
  const dashboard = JSON.parse(saved) as Dashboard;
  return { ...dashboard, folders: dashboard.folders ?? [] };
}

async function webFetch<T>(path: string, method = "GET", body?: unknown): Promise<T> {
  const base = window.location.port === "7468" ? "" : "http://127.0.0.1:7468";
  const response = await fetch(`${base}${path}`, {
    method,
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body)
  });
  if (!response.ok) throw new Error(await response.text() || `TiVault companion returned ${response.status}`);
  return response.json() as Promise<T>;
}

async function call<T>(command: string, args?: Record<string, unknown>, fallback?: () => T): Promise<T> {
  if (isTauri()) return invoke<T>(command, args);
  try {
    switch (command) {
      case "get_lock_status": return await webFetch<T>("/api/lock/status");
      case "record_activity": return await webFetch<T>("/api/lock/activity", "POST");
      case "unlock_app": return await webFetch<T>("/api/lock/unlock", "POST", { password: args?.password });
      case "configure_app_lock": return await webFetch<T>("/api/lock/configure", "POST", { password: args?.password });
      case "disable_app_lock": return await webFetch<T>("/api/lock/disable", "POST", { password: args?.password });
      case "lock_app": return await webFetch<T>("/api/lock/now", "POST");
      case "get_dashboard": return await webFetch<T>("/api/dashboard");
      case "get_account_avatar": return await webFetch<T>(`/api/accounts/${encodeURIComponent(String(args?.accountId ?? ""))}/avatar`);
      case "queue_uploads": return await webFetch<T>("/api/uploads", "POST", args?.options);
      case "dismiss_transfer": return await webFetch<T>(`/api/transfers/${args?.id}/dismiss`, "POST");
      case "dismiss_transfers": return await webFetch<T>("/api/transfers/history/delete", "POST", args?.ids);
      case "clear_transfer_history": return await webFetch<T>("/api/transfers/history/clear", "POST");
      case "pause_transfer": return await webFetch<T>(`/api/transfers/${args?.id}/pause`, "POST");
      case "resume_transfer": return await webFetch<T>(`/api/transfers/${args?.id}/resume`, "POST");
      case "cancel_transfer": return await webFetch<T>(`/api/transfers/${args?.id}/cancel`, "POST");
      case "download_file": return await webFetch<T>(`/api/files/${args?.id}/download`, "POST");
      case "rename_file": return await webFetch<T>(`/api/files/${args?.id}/rename`, "POST", { newName: args?.newName });
      case "move_file": return await webFetch<T>(`/api/files/${args?.id}/move`, "POST", { folderPath: args?.folderPath });
      case "copy_file": return await webFetch<T>(`/api/files/${args?.id}/copy`, "POST", { newName: args?.newName, folderPath: args?.folderPath });
      case "start_preview": return await webFetch<T>(`/api/files/${args?.id}/preview`, "POST");
      case "preview_text": return await webFetch<T>(`/api/preview/${args?.token}/text`, "POST");
      case "stop_preview": return await webFetch<T>(`/api/preview/${args?.token}/stop`, "POST");
      case "lookup_share_recipient": return await webFetch<T>(`/api/files/${args?.fileId}/share/recipient`, "POST", { username: args?.username });
      case "recent_share_recipients": return await webFetch<T>(`/api/files/${args?.fileId}/share/recent`);
      case "share_file": return await webFetch<T>(`/api/files/${args?.fileId}/share`, "POST", { recipientToken: args?.recipientToken, allowDecrypt: args?.allowDecrypt });
      case "lookup_folder_share_recipient": return await webFetch<T>("/api/folders/share/recipient", "POST", { path: args?.path, username: args?.username });
      case "recent_folder_share_recipients": return await webFetch<T>("/api/folders/share/recent", "POST", { path: args?.path });
      case "share_folder": return await webFetch<T>("/api/folders/share", "POST", { path: args?.path, recipientToken: args?.recipientToken, allowDecrypt: args?.allowDecrypt });
      case "create_folder": return await webFetch<T>("/api/folders/create", "POST", { parentPath: args?.parentPath, name: args?.name });
      case "download_folder": return await webFetch<T>("/api/folders/download", "POST", { path: args?.path });
      case "delete_folder": return await webFetch<T>("/api/folders/delete", "POST", { path: args?.path });
      case "delete_file": return await webFetch<T>(`/api/files/${args?.id}/delete`, "POST");
      case "delete_files": return await webFetch<T>("/api/files/delete-many", "POST", args?.ids);
      case "restore_file": return await webFetch<T>(`/api/files/${args?.id}/restore`, "POST");
      case "permanently_delete_file": return await webFetch<T>(`/api/files/${args?.id}/delete-permanently`, "POST");
      case "permanently_delete_files": return await webFetch<T>("/api/files/delete-many/permanent", "POST", args?.ids);
      case "empty_trash": return await webFetch<T>("/api/trash/empty", "POST");
      case "set_file_favorite": return await webFetch<T>(`/api/files/${args?.id}/favorite`, "POST", { favorite: args?.favorite });
      case "set_file_tags": return await webFetch<T>(`/api/files/${args?.id}/tags`, "POST", { tags: args?.tags });
      case "disconnect_account": return await webFetch<T>(`/api/accounts/${args?.accountId}/disconnect`, "POST");
      case "remove_account": return await webFetch<T>(`/api/accounts/${args?.accountId}/remove`, "POST");
      case "add_watch_folder": return await webFetch<T>("/api/watch", "POST", args?.folder);
      case "remove_watch_folder": return await webFetch<T>(`/api/watch/${args?.id}`, "DELETE");
      case "update_settings": return await webFetch<T>("/api/settings", "POST", args?.settings);
      case "clear_preview_cache": return await webFetch<T>("/api/cache/clear", "POST");
      case "recover_vault": return await webFetch<T>("/api/recovery/restore", "POST", { accountId: args?.accountId });
      case "test_recovery": return await webFetch<T>("/api/recovery/test", "POST", { accountId: args?.accountId, recoveryKey: args?.recoveryKey });
      case "run_health_check": return await webFetch<T>("/api/health/check", "POST", { accountId: args?.accountId, sampleCount: args?.sampleCount });
      case "start_telegram_login": return await webFetch<T>("/api/auth/start", "POST", args?.request);
      case "start_telegram_qr_login": return await webFetch<T>("/api/auth/qr/start", "POST", args?.request);
      case "poll_telegram_qr_login": return await webFetch<T>("/api/auth/qr/poll", "POST", args);
      case "complete_telegram_login": return await webFetch<T>("/api/auth/code", "POST", args);
      case "complete_telegram_password": return await webFetch<T>("/api/auth/password", "POST", args);
      case "export_recovery_key": return (await webFetch<{ key: string }>("/api/recovery")).key as T;
      default: throw new Error("This action is available in the TiVault desktop window.");
    }
  } catch (error) {
    if (fallback) return fallback();
    throw error;
  }
}

function selectAndStageFiles(directory = false): Promise<{ paths: string[]; root?: string }> {
  return new Promise((resolve, reject) => {
    const input = document.createElement("input");
    input.type = "file"; input.multiple = true;
    if (directory) input.setAttribute("webkitdirectory", "");
    input.onchange = async () => {
      try {
        const selected = Array.from(input.files ?? []);
        const relative = selected[0]?.webkitRelativePath;
        const root = relative ? relative.split("/")[0] : undefined;
        const form = new FormData();
        for (const file of selected) form.append("files", file, file.name);
        if (!form.has("files")) return resolve({ paths: [], root });
        const base = window.location.port === "7468" ? "" : "http://127.0.0.1:7468";
        const response = await fetch(`${base}/api/stage`, { method: "POST", body: form });
        if (!response.ok) throw new Error(await response.text());
        resolve({ paths: await response.json() as string[], root });
      } catch (error) { reject(error); }
    };
    input.click();
  });
}

export const api = {
  requestNotificationPermission: async (): Promise<boolean> => {
    if (!isTauri()) {
      if (!("Notification" in window)) return false;
      return Notification.permission === "granted" || await Notification.requestPermission() === "granted";
    }
    const { isPermissionGranted, requestPermission } = await import("@tauri-apps/plugin-notification");
    return await isPermissionGranted() || await requestPermission() === "granted";
  },
  sendNotification: async (title: string, body: string): Promise<void> => {
    if (!isTauri()) {
      if ("Notification" in window && Notification.permission === "granted") new Notification(title, { body });
      return;
    }
    const { isPermissionGranted, sendNotification } = await import("@tauri-apps/plugin-notification");
    if (await isPermissionGranted()) sendNotification({ title, body });
  },
  installAvailableUpdate: async (): Promise<"current" | "installed"> => {
    if (!isTauri()) throw new Error("Signed updates are available only in the desktop app.");
    const { check } = await import("@tauri-apps/plugin-updater");
    const update = await check();
    if (!update) return "current";
    await update.downloadAndInstall();
    const { relaunch } = await import("@tauri-apps/plugin-process");
    await relaunch();
    return "installed";
  },
  availableUpdateVersion: async (): Promise<string | null> => {
    if (!isTauri()) return null;
    const { check } = await import("@tauri-apps/plugin-updater");
    return (await check())?.version ?? null;
  },
  lockStatus: () => call<LockStatus>("get_lock_status"),
  recordActivity: () => call<LockStatus>("record_activity"),
  unlockApp: (password: string) => call<LockStatus>("unlock_app", { password }),
  configureAppLock: (password: string) => call<LockStatus>("configure_app_lock", { password }),
  disableAppLock: (password: string) => call<LockStatus>("disable_app_lock", { password }),
  lockApp: () => call<LockStatus>("lock_app"),
  dashboard: () => call<Dashboard>("get_dashboard", undefined, webData),
  accountAvatar: (accountId: string) => call<string | null>("get_account_avatar", { accountId }),
  chooseFiles: async (): Promise<string[]> => {
    if (!isTauri()) return (await selectAndStageFiles()).paths;
    const { open } = await import("@tauri-apps/plugin-dialog");
    const selection = await open({ multiple: true, directory: false });
    return selection ? (Array.isArray(selection) ? selection : [selection]) : [];
  },
  chooseFolder: async (): Promise<string | null> => {
    if (!isTauri()) return window.prompt("Enter the full path of a folder on the computer running TiVault:");
    const { open } = await import("@tauri-apps/plugin-dialog");
    const selection = await open({ multiple: false, directory: true });
    return typeof selection === "string" ? selection : null;
  },
  chooseUploadFolder: async (): Promise<{ paths: string[]; root?: string }> => {
    if (!isTauri()) return selectAndStageFiles(true);
    const { open } = await import("@tauri-apps/plugin-dialog");
    const selection = await open({ multiple: false, directory: true });
    if (typeof selection !== "string") return { paths: [] };
    const paths = await invoke<string[]>("expand_upload_paths", { paths: [selection] });
    return { paths, root: selection };
  },
  expandUploadPaths: async (paths: string[]): Promise<string[]> => {
    if (!isTauri()) return paths;
    return invoke<string[]>("expand_upload_paths", { paths });
  },
  queueUploads: (options: UploadOptions) => call<VaultFile[]>("queue_uploads", { options }),
  dismissTransfer: (id: string) => call<void>("dismiss_transfer", { id }),
  dismissTransfers: (ids: string[]) => call<number>("dismiss_transfers", { ids }),
  clearTransferHistory: () => call<number>("clear_transfer_history"),
  pauseTransfer: (id: string) => call<void>("pause_transfer", { id }),
  resumeTransfer: (id: string) => call<void>("resume_transfer", { id }),
  cancelTransfer: (id: string) => call<void>("cancel_transfer", { id }),
  downloadFile: (id: string) => call<void>("download_file", { id }),
  renameFile: (id: string, newName: string) => call<VaultFile>("rename_file", { id, newName }),
  moveFile: (id: string, folderPath: string) => call<VaultFile>("move_file", { id, folderPath }),
  copyFile: (id: string, newName: string, folderPath: string) => call<VaultFile>("copy_file", { id, newName, folderPath }),
  startPreview: (id: string) => call<PreviewInfo>("start_preview", { id }),
  previewText: (token: string) => call<PreviewText>("preview_text", { token }),
  stopPreview: (token: string) => call<void>("stop_preview", { token }),
  lookupShareRecipient: (fileId: string, username: string) => call<ShareRecipient>("lookup_share_recipient", { fileId, username }),
  recentShareRecipients: (fileId: string) => call<ShareRecipient[]>("recent_share_recipients", { fileId }),
  shareFile: (fileId: string, recipientToken: string, allowDecrypt: boolean) => call<string>("share_file", { fileId, recipientToken, allowDecrypt }),
  lookupFolderShareRecipient: (path: string, username: string) => call<ShareRecipient>("lookup_folder_share_recipient", { path, username }),
  recentFolderShareRecipients: (path: string) => call<ShareRecipient[]>("recent_folder_share_recipients", { path }),
  shareFolder: (path: string, recipientToken: string, allowDecrypt: boolean) => call<string[]>("share_folder", { path, recipientToken, allowDecrypt }),
  createFolder: (parentPath: string, name: string) => call<VaultFolderRecord>("create_folder", { parentPath, name }),
  downloadFolder: (path: string) => call<number>("download_folder", { path }),
  deleteFolder: (path: string) => call<number>("delete_folder", { path }),
  deleteFile: (id: string) => call<void>("delete_file", { id }),
  deleteFiles: (ids: string[]) => call<number>("delete_files", { ids }),
  restoreFile: (id: string) => call<void>("restore_file", { id }),
  permanentlyDeleteFile: (id: string) => call<void>("permanently_delete_file", { id }),
  permanentlyDeleteFiles: (ids: string[]) => call<number>("permanently_delete_files", { ids }),
  emptyTrash: () => call<number>("empty_trash"),
  setFavorite: (id: string, favorite: boolean) => call<void>("set_file_favorite", { id, favorite }),
  setTags: (id: string, tags: string[]) => call<void>("set_file_tags", { id, tags }),
  disconnectAccount: (accountId: string) => call<void>("disconnect_account", { accountId }),
  removeAccount: (accountId: string) => call<void>("remove_account", { accountId }),
  revealFile: (id: string) => call<void>("reveal_cached_file", { id }),
  addWatchFolder: (folder: NewWatchFolder) => call<WatchFolder>("add_watch_folder", { folder }),
  removeWatchFolder: (id: string) => call<void>("remove_watch_folder", { id }),
  updateSettings: (settings: Record<string, unknown>) => call<Dashboard>("update_settings", { settings }),
  clearPreviewCache: () => call<number>("clear_preview_cache"),
  recoverVault: (accountId: string) => call<RecoveryReport>("recover_vault", { accountId }),
  testRecovery: (accountId: string, recoveryKey: string) => call<RecoveryTestReport>("test_recovery", { accountId, recoveryKey }),
  runHealthCheck: (accountId: string, sampleCount = 5) => call<HealthReport>("run_health_check", { accountId, sampleCount }),
  startLogin: (request: LoginRequest) => call<LoginResult>("start_telegram_login", { request }),
  startQrLogin: (request: LoginRequest) => call<LoginResult>("start_telegram_qr_login", { request }),
  pollQrLogin: (flowId: string) => call<LoginResult>("poll_telegram_qr_login", { flowId }),
  completeLogin: (flowId: string, code: string) => call<LoginResult>("complete_telegram_login", { flowId, code }),
  completePassword: (flowId: string, password: string) => call<LoginResult>("complete_telegram_password", { flowId, password }),
  exportRecovery: () => call<string>("export_recovery_key")
};
