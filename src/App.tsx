import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Archive, ArrowDownToLine, ArrowUpToLine, AudioLines, BadgeCheck, Check, CheckSquare2, ChevronLeft, ChevronRight, CircleAlert,
  Cloud, Copy, Download, Edit3, Eye, File, FileArchive, FileBox, FileText, Folder, FolderInput, FolderPlus, FolderSync, FolderUp, Gauge, Grid2X2,
  HardDrive, Image, Info, KeyRound, LayoutDashboard, Link2, List, Lock, LockKeyhole, Menu, MonitorDown,
  Activity, Bell, Clock3, History, LogOut, Moon, Pause, Play, Plus, RefreshCw, RotateCcw, Search, Send, Settings, Share2, ShieldCheck,
  Star, Sun, Tag, Trash2, Upload, UserRound, UserX, Users, Video, Wifi, WifiOff, X, Zap
} from "lucide-react";
import { api } from "./lib/api";
import { formatBytes, formatDate, formatEta } from "./lib/format";
import type { Account, Category, Dashboard, HealthReport, LockStatus, LoginResult, PreviewInfo, RecoveryReport, RecoveryTestReport, ShareRecipient, Transfer, VaultFile, VaultFolderRecord, WatchFolder } from "./lib/types";
import QRCode from "qrcode";
import { getDocument, GlobalWorkerOptions, type PDFDocumentLoadingTask, type PDFDocumentProxy } from "pdfjs-dist";
import pdfWorkerUrl from "pdfjs-dist/build/pdf.worker.min.mjs?url";

// Use TiVault's bundled worker rather than a browser PDF plug-in. Embedded web
// views do not consistently provide a PDF plug-in, particularly on Linux.
GlobalWorkerOptions.workerSrc = pdfWorkerUrl;

const categories: Array<{ name: Category; icon: typeof File; color: string }> = [
  { name: "All files", icon: FileBox, color: "#4b8cff" },
  { name: "Photos", icon: Image, color: "#a267ff" },
  { name: "Videos", icon: Video, color: "#ff5f75" },
  { name: "Audio", icon: AudioLines, color: "#ffad45" },
  { name: "Documents", icon: FileText, color: "#29b6f6" },
  { name: "Archives", icon: FileArchive, color: "#39c78c" },
  { name: "Applications", icon: MonitorDown, color: "#8091a7" },
  { name: "Other", icon: File, color: "#8091a7" }
];

type View = "vault" | "favorites" | "recent" | "trash" | "transfers" | "watch" | "accounts" | "settings" | "about";
type SortOrder = "newest" | "oldest" | "name" | "largest" | "smallest" | "type";
type UploadSelection = { paths: string[]; root?: string; destinationFolder?: string };
type VaultFolder = { path: string; name: string; fileCount: number; size: number; accountName: string; latestCreatedAt: string };
type ConfirmationOptions = { title: string; message: string; confirmLabel?: string; tone?: "danger" | "warning" };
type ConfirmAction = (options: ConfirmationOptions) => Promise<boolean>;
type PendingConfirmation = ConfirmationOptions & { resolve: (confirmed: boolean) => void };
type FileAction = "rename" | "move" | "copy";

function foldersAt(files: VaultFile[], storedFolders: VaultFolderRecord[], currentPath: string, includeStoredFolders: boolean): VaultFolder[] {
  const prefix = currentPath ? `${currentPath}/` : "";
  const folders = new Map<string, VaultFolder>();
  if (includeStoredFolders) {
    for (const folder of storedFolders) {
      if (!folder.path.startsWith(prefix)) continue;
      const rest = folder.path.slice(prefix.length);
      if (!rest) continue;
      const name = rest.split("/")[0];
      const childPath = prefix + name;
      const existing = folders.get(childPath) ?? { path: childPath, name, fileCount: 0, size: 0, accountName: "TiVault", latestCreatedAt: folder.createdAt };
      if (folder.createdAt > existing.latestCreatedAt) existing.latestCreatedAt = folder.createdAt;
      folders.set(childPath, existing);
    }
  }
  for (const file of files) {
    const path = file.folderPath ?? "";
    if (!path.startsWith(prefix)) continue;
    const rest = path.slice(prefix.length);
    if (!rest) continue;
    const name = rest.split("/")[0];
    const childPath = prefix + name;
    const existing = folders.get(childPath) ?? { path: childPath, name, fileCount: 0, size: 0, accountName: file.accountName, latestCreatedAt: file.createdAt };
    existing.fileCount += 1;
    existing.size += file.size;
    if (existing.accountName === "TiVault") existing.accountName = file.accountName;
    if (file.createdAt > existing.latestCreatedAt) existing.latestCreatedAt = file.createdAt;
    folders.set(childPath, existing);
  }
  return [...folders.values()].sort((a, b) => a.name.localeCompare(b.name));
}

async function withDeadline<T>(operation: Promise<T>, milliseconds: number, message: string): Promise<T> {
  let timer = 0;
  const deadline = new Promise<never>((_, reject) => {
    timer = window.setTimeout(() => reject(new Error(message)), milliseconds);
  });
  try {
    return await Promise.race([operation, deadline]);
  } finally {
    window.clearTimeout(timer);
  }
}

function fileVisual(file: VaultFile) {
  return categories.find((item) => item.name === file.category) ?? categories[7];
}

function Skeleton() {
  return <div className="loading-screen"><img src="/assets/tivault-logo.png" /><div className="loader" /><p>Opening your vault…</p></div>;
}

function ConfirmationModal({ confirmation, onAnswer }: { confirmation: PendingConfirmation; onAnswer: (confirmed: boolean) => void }) {
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => { if (event.key === "Escape") onAnswer(false); };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onAnswer]);
  return <div className="modal-backdrop confirm-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) onAnswer(false); }}>
    <section className="confirm-modal" role="alertdialog" aria-modal="true" aria-labelledby="confirm-title" aria-describedby="confirm-message">
      <div className={`confirm-icon ${confirmation.tone ?? "danger"}`}><CircleAlert size={28} /></div>
      <div className="confirm-copy"><span className="eyebrow">CONFIRM ACTION</span><h2 id="confirm-title">{confirmation.title}</h2><p id="confirm-message">{confirmation.message}</p></div>
      <div className="modal-actions"><button className="button ghost" autoFocus onClick={() => onAnswer(false)}>Cancel</button><button className={`button ${confirmation.tone === "warning" ? "warning" : "danger-solid"}`} onClick={() => onAnswer(true)}>{confirmation.tone === "warning" ? <Check size={16} /> : <Trash2 size={16} />} {confirmation.confirmLabel ?? "Delete permanently"}</button></div>
    </section>
  </div>;
}

function CreateFolderModal({ parentPath, onClose, onCreate }: { parentPath: string; onClose: () => void; onCreate: (name: string) => Promise<void> }) {
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!name.trim() || busy) return;
    setBusy(true); setError("");
    try { await onCreate(name.trim()); onClose(); }
    catch (cause) { setError(String(cause)); setBusy(false); }
  };
  return <div className="modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
    <form className="modal create-folder-modal" onSubmit={submit}>
      <div className="modal-head"><div><span className="eyebrow">NEW FOLDER</span><h2>Create a folder</h2></div><button type="button" className="icon-button" onClick={onClose}><X /></button></div>
      <div className="create-folder-destination"><Folder size={18} /><span><small>LOCATION</small><strong>{parentPath || "My TiVault"}</strong></span></div>
      <label className="field"><span>Folder name</span><input autoFocus value={name} maxLength={255} placeholder="Untitled folder" onChange={(event) => setName(event.target.value)} /></label>
      {error && <div className="notice danger"><CircleAlert size={17} /> {error}</div>}
      <div className="modal-actions"><button type="button" className="button ghost" onClick={onClose}>Cancel</button><button className="button primary" disabled={!name.trim() || busy}>{busy ? <RefreshCw className="spin" size={17} /> : <FolderPlus size={17} />} Create folder</button></div>
    </form>
  </div>;
}

function EmptyState({ category, onUpload }: { category: Category; onUpload: () => void }) {
  return (
    <div className="empty-state">
      <div className="empty-cloud"><Cloud size={34} /><Plus size={18} /></div>
      <h3>No {category === "All files" ? "files" : category.toLowerCase()} here yet</h3>
      <p>Drop files anywhere or choose them from your device. TiVault handles chunking automatically.</p>
      <button className="button primary" onClick={onUpload}><Upload size={17} /> Upload files</button>
    </div>
  );
}

function PdfPreview({ url, name, size }: { url: string; name: string; size: number }) {
  const canvas = useRef<HTMLCanvasElement | null>(null);
  const loadingTaskRef = useRef<PDFDocumentLoadingTask | null>(null);
  const [document, setDocument] = useState<PDFDocumentProxy | null>(null);
  const [pageNumber, setPageNumber] = useState(1);
  const [error, setError] = useState("");

  useEffect(() => {
    const controller = new AbortController();
    let cancelled = false;
    setDocument(null); setPageNumber(1); setError("");
    if (size > 128 * 1024 * 1024) {
      setError("This PDF is larger than 128 MB, so TiVault will not load it into memory for an inline preview. Download it to open it safely.");
      return () => controller.abort();
    }
    void (async () => {
      try {
        const response = await fetch(url, { signal: controller.signal, credentials: "omit" });
        if (!response.ok) throw new Error(`TiVault could not load this PDF (${response.status}).`);
        const bytes = new Uint8Array(await response.arrayBuffer());
        if (cancelled) return;
        const task = getDocument({ data: bytes, disableRange: true, disableStream: true, disableAutoFetch: true, enableXfa: false });
        loadingTaskRef.current = task;
        const loaded = await task.promise;
        if (cancelled) return;
        setDocument(loaded);
      } catch (cause) {
        if (!cancelled && !controller.signal.aborted) {
          setError(cause instanceof Error ? cause.message : "TiVault could not render this PDF.");
        }
      }
    })();
    return () => {
      cancelled = true;
      controller.abort();
      const task = loadingTaskRef.current;
      loadingTaskRef.current = null;
      if (task) void task.destroy();
    };
  }, [size, url]);

  useEffect(() => {
    if (!document || error) return;
    let cancelled = false;
    let renderTask: ReturnType<Awaited<ReturnType<PDFDocumentProxy["getPage"]>>["render"]> | null = null;
    void document.getPage(pageNumber).then((page) => {
      if (cancelled || !canvas.current) return;
      const deviceScale = Math.min(window.devicePixelRatio || 1, 2);
      const viewport = page.getViewport({ scale: 1.2 * deviceScale });
      const element = canvas.current;
      element.width = Math.ceil(viewport.width);
      element.height = Math.ceil(viewport.height);
      element.style.width = `${Math.ceil(viewport.width / deviceScale)}px`;
      element.style.height = `${Math.ceil(viewport.height / deviceScale)}px`;
      renderTask = page.render({ canvas: element, viewport });
      return renderTask.promise;
    }).catch((cause) => {
      if (!cancelled) setError(cause instanceof Error ? cause.message : "TiVault could not render this PDF page.");
    });
    return () => {
      cancelled = true;
      renderTask?.cancel();
    };
  }, [document, error, pageNumber]);

  if (error) return <div className="preview-empty"><CircleAlert size={38} /><h3>PDF preview unavailable</h3><p>{error}</p></div>;
  if (!document) return <div className="preview-empty"><RefreshCw className="spin" size={38} /><h3>Preparing PDF preview</h3><p>The PDF is processed locally in TiVault. No file is sent to a preview service.</p></div>;
  return <div className="inline-pdf-renderer">
    <div className="pdf-page-controls"><button className="icon-button" aria-label="Previous PDF page" disabled={pageNumber <= 1} onClick={() => setPageNumber((current) => Math.max(1, current - 1))}><ChevronLeft size={19} /></button><span>Page {pageNumber} of {document.numPages}</span><button className="icon-button" aria-label="Next PDF page" disabled={pageNumber >= document.numPages} onClick={() => setPageNumber((current) => Math.min(document.numPages, current + 1))}><ChevronRight size={19} /></button></div>
    <div className="pdf-canvas-scroll"><canvas ref={canvas} aria-label={`Page ${pageNumber} of ${name}`} /></div>
  </div>;
}

function LazyFileThumbnail({ file, compact = false }: { file: VaultFile; compact?: boolean }) {
  const host = useRef<HTMLSpanElement | null>(null);
  const [visible, setVisible] = useState(false);
  const [info, setInfo] = useState<PreviewInfo | null>(null);
  const [textPreview, setTextPreview] = useState("");
  const [failed, setFailed] = useState(false);
  const visual = fileVisual(file); const Icon = visual.icon;
  const isPdf = file.mimeType === "application/pdf" || /\.pdf$/i.test(file.name);
  useEffect(() => {
    const node = host.current;
    if (!node) return;
    const observer = new IntersectionObserver(([entry]) => setVisible(entry.isIntersecting), { rootMargin: "160px" });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  useEffect(() => {
    if (!visible || file.thumbnail || failed || isPdf) return;
    let cancelled = false;
    let token = "";
    void api.startPreview(file.id).then((preview) => {
      token = preview.token;
      if (cancelled) { void api.stopPreview(preview.token); return; }
      setInfo(preview);
      if (!compact && (preview.kind === "text" || preview.kind === "document")) {
        void api.previewText(preview.token).then((result) => { if (!cancelled) setTextPreview(result.content.trim().slice(0, 280)); }).catch(() => undefined);
      }
    }).catch(() => { if (!cancelled) setFailed(true); });
    return () => { cancelled = true; if (token) void api.stopPreview(token); };
  }, [compact, failed, file.id, file.thumbnail, isPdf, visible]);
  const className = compact ? "lazy-thumbnail compact" : "lazy-thumbnail";
  if (file.thumbnail) return <span ref={host} className={className}><img src={file.thumbnail} alt="" /></span>;
  if (!failed && info?.kind === "image") return <span ref={host} className={className}><img src={info.url} alt="" onError={() => setFailed(true)} /></span>;
  if (!failed && info?.kind === "video") return <span ref={host} className={className}><video src={info.url} muted playsInline preload="metadata" onLoadedMetadata={(event) => { if (Number.isFinite(event.currentTarget.duration)) event.currentTarget.currentTime = Math.min(0.2, event.currentTarget.duration / 2); }} onError={() => setFailed(true)} /></span>;
  if (isPdf) return <span ref={host} className={`${className} document-thumbnail`}><FileText size={28} /><small>PDF document</small></span>;
  if (!compact && (info?.kind === "text" || info?.kind === "document")) return <span ref={host} className={`${className} document-thumbnail`}><FileText size={28} /><small>{textPreview || file.name}</small></span>;
  return <span ref={host} className={className} style={{ color: visual.color }}>{visible && !failed && !info ? <RefreshCw className="spin" size={compact ? 16 : 28} /> : <Icon size={compact ? 19 : 46} strokeWidth={1.5} />}</span>;
}

function FileCard({ file, active, selectionMode, checked, onSelect, onToggleSelection, onDownload, onDelete, onFavorite }: { file: VaultFile; active: boolean; selectionMode: boolean; checked: boolean; onSelect: () => void; onToggleSelection: () => void; onDownload: () => void; onDelete: () => void; onFavorite: () => void }) {
  const visual = fileVisual(file);
  return (
    <article className={`file-card ${active || checked ? "selected" : ""} ${selectionMode ? "selection-mode" : ""}`} onClick={selectionMode ? onToggleSelection : onSelect} tabIndex={0}>
      <div className="file-preview" style={{ "--file-color": visual.color } as React.CSSProperties}>
        {selectionMode && <button className={`selection-check ${checked ? "checked" : ""}`} aria-label={`${checked ? "Deselect" : "Select"} ${file.name}`} onClick={(event) => { event.stopPropagation(); onToggleSelection(); }}>{checked && <Check size={14} />}</button>}
        <LazyFileThumbnail file={file} />
        <div className="file-badges">
          {file.favorite && <span><Star size={12} fill="currentColor" /> Favourite</span>}
          {file.encrypted && <span title="Client-side encrypted"><LockKeyhole size={12} /> Encrypted</span>}
          {file.chunkCount > 1 && <span>{file.chunkCount} parts</span>}
        </div>
      </div>
      <div className="file-card-info">
        <div><h4 title={file.name}>{file.name}</h4><p>{formatBytes(file.size)} · {formatDate(file.createdAt)}</p></div>
        <div className="quick-actions">
          <button className={`icon-button ${file.favorite ? "favourite" : ""}`} title={file.favorite ? "Remove from favourites" : "Add to favourites"} onClick={(event) => { event.stopPropagation(); onFavorite(); }}><Star size={15} fill={file.favorite ? "currentColor" : "none"} /></button>
          <button className="icon-button" title="Download" onClick={(event) => { event.stopPropagation(); onDownload(); }}><Download size={15} /></button>
          <button className="icon-button danger" title="Move to Recycle Bin" onClick={(event) => { event.stopPropagation(); onDelete(); }}><Trash2 size={15} /></button>
        </div>
      </div>
    </article>
  );
}

function FileRow({ file, active, selectionMode, checked, onSelect, onToggleSelection, onDownload, onDelete, onFavorite }: { file: VaultFile; active: boolean; selectionMode: boolean; checked: boolean; onSelect: () => void; onToggleSelection: () => void; onDownload: () => void; onDelete: () => void; onFavorite: () => void }) {
  const visual = fileVisual(file);
  return (
    <div className={`file-row ${active || checked ? "selected" : ""} ${selectionMode ? "selection-mode" : ""}`} onClick={selectionMode ? onToggleSelection : onSelect}>
      <div className="row-name">{selectionMode && <button className={`selection-check inline ${checked ? "checked" : ""}`} aria-label={`${checked ? "Deselect" : "Select"} ${file.name}`} onClick={(event) => { event.stopPropagation(); onToggleSelection(); }}>{checked && <Check size={14} />}</button>}<span className="small-file-icon" style={{ color: visual.color }}><LazyFileThumbnail file={file} compact /></span><span><strong>{file.name}</strong><small>{file.mimeType}</small></span></div>
      <span>{file.category}</span><span>{formatBytes(file.size)}</span><span>{file.accountName}</span><span>{formatDate(file.createdAt)}</span>
      <span className="row-status">{file.encrypted && <LockKeyhole size={13} />} {file.chunkCount > 1 ? `${file.chunkCount} parts` : "Ready"}</span>
      <div className="row-actions"><button className={`icon-button ${file.favorite ? "favourite" : ""}`} title={file.favorite ? "Remove from favourites" : "Add to favourites"} onClick={(e) => { e.stopPropagation(); onFavorite(); }}><Star size={15} fill={file.favorite ? "currentColor" : "none"} /></button><button className="icon-button" title="Download" onClick={(e) => { e.stopPropagation(); onDownload(); }}><Download size={15} /></button><button className="icon-button danger" title="Move to Recycle Bin" onClick={(e) => { e.stopPropagation(); onDelete(); }}><Trash2 size={15} /></button></div>
    </div>
  );
}

function FolderCard({ folder, onOpen, onDownload, onShare, onDelete }: { folder: VaultFolder; onOpen: () => void; onDownload: () => void; onShare: () => void; onDelete: () => void }) {
  return <article className="file-card folder-card" onClick={onOpen} tabIndex={0} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") onOpen(); }}>
    <div className="file-preview folder-preview"><Folder size={62} strokeWidth={1.25} /><span className="folder-count">{folder.fileCount}</span></div>
    <div className="file-card-info"><div><h4>{folder.name}</h4><p>{folder.fileCount} {folder.fileCount === 1 ? "file" : "files"} · {formatBytes(folder.size)}</p></div><div className="folder-actions"><button className="icon-button" title="Share folder via Telegram" aria-label={`Share folder ${folder.name}`} onClick={(event) => { event.stopPropagation(); onShare(); }}><Share2 size={15} /></button><button className="icon-button" title="Download entire folder" aria-label={`Download folder ${folder.name}`} onClick={(event) => { event.stopPropagation(); onDownload(); }}><Download size={15} /></button><button className="icon-button danger" title="Move folder to Recycle Bin" aria-label={`Move folder ${folder.name} to Recycle Bin`} onClick={(event) => { event.stopPropagation(); onDelete(); }}><Trash2 size={15} /></button><ChevronRight size={17} className="folder-chevron" /></div></div>
  </article>;
}

function FolderRow({ folder, onOpen, onDownload, onShare, onDelete }: { folder: VaultFolder; onOpen: () => void; onDownload: () => void; onShare: () => void; onDelete: () => void }) {
  return <div className="file-row folder-row" onClick={onOpen} tabIndex={0} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") onOpen(); }}>
    <div className="row-name"><span className="small-file-icon folder-small"><Folder size={19} /></span><span><strong>{folder.name}</strong><small>{folder.fileCount} {folder.fileCount === 1 ? "file" : "files"}</small></span></div>
    <span>Folder</span><span>{formatBytes(folder.size)}</span><span>{folder.accountName}</span><span>{formatDate(folder.latestCreatedAt)}</span><span className="row-status"><Folder size={13} /> Open</span><div className="row-actions"><button className="icon-button" title="Share folder via Telegram" aria-label={`Share folder ${folder.name}`} onClick={(event) => { event.stopPropagation(); onShare(); }}><Share2 size={15} /></button><button className="icon-button" title="Download entire folder" aria-label={`Download folder ${folder.name}`} onClick={(event) => { event.stopPropagation(); onDownload(); }}><Download size={15} /></button><button className="icon-button danger" title="Move folder to Recycle Bin" aria-label={`Move folder ${folder.name} to Recycle Bin`} onClick={(event) => { event.stopPropagation(); onDelete(); }}><Trash2 size={15} /></button><ChevronRight size={16} className="folder-chevron" /></div>
  </div>;
}

function TransferItem({ transfer, selectionMode, checked, onToggleSelection, onPause, onResume, onCancel, onDismiss }: { transfer: Transfer; selectionMode: boolean; checked: boolean; onToggleSelection: () => void; onPause: () => void; onResume: () => void; onCancel: () => void; onDismiss: () => void }) {
  const active = transfer.state === "uploading" || transfer.state === "downloading" || transfer.state === "preparing";
  const waiting = transfer.state === "waiting";
  const history = transfer.state === "complete" || transfer.state === "failed";
  const canPause = (active || waiting) && !(transfer.direction === "share" && transfer.state === "uploading");
  return (
    <div className={`transfer-item ${selectionMode ? "selection-mode" : ""} ${checked ? "selected" : ""}`}>
      {selectionMode && (history ? <button className={`selection-check inline ${checked ? "checked" : ""}`} aria-label={`${checked ? "Deselect" : "Select"} ${transfer.fileName} transfer`} onClick={onToggleSelection}>{checked && <Check size={14} />}</button> : <span className="selection-check-spacer" />)}
      <div className={`transfer-direction ${transfer.direction}`}>
        {transfer.direction === "upload" ? <ArrowUpToLine size={18} /> : transfer.direction === "share" ? <Send size={17} /> : <ArrowDownToLine size={18} />}
      </div>
      <div className="transfer-main">
        <div className="transfer-title"><strong>{transfer.fileName}</strong><span>{transfer.encrypted && <LockKeyhole size={12} />} {transfer.state}</span></div>
        <div className="progress"><span style={{ width: `${Math.max(0, Math.min(1, transfer.progress)) * 100}%` }} /></div>
        <div className="transfer-meta"><span>{formatBytes(transfer.transferred)} of {formatBytes(transfer.total)}</span><span>{active && transfer.speed > 0 ? `${formatBytes(transfer.speed)}/s · ${formatEta(transfer.etaSeconds)}` : transfer.message ?? transfer.state}</span></div>
      </div>
      <div className="transfer-controls">
        {canPause ? <button className="icon-button" onClick={onPause}><Pause size={16} /></button> : transfer.state === "paused" || (transfer.state === "failed" && transfer.direction !== "share") ? <button className="icon-button" onClick={onResume}><Play size={16} /></button> : null}
        {transfer.state !== "complete" && <button className="icon-button" title="Cancel and remove transfer" onClick={onCancel}><X size={16} /></button>}
        {transfer.state === "complete" && <><Check size={17} className="success" /><button className="icon-button" title="Remove completed transfer from history" aria-label={`Remove ${transfer.fileName} from transfer history`} onClick={onDismiss}><X size={16} /></button></>}
      </div>
    </div>
  );
}

function UploadModal({ accounts, paths: initialPaths, folderRoot, destinationFolder, onClose, onQueued }: { accounts: Account[]; paths: string[]; folderRoot?: string; destinationFolder?: string; onClose: () => void; onQueued: () => void }) {
  const [paths, setPaths] = useState(initialPaths);
  const [encrypt, setEncrypt] = useState(true);
  const [skipDuplicates, setSkipDuplicates] = useState(true);
  const [accountId, setAccountId] = useState(accounts[0]?.id ?? "");
  const [busy, setBusy] = useState(false);
  const rootPrefix = folderRoot ? `${folderRoot.replace(/[\\/]+$/, "")}/` : "";
  const names = paths.map((path) => rootPrefix && path.startsWith(rootPrefix) ? path.slice(rootPrefix.length) : path.split(/[\\/]/).pop() ?? path);
  const folderName = folderRoot?.split(/[\\/]/).filter(Boolean).pop();
  const chooseMore = async () => {
    const more = await api.chooseFiles();
    setPaths((current) => [...current, ...more]);
  };
  const queue = async () => {
    if (!paths.length || !accountId) return;
    setBusy(true);
    try { await api.queueUploads({ paths, folderRoot, destinationFolder, encrypt, accountId, duplicatePolicy: skipDuplicates ? "skip" : "keep" }); onQueued(); onClose(); }
    catch (error) { alert(String(error)); setBusy(false); }
  };
  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div className="modal upload-modal" onMouseDown={(e) => e.stopPropagation()}>
        <div className="modal-head"><div><span className="eyebrow">NEW TRANSFER</span><h2>Upload to TiVault</h2></div><button className="icon-button" onClick={onClose}><X /></button></div>
        {folderName && <div className="notice"><FolderUp size={17} /><span><strong>Folder upload: {folderName}</strong><br />{paths.length} {paths.length === 1 ? "file was" : "files were"} found recursively. Symlinks are skipped for safety.</span></div>}
        {destinationFolder && <div className="notice"><Folder size={17} /><span><strong>Upload destination</strong><br />{destinationFolder}</span></div>}
        <div className="upload-list">
          {names.length ? names.slice(0, 200).map((name, index) => <div key={`${paths[index]}-${index}`}><span className="small-file-icon"><File size={18} /></span><span><strong>{name}</strong><small>Size is checked before transfer</small></span><button className="icon-button" onClick={() => setPaths(paths.filter((_, i) => i !== index))}><X size={14} /></button></div>) : <button className="drop-mini" onClick={chooseMore}><Upload /><span><strong>Choose files</strong><small>Files of any size are supported</small></span></button>}
        </div>
        {names.length > 200 && <p className="upload-overflow">Plus {names.length - 200} more files from this folder</p>}
        {paths.length > 0 && <button className="text-button" onClick={chooseMore}><Plus size={15} /> Add more files</button>}
        <div className="option-card">
          <div className="option-icon"><ShieldCheck /></div><div><strong>Client-side encryption</strong><p>Encrypt content before it leaves this device. Filename privacy can be changed in Settings.</p></div>
          <label className="switch"><input type="checkbox" checked={encrypt} onChange={(e) => setEncrypt(e.target.checked)} /><span /></label>
        </div>
        <div className="option-card">
          <div className="option-icon"><Copy /></div><div><strong>Skip exact duplicates</strong><p>When a same-size candidate exists, TiVault hashes it locally and skips the upload only when the content is identical.</p></div>
          <label className="switch"><input type="checkbox" checked={skipDuplicates} onChange={(e) => setSkipDuplicates(e.target.checked)} /><span /></label>
        </div>
        <label className="field"><span>Telegram account</span><select value={accountId} onChange={(e) => setAccountId(e.target.value)}>{accounts.map((account) => <option value={account.id} key={account.id}>{account.name} · {account.phone}</option>)}</select></label>
        {!accounts.length && <div className="notice warning"><CircleAlert size={17} /> Connect a Telegram account before uploading.</div>}
        <div className="modal-actions"><button className="button ghost" onClick={onClose}>Cancel</button><button className="button primary" disabled={!paths.length || !accountId || busy} onClick={queue}>{busy ? <RefreshCw className="spin" size={17} /> : <Upload size={17} />} Queue {paths.length || ""} {paths.length === 1 ? "file" : "files"}</button></div>
      </div>
    </div>
  );
}

function AccountModal({ account, onClose, onConnected }: { account?: Account; onClose: () => void; onConnected: () => void }) {
  const [step, setStep] = useState<"details" | "code" | "qr" | "password" | "done">("details");
  const [form, setForm] = useState({ name: account?.name ?? "Personal", phone: "", apiId: "", apiHash: "", code: "", password: "" });
  const [flow, setFlow] = useState<LoginResult | null>(null);
  const [qrImage, setQrImage] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const update = (key: keyof typeof form, value: string) => setForm((current) => ({ ...current, [key]: value }));
  useEffect(() => {
    if (step !== "qr" || !flow?.qrUrl) return;
    QRCode.toDataURL(flow.qrUrl, { width: 260, margin: 2, errorCorrectionLevel: "M" })
      .then(setQrImage)
      .catch((cause) => setError(String(cause)));
  }, [step, flow?.qrUrl]);
  useEffect(() => {
    if (step !== "qr" || !flow?.flowId) return;
    let cancelled = false;
    let timer = 0;
    const poll = async () => {
      try {
        const result = await withDeadline(api.pollQrLogin(flow.flowId), 35_000, "Telegram stopped responding while checking the QR code.");
        if (cancelled) return;
        setFlow(result);
        if (result.status === "connected") { setStep("done"); onConnected(); return; }
        if (result.status === "password_required") { setStep("password"); return; }
        timer = window.setTimeout(poll, 2500);
      } catch (cause) {
        if (!cancelled) setError(String(cause));
      }
    };
    timer = window.setTimeout(poll, 2500);
    return () => { cancelled = true; window.clearTimeout(timer); };
  }, [step, flow?.flowId, onConnected]);
  const request = account
    ? { accountId: account.id, name: account.name, phone: "", apiId: 0, apiHash: "" }
    : { name: form.name, phone: form.phone, apiId: Number(form.apiId), apiHash: form.apiHash };
  const start = async () => {
    setBusy(true); setError("");
    try {
      const result = await withDeadline(
        api.startLogin(request),
        35_000,
        "Telegram did not respond. Check your internet connection, VPN or proxy, and verify your API credentials before trying again."
      );
      setFlow(result);
      if (result.status === "connected") { setStep("done"); onConnected(); }
      else if (result.status === "password_required") setStep("password");
      else setStep("code");
    }
    catch (e) { setError(String(e)); } finally { setBusy(false); }
  };
  const startQr = async () => {
    setBusy(true); setError(""); setQrImage("");
    try {
      const result = await withDeadline(
        api.startQrLogin(request),
        35_000,
        "Telegram did not respond while creating the QR code."
      );
      setFlow(result);
      if (result.status === "connected") { setStep("done"); onConnected(); } else setStep("qr");
    }
    catch (e) { setError(String(e)); } finally { setBusy(false); }
  };
  const code = async () => {
    if (!flow) return; setBusy(true); setError("");
    try { const result = await api.completeLogin(flow.flowId, form.code); setFlow(result); if (result.status === "password_required") setStep("password"); else { setStep("done"); onConnected(); } }
    catch (e) { setError(String(e)); } finally { setBusy(false); }
  };
  const password = async () => {
    if (!flow) return; setBusy(true); setError("");
    try { await api.completePassword(flow.flowId, form.password); setStep("done"); onConnected(); }
    catch (e) { setError(String(e)); } finally { setBusy(false); }
  };
  return (
    <div className="modal-backdrop" onMouseDown={onClose}><div className="modal account-modal" onMouseDown={(e) => e.stopPropagation()}>
      <div className="modal-head"><div><span className="eyebrow">SECURE SIGN-IN</span><h2>{account ? `Reconnect ${account.name}` : "Connect Telegram"}</h2></div><button className="icon-button" onClick={onClose}><X /></button></div>
      {step === "details" && <><p className="modal-copy">{account ? "TiVault will reuse this vault’s protected phone number and Telegram API credentials. Reconnecting keeps every existing file attached to the same account." : "Use your own Telegram application credentials. TiVault stores the session only on this device and never asks for your Telegram password outside this sign-in window."}</p>
        {!account && <><div className="field-grid"><label className="field"><span>Profile name</span><input value={form.name} onChange={(e) => update("name", e.target.value)} /></label><label className="field"><span>Phone number</span><input placeholder="+44…" value={form.phone} onChange={(e) => update("phone", e.target.value)} /></label></div>
        <label className="field"><span>API ID</span><input inputMode="numeric" placeholder="From my.telegram.org" value={form.apiId} onChange={(e) => update("apiId", e.target.value)} /></label>
        <label className="field"><span>API hash</span><input type="password" value={form.apiHash} onChange={(e) => update("apiHash", e.target.value)} /></label></>}
        <div className="notice"><ShieldCheck size={17} /> Credentials remain in TiVault’s protected local application data.</div>
        {busy && <div className="notice"><RefreshCw className="spin" size={17} /> Connecting to Telegram… This can take up to 30 seconds.</div>}
        {error && <div className="notice danger">{error}</div>}<div className="modal-actions"><button className="button ghost" onClick={onClose}>Cancel</button><button className="button ghost" disabled={busy || (!account && (!form.phone || !form.apiId || !form.apiHash))} onClick={startQr}>Use QR code</button><button className="button primary" disabled={busy || (!account && (!form.phone || !form.apiId || !form.apiHash))} onClick={start}>{busy ? <RefreshCw className="spin" size={17} /> : null} {busy ? "Connecting…" : "Send login code"}</button></div></>}
      {step === "code" && <div className="auth-step"><div className="auth-art"><img src="/assets/tivault-logo.png" /></div><h3>Enter the Telegram code</h3><p>Telegram sent a confirmation code to your active Telegram session or phone.</p><input className="code-input" autoFocus maxLength={8} value={form.code} onChange={(e) => update("code", e.target.value)} />{error && <div className="notice danger">{error}</div>}<button className="button primary wide" disabled={busy || !form.code} onClick={code}>Continue</button></div>}
      {step === "qr" && <div className="auth-step qr-step"><h3>Scan with Telegram</h3><p>On your phone, open Telegram → Settings → Devices → Link Desktop Device, then scan this code.</p>{qrImage ? <img className="qr-code" src={qrImage} alt="Telegram login QR code" /> : <div className="qr-loading"><RefreshCw className="spin" /></div>}{error && <div className="notice danger">{error}</div>}<p className="qr-waiting"><RefreshCw className="spin" size={14} /> Waiting for approval…</p></div>}
      {step === "password" && <div className="auth-step"><div className="auth-art"><KeyRound /></div><h3>Two-step verification</h3><p>{flow?.hint ? `Password hint: ${flow.hint}` : "Enter your Telegram two-step verification password."}</p><input className="password-input" type="password" autoFocus value={form.password} onChange={(e) => update("password", e.target.value)} />{error && <div className="notice danger">{error}</div>}<button className="button primary wide" disabled={busy || !form.password} onClick={password}>Unlock account</button></div>}
      {step === "done" && <div className="auth-step"><div className="success-orb"><Check /></div><h3>Account connected</h3><p>TiVault can now store files in this account’s Saved Messages.</p><button className="button primary wide" onClick={onClose}>Open vault</button></div>}
    </div></div>
  );
}

function FilePreviewModal({ file, onClose, onDownload, onPrevious, onNext, playing, onTogglePlay }: { file: VaultFile; onClose: () => void; onDownload: () => void; onPrevious?: () => void; onNext?: () => void; playing?: boolean; onTogglePlay?: () => void }) {
  const [info, setInfo] = useState<PreviewInfo | null>(null);
  const [text, setText] = useState("");
  const [textTruncated, setTextTruncated] = useState(false);
  const [error, setError] = useState("");
  const [mediaError, setMediaError] = useState("");
  const tokenRef = useRef("");
  useEffect(() => {
    let cancelled = false;
    setInfo(null); setText(""); setTextTruncated(false); setError(""); setMediaError(""); tokenRef.current = "";
    void api.startPreview(file.id).then((result) => {
      if (cancelled) { void api.stopPreview(result.token); return; }
      tokenRef.current = result.token;
      setInfo(result);
      if (result.kind === "text" || result.kind === "document") {
        void api.previewText(result.token)
          .then((preview) => { if (!cancelled) { setText(preview.content); setTextTruncated(preview.truncated); } })
          .catch((cause) => { if (!cancelled) setError(String(cause)); });
      }
    }).catch((cause) => { if (!cancelled) setError(String(cause)); });
    return () => {
      cancelled = true;
      if (tokenRef.current) void api.stopPreview(tokenRef.current);
    };
  }, [file.id]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if (event.key === "Escape") onClose(); else if (event.key === "ArrowLeft") onPrevious?.(); else if (event.key === "ArrowRight") onNext?.(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, onNext, onPrevious]);
  const renderContent = () => {
    if (error) return <div className="preview-empty"><CircleAlert size={38} /><h3>Preview unavailable</h3><p>{error}</p></div>;
    if (!info) return <div className="preview-empty"><RefreshCw className="spin" size={38} /><h3>Preparing secure preview</h3><p>Only the requested portions are downloaded and decrypted locally.</p></div>;
    if (mediaError) return <div className="preview-empty"><CircleAlert size={38} /><h3>This media cannot be played here</h3><p>{mediaError}</p></div>;
    if (info.kind === "image") return <img className="inline-image-preview" src={info.url} alt={file.name} onError={() => setMediaError("The image format may not be supported by the system web view. Download it to open externally.")} />;
    if (info.kind === "video") return <video className="inline-video-preview" src={info.url} controls preload="metadata" playsInline onError={() => setMediaError("The video codec or container is not supported by the system player. Download it to open externally.")} />;
    if (info.kind === "audio") return <div className="inline-audio-preview"><AudioLines size={52} /><audio src={info.url} controls preload="metadata" onError={() => setMediaError("The audio codec is not supported by the system player.")} /></div>;
    if (info.kind === "pdf") return <PdfPreview url={info.url} name={file.name} size={info.size} />;
    if (info.kind === "text" || info.kind === "document") {
      if (!text && !error) return <div className="preview-empty"><RefreshCw className="spin" size={38} /><h3>Extracting safe text</h3><p>Scripts and document macros are not executed.</p></div>;
      return <div className="inline-text-wrap">{textTruncated && <div className="notice warning"><CircleAlert size={16} /> The inline preview is truncated. Download the file to read everything.</div>}<pre className="inline-text-preview">{text || "This document contains no extractable text."}</pre></div>;
    }
    return <div className="preview-empty"><Eye size={42} /><h3>No safe inline preview</h3><p>{info.message}</p></div>;
  };
  return <div className="modal-backdrop file-preview-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
    <section className="file-preview-modal" role="dialog" aria-modal="true" aria-label={`Preview ${file.name}`}>
      <header className="file-preview-toolbar"><div><span className="eyebrow">LOCAL SECURE PREVIEW</span><strong>{file.name}</strong><small>{formatBytes(file.size)}{file.encrypted ? " · decrypted only on this device" : ""}</small></div><div className="file-preview-toolbar-actions">{onPrevious && <button className="icon-button" aria-label="Previous photo" onClick={onPrevious}><ChevronLeft size={19} /></button>}{onTogglePlay && <button className="icon-button" aria-label={playing ? "Pause slideshow" : "Play slideshow"} onClick={onTogglePlay}>{playing ? <Pause size={18} /> : <Play size={18} />}</button>}{onNext && <button className="icon-button" aria-label="Next photo" onClick={onNext}><ChevronRight size={19} /></button>}<button className="button ghost compact" onClick={onDownload}><Download size={15} /> Download</button><button className="icon-button" aria-label="Close preview" onClick={onClose}><X size={19} /></button></div></header>
      <div className="file-preview-stage">{renderContent()}</div>
      <footer className="file-preview-footer"><span><ShieldCheck size={14} /> Loopback only · no external preview service</span>{info?.kind === "video" && <span>8 MB authenticated blocks · cache capped at {formatBytes(info.cacheLimit)}</span>}</footer>
    </section>
  </div>;
}

function ShareModal({ file, folder, folderFiles = [], confirmAction, onClose, onQueued }: { file?: VaultFile; folder?: VaultFolder; folderFiles?: VaultFile[]; confirmAction: ConfirmAction; onClose: () => void; onQueued: () => void }) {
  const isFolder = Boolean(folder);
  const name = folder?.name ?? file?.name ?? "TiVault item";
  const size = folder?.size ?? file?.size ?? 0;
  const accountName = folder?.accountName ?? file?.accountName ?? "Telegram";
  const encrypted = file?.encrypted ?? folderFiles.some((item) => item.encrypted);
  const itemCount = isFolder ? folderFiles.length : 1;
  const [query, setQuery] = useState("");
  const [recipient, setRecipient] = useState<ShareRecipient | null>(null);
  const [recent, setRecent] = useState<ShareRecipient[]>([]);
  const [loadingRecent, setLoadingRecent] = useState(true);
  const [searching, setSearching] = useState(false);
  const [sending, setSending] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    let cancelled = false;
    const request = folder ? api.recentFolderShareRecipients(folder.path) : file ? api.recentShareRecipients(file.id) : Promise.resolve([]);
    void request
      .then((recipients) => { if (!cancelled) setRecent(recipients); })
      .catch(() => undefined)
      .finally(() => { if (!cancelled) setLoadingRecent(false); });
    return () => { cancelled = true; };
  }, [file?.id, folder?.path]);
  const searchRecipient = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!query.trim() || searching) return;
    setSearching(true); setError(""); setRecipient(null);
    try {
      const request = folder ? api.lookupFolderShareRecipient(folder.path, query) : file ? api.lookupShareRecipient(file.id, query) : Promise.reject(new Error("Nothing was selected to share."));
      setRecipient(await withDeadline(request, 35_000, "Telegram did not respond while looking up that username."));
    }
    catch (cause) { setError(String(cause)); }
    finally { setSearching(false); }
  };
  const send = async () => {
    if (!recipient || sending) return;
    const recipientHandle = recipient.username ? `@${recipient.username}` : recipient.displayName;
    const encryptionWarning = encrypted
      ? `TiVault will reconstruct encrypted content locally and send ${isFolder ? "the files" : "this file"} as normal readable ${isFolder ? "documents" : "a document"}, one at a time. A regular Telegram cloud chat is not end-to-end encrypted.`
      : `TiVault will upload ${isFolder ? `${itemCount} individual files` : "a normal readable copy"}. The recipient can save or forward them, and deleting your vault copy will not remove theirs.`;
    const confirmed = await confirmAction({
      title: `Send “${name}” to ${recipientHandle}?`,
      message: `${encryptionWarning} Confirm that ${recipient.displayName}${recipient.username ? ` (@${recipient.username})` : " in your existing Telegram chats"} is the intended recipient.`,
      confirmLabel: encrypted ? "Decrypt and send" : isFolder ? `Send ${itemCount} files` : "Send copy",
      tone: "warning",
    });
    if (!confirmed) return;
    setSending(true); setError("");
    try {
      if (folder) await api.shareFolder(folder.path, recipient.token, encrypted);
      else if (file) await api.shareFile(file.id, recipient.token, encrypted);
      onQueued(); onClose();
    }
    catch (cause) { setError(String(cause)); setRecipient(null); setSending(false); }
  };
  return <div className="modal-backdrop share-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !sending) onClose(); }}>
    <section className="modal share-modal" role="dialog" aria-modal="true" aria-labelledby="share-title">
      <div className="modal-head"><div><span className="eyebrow">SEND VIA TELEGRAM</span><h2 id="share-title">Share {isFolder ? "folder contents" : "a usable copy"}</h2></div><button className="icon-button" disabled={sending} onClick={onClose}><X /></button></div>
      <div className="share-file-summary"><span className="small-file-icon">{isFolder ? <Folder size={19} /> : <FileText size={19} />}</span><span><strong>{name}</strong><small>{isFolder ? `${itemCount} files · ` : ""}{formatBytes(size)} · {accountName}</small></span>{encrypted && <span className="encrypted-chip"><LockKeyhole size={12} /> Encrypted</span>}</div>
      {(loadingRecent || recent.length > 0) && <div className="recent-recipients"><span className="field-label">Recent Telegram chats</span><div>{loadingRecent ? <span className="recent-loading"><RefreshCw className="spin" size={14} /> Loading recent chats…</span> : recent.map((item) => <button key={item.token} className={recipient?.token === item.token ? "active" : ""} onClick={() => { setRecipient(item); setQuery(item.username); setError(""); }}><span className="recent-avatar">{item.initials}</span><span><strong>{item.displayName}</strong><small>{item.username ? `@${item.username}` : "Existing private chat"}</small></span>{item.verified && <BadgeCheck size={13} />}</button>)}</div></div>}
      <form className="recipient-search" onSubmit={searchRecipient}><label className="field"><span>Exact public Telegram username</span><div className="recipient-search-input"><span>@</span><input autoFocus value={query.replace(/^@/, "")} onChange={(event) => { setQuery(event.target.value); setRecipient(null); setError(""); }} placeholder="username" autoComplete="off" spellCheck={false} /><button className="button ghost compact" disabled={!query.trim() || searching}>{searching ? <RefreshCw className="spin" size={15} /> : <Search size={15} />} Verify</button></div></label></form>
      {recipient && <div className="recipient-card"><div className="recipient-avatar">{recipient.initials}</div><div><strong>{recipient.displayName} {recipient.verified && <BadgeCheck size={15} className="verified" />}</strong><span>{recipient.username ? `@${recipient.username}` : "Existing private chat"} · {recipient.kind === "bot" ? "Telegram bot" : "Telegram user"}</span></div><Check className="success" /></div>}
      {encrypted && <div className="notice warning"><LockKeyhole size={17} /><span><strong>Readable copy warning</strong><br />The recipient cannot use TiVault’s encrypted chunks. Sending will create readable Telegram {isFolder ? "copies one at a time" : "a copy"} without revealing your vault recovery key.</span></div>}
      {recipient?.kind === "bot" && <div className="notice danger"><CircleAlert size={17} /> Bots may process or retain uploaded files outside Telegram. Send only if you trust this bot.</div>}
      {error && <div className="notice danger"><CircleAlert size={17} /> {error}</div>}
      <p className="modal-copy">Only exact public usernames are resolved. Always verify the displayed name and username before continuing.</p>
      <div className="modal-actions"><button className="button ghost" disabled={sending} onClick={onClose}>Cancel</button><button className="button primary" disabled={!recipient || sending || itemCount === 0} onClick={send}>{sending ? <RefreshCw className="spin" size={16} /> : <Send size={16} />} {sending ? "Queueing…" : encrypted ? "Review encrypted send" : "Review and send"}</button></div>
    </section>
  </div>;
}

function PreviewPane({ file, onClose, onPreview, onShare, onDownload, onDelete, onManage, onFavorite, onEditTags }: { file: VaultFile; onClose: () => void; onPreview: () => void; onShare: () => void; onDownload: () => void; onDelete: () => void; onManage: (action: FileAction) => void; onFavorite: () => void; onEditTags: () => void }) {
  const visual = fileVisual(file);
  return <aside className="preview-pane"><div className="preview-head"><span>File details</span><button className="icon-button" onClick={onClose}><X size={18} /></button></div><div className="preview-visual" style={{ "--file-color": visual.color } as React.CSSProperties}><LazyFileThumbnail file={file} /></div><h3>{file.name}</h3><p className="preview-sub">{formatBytes(file.size)} · {file.category}</p><div className="preview-actions"><button className="button primary" onClick={onPreview}><Eye size={16} /> Preview</button><button className="button ghost" onClick={onShare}><Share2 size={16} /> Send via Telegram</button><button className="button ghost" onClick={onDownload}><Download size={16} /> Download</button></div><div className="file-manage-actions"><button onClick={onFavorite}><Star size={14} fill={file.favorite ? "currentColor" : "none"} /> {file.favorite ? "Unfavourite" : "Favourite"}</button><button onClick={onEditTags}><Tag size={14} /> Tags</button><button onClick={() => onManage("rename")}><Edit3 size={14} /> Rename</button><button onClick={() => onManage("move")}><FolderInput size={14} /> Move</button><button onClick={() => onManage("copy")}><Copy size={14} /> Copy</button></div>{file.tags.length > 0 && <div className="tag-list">{file.tags.map((tag) => <span key={tag}>{tag}</span>)}</div>}<dl>{file.folderPath && <div><dt>Folder</dt><dd>{file.folderPath}</dd></div>}<div><dt>Protection</dt><dd>{file.encrypted ? <><LockKeyhole size={14} /> Client encrypted</> : "Telegram cloud"}</dd></div><div><dt>Telegram parts</dt><dd>{file.chunkCount}</dd></div><div><dt>Account</dt><dd>{file.accountName}</dd></div><div><dt>Added</dt><dd>{new Date(file.createdAt).toLocaleString()}</dd></div><div><dt>Offline copy</dt><dd>{file.cached ? "Available" : "Cloud only"}</dd></div></dl><button className="danger-button" onClick={onDelete}><Trash2 size={15} /> Move to Recycle Bin</button></aside>;
}

function FileActionModal({ file, action, folders, onClose, onComplete }: { file: VaultFile; action: FileAction; folders: VaultFolderRecord[]; onClose: () => void; onComplete: (file: VaultFile) => void }) {
  const dot = file.name.lastIndexOf(".");
  const suggestedCopy = dot > 0 ? `${file.name.slice(0, dot)} copy${file.name.slice(dot)}` : `${file.name} copy`;
  const [name, setName] = useState(action === "copy" ? suggestedCopy : file.name);
  const [folderPath, setFolderPath] = useState(file.folderPath ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const submit = async (event: React.FormEvent) => {
    event.preventDefault(); setBusy(true); setError("");
    try {
      const result = action === "rename" ? await api.renameFile(file.id, name) : action === "move" ? await api.moveFile(file.id, folderPath) : await api.copyFile(file.id, name, folderPath);
      onComplete(result); onClose();
    } catch (cause) { setError(String(cause)); setBusy(false); }
  };
  return <div className="modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !busy) onClose(); }}><form className="modal file-action-modal" onSubmit={submit}><div className="modal-head"><div><span className="eyebrow">FILE MANAGEMENT</span><h2>{action === "rename" ? "Rename file" : action === "move" ? "Move file" : "Copy file"}</h2></div><button type="button" className="icon-button" disabled={busy} onClick={onClose}><X /></button></div>
    <div className="share-file-summary"><span className="small-file-icon"><FileText /></span><span><strong>{file.name}</strong><small>{formatBytes(file.size)} · {file.folderPath || "My TiVault"}</small></span></div>
    {action !== "move" && <label className="field"><span>Filename</span><input autoFocus maxLength={255} value={name} onChange={(event) => setName(event.target.value)} /></label>}
    {action !== "rename" && <label className="field"><span>Destination folder</span><select value={folderPath} onChange={(event) => setFolderPath(event.target.value)}><option value="">My TiVault</option>{folders.map((folder) => <option key={folder.id} value={folder.path}>{folder.path}</option>)}</select></label>}
    {action === "copy" && <div className="notice"><Copy size={16} /> The copy gets an independent recovery manifest while safely reusing the same verified Telegram chunks.</div>}
    {error && <div className="notice danger"><CircleAlert size={16} /> {error}</div>}
    <div className="modal-actions"><button type="button" className="button ghost" disabled={busy} onClick={onClose}>Cancel</button><button className="button primary" disabled={busy || (action !== "move" && !name.trim())}>{busy ? <RefreshCw className="spin" size={16} /> : action === "rename" ? <Edit3 size={16} /> : action === "move" ? <FolderInput size={16} /> : <Copy size={16} />} {busy ? "Saving…" : action === "rename" ? "Rename" : action === "move" ? "Move" : "Create copy"}</button></div>
  </form></div>;
}

function TagEditorModal({ file, onClose, onSaved }: { file: VaultFile; onClose: () => void; onSaved: () => void }) {
  const [value, setValue] = useState(file.tags.join(", "));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const save = async (event: React.FormEvent) => {
    event.preventDefault(); setBusy(true); setError("");
    try {
      await api.setTags(file.id, value.split(",").map((tag) => tag.trim()).filter(Boolean));
      onSaved(); onClose();
    } catch (cause) { setError(String(cause)); setBusy(false); }
  };
  return <div className="modal-backdrop" onMouseDown={(event) => { if (event.target === event.currentTarget && !busy) onClose(); }}><form className="modal tag-modal" onSubmit={save}><div className="modal-head"><div><span className="eyebrow">ORGANIZE</span><h2>Edit tags</h2></div><button type="button" className="icon-button" onClick={onClose}><X /></button></div><p className="modal-copy">Add up to 20 comma-separated tags. Tags are stored only in TiVault's local catalogue.</p><label className="field"><span>Tags for {file.name}</span><input autoFocus value={value} onChange={(event) => setValue(event.target.value)} placeholder="work, receipts, important" /></label>{error && <div className="notice danger"><CircleAlert size={16} /> {error}</div>}<div className="modal-actions"><button type="button" className="button ghost" onClick={onClose}>Cancel</button><button className="button primary" disabled={busy}>{busy ? <RefreshCw className="spin" size={16} /> : <Tag size={16} />} Save tags</button></div></form></div>;
}

function LockScreen({ onUnlock }: { onUnlock: (status: LockStatus) => void }) {
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const submit = async (event: React.FormEvent) => {
    event.preventDefault(); if (!password || busy) return; setBusy(true); setError("");
    try { onUnlock(await api.unlockApp(password)); setPassword(""); }
    catch (cause) { setError(String(cause)); setBusy(false); }
  };
  return <div className="lock-screen"><form className="lock-card" onSubmit={submit}><img src="/assets/tivault-logo.png" alt="TiVault" /><div className="lock-orb"><Lock /></div><span className="eyebrow">APP LOCK</span><h1>TiVault is locked</h1><p>Your vault catalogue and local preview endpoints remain inaccessible until you unlock the app.</p><label className="field"><span>Password</span><input autoFocus type="password" value={password} onChange={(event) => setPassword(event.target.value)} autoComplete="current-password" /></label>{error && <div className="notice danger"><CircleAlert size={16} /> {error}</div>}<button className="button primary wide" disabled={!password || busy}>{busy ? <RefreshCw className="spin" /> : <LockKeyhole />} Unlock TiVault</button></form></div>;
}

function SettingsView({ data, onRefresh, onCacheCleared, setTheme, onLock, confirmAction }: { data: Dashboard; onRefresh: () => void; onCacheCleared: () => void; setTheme: (theme: "light" | "dark" | "system") => void; onLock: (status: LockStatus) => void; confirmAction: ConfirmAction }) {
  const [cacheMessage, setCacheMessage] = useState("");
  const [lockPassword, setLockPassword] = useState("");
  const [lockConfirm, setLockConfirm] = useState("");
  const [lockBusy, setLockBusy] = useState(false);
  const [lockError, setLockError] = useState("");
  const [recoveryAccount, setRecoveryAccount] = useState(data.accounts[0]?.id ?? "");
  const [recovering, setRecovering] = useState(false);
  const [recoveryReport, setRecoveryReport] = useState<RecoveryReport | null>(null);
  const [recoveryKey, setRecoveryKey] = useState("");
  const [testingRecovery, setTestingRecovery] = useState(false);
  const [recoveryTest, setRecoveryTest] = useState<RecoveryTestReport | null>(null);
  const [checkingHealth, setCheckingHealth] = useState(false);
  const [healthReport, setHealthReport] = useState<HealthReport | null>(data.latestHealthReport ?? null);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [updateMessage, setUpdateMessage] = useState("");
  const save = async (settings: Record<string, unknown>) => { await api.updateSettings(settings); onRefresh(); };
  const clearCache = async () => { const bytes = await api.clearPreviewCache(); onCacheCleared(); setCacheMessage(`${formatBytes(bytes)} of temporary preview and profile-photo data cleared.`); await onRefresh(); };
  const enableLock = async () => {
    if (lockPassword !== lockConfirm) { setLockError("The passwords do not match."); return; }
    setLockBusy(true); setLockError("");
    try { await api.configureAppLock(lockPassword); setLockPassword(""); setLockConfirm(""); await onRefresh(); }
    catch (cause) { setLockError(String(cause)); } finally { setLockBusy(false); }
  };
  const disableLock = async () => {
    if (!await confirmAction({ title: "Remove app lock?", message: "TiVault will stop asking for this password at startup and after inactivity. Your vault encryption and operating-system keychain remain unchanged.", confirmLabel: "Remove app lock", tone: "warning" })) return;
    setLockBusy(true); setLockError("");
    try { await api.disableAppLock(lockPassword); setLockPassword(""); await onRefresh(); }
    catch (cause) { setLockError(String(cause)); } finally { setLockBusy(false); }
  };
  const recover = async () => {
    if (!recoveryAccount || recovering) return;
    if (!await confirmAction({ title: "Rebuild this vault from Telegram?", message: "TiVault will scan Saved Messages for signed TiVault manifests and restore missing catalogue entries locally. It will not upload, forward or delete Telegram messages.", confirmLabel: "Scan and restore", tone: "warning" })) return;
    setRecovering(true); setRecoveryReport(null);
    try { setRecoveryReport(await api.recoverVault(recoveryAccount)); await onRefresh(); }
    catch (cause) { alert(String(cause)); } finally { setRecovering(false); }
  };
  const testRecovery = async () => {
    if (!recoveryAccount || !recoveryKey.trim() || testingRecovery) return;
    setTestingRecovery(true); setRecoveryTest(null);
    try { setRecoveryTest(await api.testRecovery(recoveryAccount, recoveryKey)); }
    catch (cause) { alert(String(cause)); } finally { setTestingRecovery(false); }
  };
  const checkHealth = async () => {
    if (!recoveryAccount || checkingHealth) return;
    setCheckingHealth(true);
    try { setHealthReport(await api.runHealthCheck(recoveryAccount, 5)); await onRefresh(); }
    catch (cause) { alert(String(cause)); } finally { setCheckingHealth(false); }
  };
  const changeNotifications = async (enabled: boolean) => {
    if (enabled && !await api.requestNotificationPermission()) {
      alert("Notifications are disabled in macOS settings. Allow TiVault notifications there, then try again.");
      return;
    }
    await save({ notificationsEnabled: enabled });
  };
  const checkForUpdate = async () => {
    if (!data.automaticUpdatesConfigured || updateBusy) return;
    if (!await confirmAction({ title: "Check for a signed update?", message: "TiVault will contact the configured HTTPS release endpoint. If an update exists, its signature will be verified before installation and the app will restart.", confirmLabel: "Check for update", tone: "warning" })) return;
    setUpdateBusy(true); setUpdateMessage("");
    try {
      const result = await api.installAvailableUpdate();
      if (result === "current") setUpdateMessage("TiVault is already up to date.");
    } catch (cause) { setUpdateMessage(String(cause)); } finally { setUpdateBusy(false); }
  };
  return <div className="settings-page"><div className="page-title"><div><span className="eyebrow">PREFERENCES</span><h1>Settings</h1><p>Control privacy, recovery, performance, caching and the web companion.</p></div></div>
    <section className="settings-section"><div className="section-title"><ShieldCheck /><div><h3>Privacy, keychain and encryption</h3><p>Encrypted uploads are protected before leaving this device.</p></div></div><div className="settings-card"><div className="setting-row"><span><strong>Operating-system keychain</strong><small>{data.keychainBacked ? "The vault recovery key is stored in the operating-system credential vault instead of a plaintext application file." : "The credential vault was unavailable; the recovery key remains in a private 0600 fallback file."}</small></span>{data.keychainBacked ? <Check className="success" /> : <CircleAlert className="warning" />}</div><button className="setting-row action" onClick={async () => alert(`Recovery key:\n\n${await api.exportRecovery()}\n\nStore it somewhere safe.`)}><span><strong>Export recovery key</strong><small>Needed to open encrypted files on another device.</small></span><KeyRound /></button></div></section>
    <section className="settings-section"><div className="section-title"><Lock /><div><h3>App lock</h3><p>Require an Argon2-protected password whenever TiVault starts or becomes idle.</p></div></div><div className="settings-card lock-settings">{data.appLockEnabled ? <><label className="setting-row"><span><strong>Automatic lock</strong><small>Lock after this much inactivity.</small></span><select value={data.appLockTimeoutMinutes} onChange={(event) => save({ appLockTimeoutMinutes: Number(event.target.value) })}><option value="5">5 minutes</option><option value="15">15 minutes</option><option value="30">30 minutes</option><option value="60">1 hour</option></select></label><label className="field"><span>Current app-lock password</span><input type="password" value={lockPassword} onChange={(event) => setLockPassword(event.target.value)} autoComplete="current-password" /></label><div className="inline-settings-actions"><button className="button ghost" disabled={lockBusy} onClick={async () => onLock(await api.lockApp())}><Lock size={15} /> Lock now</button><button className="button danger" disabled={lockBusy || !lockPassword} onClick={disableLock}><Trash2 size={15} /> Remove app lock</button></div><small className="setting-help">Removing app lock does not disable file encryption or remove the recovery key from your operating-system keychain.</small></> : <><label className="field"><span>New app-lock password</span><input type="password" value={lockPassword} onChange={(event) => setLockPassword(event.target.value)} autoComplete="new-password" /></label><label className="field"><span>Confirm password</span><input type="password" value={lockConfirm} onChange={(event) => setLockConfirm(event.target.value)} autoComplete="new-password" /></label><button className="button primary" disabled={lockBusy || lockPassword.length < 8 || !lockConfirm} onClick={enableLock}>{lockBusy ? <RefreshCw className="spin" /> : <Lock />} Enable app lock</button></>}{lockError && <div className="notice danger"><CircleAlert size={16} /> {lockError}</div>}</div></section>
    <section className="settings-section"><div className="section-title"><RefreshCw /><div><h3>Vault recovery from Telegram</h3><p>Test recovery before a disaster, or rebuild missing catalogue entries from Saved Messages.</p></div></div><div className="settings-card"><label className="setting-row"><span><strong>Telegram vault account</strong><small>Recovery tests and scans are read-only in Telegram.</small></span><select value={recoveryAccount} onChange={(event) => setRecoveryAccount(event.target.value)}>{data.accounts.map((account) => <option key={account.id} value={account.id}>{account.name}</option>)}</select></label><label className="field recovery-key-field"><span>Recovery key to verify</span><input type="password" autoComplete="off" value={recoveryKey} onChange={(event) => setRecoveryKey(event.target.value)} placeholder="Paste the exported recovery key" /></label><button className="setting-row action" disabled={!recoveryAccount || !recoveryKey.trim() || testingRecovery} onClick={testRecovery}><span><strong>{testingRecovery ? "Testing a restore sample…" : "Run recovery test wizard"}</strong><small>Verifies the key, manifests and representative chunks without changing your vault.</small></span>{testingRecovery ? <RefreshCw className="spin" /> : <ShieldCheck />}</button>{recoveryTest && <div className={`recovery-result ${recoveryTest.keyValid && recoveryTest.warnings.length === 0 ? "healthy" : ""}`}><strong>{recoveryTest.keyValid ? "Recovery key verified" : "Recovery key failed"}</strong><span>{recoveryTest.manifestsValid} of {recoveryTest.filesSampled} sampled manifests valid · {recoveryTest.chunksAvailable} chunks available.</span>{recoveryTest.warnings.slice(0, 3).map((warning) => <small key={warning}>{warning}</small>)}</div>}<button className="setting-row action" disabled={!recoveryAccount || recovering} onClick={recover}><span><strong>{recovering ? "Scanning Saved Messages…" : "Scan and restore missing files"}</strong><small>Existing file IDs are skipped; valid missing manifests are imported locally.</small></span>{recovering ? <RefreshCw className="spin" /> : <RefreshCw />}</button>{recoveryReport && <div className="recovery-result"><strong>{recoveryReport.restored} restored · {recoveryReport.skipped} skipped</strong><span>{recoveryReport.manifestsFound} manifests found while scanning {recoveryReport.scannedMessages} messages.</span>{recoveryReport.warnings.slice(0, 3).map((warning) => <small key={warning}>{warning}</small>)}</div>}</div></section>
    <section className="settings-section"><div className="section-title"><Activity /><div><h3>Vault health checks</h3><p>Sample remote manifests and chunks to detect missing or corrupted data early.</p></div></div><div className="settings-card"><label className="setting-row"><span><strong>Automatic health checks</strong><small>Uses a small randomized sample and never changes Telegram messages.</small></span><label className="switch"><input type="checkbox" checked={data.healthChecksEnabled} onChange={(event) => save({ healthChecksEnabled: event.target.checked })} /><span /></label></label><label className="setting-row"><span><strong>Check interval</strong><small>Runs while TiVault is open and the account is connected.</small></span><select value={data.healthCheckIntervalDays} onChange={(event) => save({ healthCheckIntervalDays: Number(event.target.value) })}><option value="1">Daily</option><option value="7">Weekly</option><option value="14">Every 2 weeks</option><option value="30">Monthly</option></select></label><button className="setting-row action" disabled={!recoveryAccount || checkingHealth} onClick={checkHealth}><span><strong>{checkingHealth ? "Checking Telegram samples…" : "Run health check now"}</strong><small>Large chunks are checked by remote size; chunks up to 32 MB also receive a full SHA-256 verification.</small></span>{checkingHealth ? <RefreshCw className="spin" /> : <Activity />}</button>{healthReport && <div className={`recovery-result ${healthReport.healthy ? "healthy" : ""}`}><strong>{healthReport.healthy ? "Vault sample is healthy" : "Vault sample needs attention"}</strong><span>{healthReport.filesSampled} files · {healthReport.chunksChecked} chunks · {healthReport.hashesVerified} full hashes · {healthReport.missing} missing · {healthReport.corrupted} corrupted.</span>{healthReport.warnings.slice(0, 3).map((warning) => <small key={warning}>{warning}</small>)}</div>}</div></section>
    <section className="settings-section"><div className="section-title"><Trash2 /><div><h3>Recycle Bin</h3><p>Keep deleted files recoverable before their Telegram messages are removed.</p></div></div><div className="settings-card"><label className="setting-row"><span><strong>Automatic permanent deletion</strong><small>Files remain restorable until this retention period expires.</small></span><select value={data.recycleRetentionDays} onChange={(event) => save({ recycleRetentionDays: Number(event.target.value) })}><option value="7">After 7 days</option><option value="14">After 14 days</option><option value="30">After 30 days</option></select></label></div></section>
    <section className="settings-section"><div className="section-title"><Gauge /><div><h3>Transfers and notifications</h3><p>Control concurrency, automatic retry behavior and private local alerts.</p></div></div><div className="settings-card"><div className="segmented wide"><button className={data.speedProfile === "low-impact" ? "active" : ""} onClick={() => save({ speedProfile: "low-impact" })}>Low impact</button><button className={data.speedProfile === "balanced" ? "active" : ""} onClick={() => save({ speedProfile: "balanced" })}>Balanced</button><button className={data.speedProfile === "maximum" ? "active" : ""} onClick={() => save({ speedProfile: "maximum" })}>Maximum</button></div><label className="setting-row"><span><strong>Automatic retries</strong><small>Transient Telegram and network errors use bounded backoff; invalid requests are never retried.</small></span><select value={data.automaticRetryCount} onChange={(event) => save({ automaticRetryCount: Number(event.target.value) })}><option value="0">Off</option><option value="1">1 retry</option><option value="3">3 retries</option><option value="5">5 retries</option></select></label><label className="setting-row"><span><strong>Transfer notifications</strong><small>Show generic completed, failed, paused and FLOOD_WAIT alerts without filenames.</small></span><label className="switch"><input type="checkbox" checked={data.notificationsEnabled} onChange={(event) => void changeNotifications(event.target.checked)} /><span /></label></label></div></section>
    <section className="settings-section"><div className="section-title"><HardDrive /><div><h3>Temporary cache</h3><p>Preview blocks and the small profile-photo cache stay local and removable.</p></div></div><div className="settings-card"><label className="setting-row"><span><strong>Automatic preview cleanup</strong><small>Clear idle preview sessions after this time.</small></span><select value={data.previewCacheTtlMinutes} onChange={(event) => save({ previewCacheTtlMinutes: Number(event.target.value) })}><option value="5">5 minutes</option><option value="15">15 minutes</option><option value="30">30 minutes</option><option value="60">1 hour</option></select></label><label className="setting-row"><span><strong>Maximum preview cache</strong><small>Encrypted previews never exceed this limit.</small></span><select value={Math.round(data.previewCacheLimit / 1024 ** 2)} onChange={(event) => save({ previewCacheLimitMb: Number(event.target.value) })}><option value="128">128 MB</option><option value="256">256 MB</option><option value="512">512 MB</option></select></label><button className="setting-row action" onClick={clearCache}><span><strong>Clear cache now</strong><small>Close active previews and remove preview blocks and the cached profile photo immediately.</small></span><Trash2 /></button>{cacheMessage && <div className="notice"><Check size={16} /> {cacheMessage}</div>}</div></section>
    <section className="settings-section"><div className="section-title"><ShieldCheck /><div><h3>Signed automatic updates</h3><p>Every public update must be cryptographically signed before TiVault will install it.</p></div></div><div className="settings-card"><div className="setting-row"><span><strong>{data.automaticUpdatesConfigured ? "Release channel configured" : "Waiting for release signing setup"}</strong><small>{data.automaticUpdatesConfigured ? "This build contains a release endpoint and verification public key." : "No updater endpoint or public verification key is embedded in this build. Automatic checks stay disabled safely."}</small></span>{data.automaticUpdatesConfigured ? <Check className="success" /> : <CircleAlert className="warning" />}</div>{data.automaticUpdatesConfigured && <button className="setting-row action" disabled={updateBusy} onClick={checkForUpdate}><span><strong>{updateBusy ? "Checking and verifying…" : "Check for updates"}</strong><small>Only an artifact signed by the configured TiVault release key can be installed.</small></span>{updateBusy ? <RefreshCw className="spin" /> : <RefreshCw />}</button>}{updateMessage && <div className="notice"><Info size={16} /> {updateMessage}</div>}</div></section>
    <section className="settings-section"><div className="section-title"><Moon /><div><h3>Appearance</h3><p>Use the style that fits your system.</p></div></div><div className="theme-choices"><button onClick={() => setTheme("light")}><Sun /> Light</button><button onClick={() => setTheme("dark")}><Moon /> Dark</button><button onClick={() => setTheme("system")}><MonitorDown /> System</button></div></section>
    <section className="settings-section"><div className="section-title"><Cloud /><div><h3>Web companion</h3><p>Available only while the TiVault desktop app is running.</p></div></div><div className="settings-card"><button className="setting-row action" onClick={async () => { if ("__TAURI_INTERNALS__" in window) { const { openUrl } = await import("@tauri-apps/plugin-opener"); await openUrl("http://127.0.0.1:7468"); } else { window.location.href = "http://127.0.0.1:7468"; } }}><span><strong>Open web interface</strong><small>http://127.0.0.1:7468 · loopback only</small></span><Link2 /></button></div></section>
  </div>;
}

function AboutView() {
  return <div className="content-page about-page"><div className="about-card"><div className="about-brand"><img src="/assets/tivault-logo.png" alt="TiVault logo" /><div><span className="eyebrow">ABOUT TIVAULT</span><h1>TiVault</h1><p>Your private, encrypted file vault backed by Telegram Saved Messages.</p></div></div><div className="about-details"><div><span>Version</span><strong>0.1.1-alpha</strong></div><div><span>Developer</span><strong>H34TB145T</strong></div><div><span>Storage</span><strong>Telegram Saved Messages</strong></div><div><span>Privacy</span><strong>Optional client-side encryption</strong></div></div><div className="about-credit"><ShieldCheck size={18} /><span><small>DESIGNED AND DEVELOPED BY</small><strong>H34TB145T</strong></span></div></div></div>;
}

function RecycleBinView({ files, onRestore, onDelete, onEmpty }: { files: VaultFile[]; onRestore: (file: VaultFile) => void; onDelete: (file: VaultFile) => void; onEmpty: () => void }) {
  return <div className="content-page"><div className="page-title"><div><span className="eyebrow">RECOVERABLE DELETIONS</span><h1>Recycle Bin</h1><p>{files.length} {files.length === 1 ? "file" : "files"} waiting for automatic permanent deletion.</p></div><button className="button danger" disabled={!files.length} onClick={onEmpty}><Trash2 size={16} /> Empty Recycle Bin</button></div><div className="notice warning"><Clock3 size={17} /><span><strong>Telegram files are still intact</strong><br />Permanent deletion happens only after the retention date or when you explicitly delete an item here.</span></div><div className="trash-list">{files.length ? files.map((file) => <div className="trash-row" key={file.id}><span className="small-file-icon" style={{ color: fileVisual(file).color }}><File size={19} /></span><span><strong>{file.name}</strong><small>{formatBytes(file.size)} · {file.folderPath || "My TiVault"}</small></span><span><small>Deleted</small><strong>{file.deletedAt ? formatDate(file.deletedAt) : "Recently"}</strong></span><span><small>Permanent deletion</small><strong>{file.purgeAt ? new Date(file.purgeAt).toLocaleDateString() : "Pending"}</strong></span><button className="button ghost compact" onClick={() => onRestore(file)}><RotateCcw size={15} /> Restore</button><button className="button danger compact" onClick={() => onDelete(file)}><Trash2 size={15} /> Delete permanently</button></div>) : <div className="empty-state"><div className="empty-cloud"><Trash2 /></div><h3>Recycle Bin is empty</h3><p>Deleted files will appear here until their retention period ends.</p></div>}</div></div>;
}

export default function App() {
  const [data, setData] = useState<Dashboard | null>(null);
  const [lockStatus, setLockStatus] = useState<LockStatus | null>(null);
  const [view, setView] = useState<View>("vault");
  const [category, setCategory] = useState<Category>("All files");
  const [currentFolder, setCurrentFolder] = useState("");
  const [query, setQuery] = useState("");
  const [layout, setLayout] = useState<"grid" | "list">("grid");
  const [sortOrder, setSortOrder] = useState<SortOrder>("newest");
  const [tagFilter, setTagFilter] = useState("");
  const [uploadSelection, setUploadSelection] = useState<UploadSelection | null>(null);
  const [createFolderOpen, setCreateFolderOpen] = useState(false);
  const [accountModal, setAccountModal] = useState<Account | "new" | null>(null);
  const [selected, setSelected] = useState<VaultFile | null>(null);
  const [previewFile, setPreviewFile] = useState<VaultFile | null>(null);
  const [shareFile, setShareFile] = useState<VaultFile | null>(null);
  const [shareFolder, setShareFolder] = useState<VaultFolder | null>(null);
  const [manageFile, setManageFile] = useState<{ file: VaultFile; action: FileAction } | null>(null);
  const [tagFile, setTagFile] = useState<VaultFile | null>(null);
  const [fileSelectionMode, setFileSelectionMode] = useState(false);
  const [selectedFileIds, setSelectedFileIds] = useState<Set<string>>(() => new Set());
  const [transferSelectionMode, setTransferSelectionMode] = useState(false);
  const [selectedTransferIds, setSelectedTransferIds] = useState<Set<string>>(() => new Set());
  const [confirmation, setConfirmation] = useState<PendingConfirmation | null>(null);
  const [dragging, setDragging] = useState(false);
  const [mobileNav, setMobileNav] = useState(false);
  const [avatarUrl, setAvatarUrl] = useState<string | null>(null);
  const refreshTimer = useRef<number | undefined>(undefined);
  const confirmationRef = useRef<PendingConfirmation | null>(null);
  const previousTransferStates = useRef<Map<string, Transfer["state"]> | null>(null);
  const automaticUpdateChecked = useRef(false);

  const refresh = useCallback(async () => {
    try { setData(await api.dashboard()); }
    catch {
      try {
        const status = await api.lockStatus();
        setLockStatus(status);
        if (status.locked) setData(null);
      } catch (error) { console.error(error); }
    }
  }, []);
  const confirmAction = useCallback<ConfirmAction>((options) => new Promise((resolve) => {
    confirmationRef.current?.resolve(false);
    const pending = { ...options, resolve };
    confirmationRef.current = pending;
    setConfirmation(pending);
  }), []);
  const answerConfirmation = useCallback((confirmed: boolean) => {
    const pending = confirmationRef.current;
    confirmationRef.current = null;
    setConfirmation(null);
    pending?.resolve(confirmed);
  }, []);
  useEffect(() => {
    let active = true;
    const checkLock = async () => {
      try {
        const status = await api.lockStatus();
        if (!active) return;
        setLockStatus(status);
        if (status.locked) {
          setData(null); setSelected(null); setPreviewFile(null); setShareFile(null); setShareFolder(null); setManageFile(null);
        }
      } catch (error) { console.error(error); }
    };
    void checkLock();
    const lockTimer = window.setInterval(checkLock, 2_000);
    return () => { active = false; window.clearInterval(lockTimer); };
  }, []);
  useEffect(() => { void refresh(); refreshTimer.current = window.setInterval(refresh, 1_500); return () => window.clearInterval(refreshTimer.current); }, [refresh]);
  useEffect(() => {
    if (!data) return;
    const current = new Map(data.transfers.map((transfer) => [transfer.id, transfer.state]));
    const previous = previousTransferStates.current;
    previousTransferStates.current = current;
    if (!previous || !data.notificationsEnabled) return;
    for (const transfer of data.transfers) {
      if (previous.get(transfer.id) === transfer.state) continue;
      const notification = transfer.state === "complete" ? ["Transfer complete", "A TiVault transfer completed successfully."] : transfer.state === "failed" ? ["Transfer failed", "A TiVault transfer needs your attention."] : transfer.state === "paused" ? ["Transfer paused", "A TiVault transfer is paused."] : transfer.state === "waiting" ? ["Telegram asked TiVault to wait", "A transfer will retry automatically after Telegram's wait period."] : null;
      if (notification) void api.sendNotification(notification[0], notification[1]);
    }
  }, [data]);
  useEffect(() => {
    if (!data?.automaticUpdatesConfigured || automaticUpdateChecked.current) return;
    automaticUpdateChecked.current = true;
    void api.availableUpdateVersion().then((version) => {
      if (version) void api.sendNotification("Signed TiVault update available", `Version ${version} is ready to review in Settings.`);
    }).catch((error) => console.warn("Automatic update check failed", error));
  }, [data?.automaticUpdatesConfigured]);
  const primaryAccount = data?.accounts.find((account) => account.connected) ?? data?.accounts[0];
  const avatarRefreshContext = view === "accounts" || view === "settings" ? view : "main";
  useEffect(() => {
    let active = true;
    setAvatarUrl(null);
    if (!primaryAccount?.connected) return () => { active = false; };
    void api.accountAvatar(primaryAccount.id)
      .then((url) => { if (active) setAvatarUrl(url); })
      .catch((error) => console.warn("Unable to refresh Telegram profile photo", error));
    return () => { active = false; };
  }, [primaryAccount?.id, primaryAccount?.connected, avatarRefreshContext]);
  useEffect(() => {
    let lastRecorded = 0;
    const record = () => {
      const now = Date.now();
      if (now - lastRecorded < 10_000) return;
      lastRecorded = now;
      void api.recordActivity().then(setLockStatus).catch(() => undefined);
    };
    window.addEventListener("pointerdown", record, { passive: true });
    window.addEventListener("keydown", record);
    return () => { window.removeEventListener("pointerdown", record); window.removeEventListener("keydown", record); };
  }, []);
  useEffect(() => {
    let unlisten: undefined | (() => void);
    if ("__TAURI_INTERNALS__" in window) import("@tauri-apps/api/webview").then(({ getCurrentWebview }) => getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type === "over") setDragging(true);
      if (event.payload.type === "leave") setDragging(false);
      if (event.payload.type === "drop") {
        setDragging(false);
        const dropped = event.payload.paths;
        void api.expandUploadPaths(dropped)
          .then((paths) => setUploadSelection({
            paths,
            root: dropped.length === 1 && !paths.includes(dropped[0]) ? dropped[0] : undefined,
            destinationFolder: view === "vault" ? currentFolder : "",
          }))
          .catch((error) => alert(String(error)));
      }
    }).then((fn) => { unlisten = fn; }));
    return () => unlisten?.();
  }, [currentFolder, view]);

  const availableTags = useMemo(() => [...new Set((data?.files ?? []).filter((file) => file.status === "ready").flatMap((file) => file.tags))].sort((a, b) => a.localeCompare(b)), [data]);
  const matchingFiles = useMemo(() => {
    const term = query.trim().toLowerCase();
    const files = (data?.files ?? []).filter((file) => {
      if (file.status !== "ready") return false;
      if (view === "favorites" && !file.favorite) return false;
      if (view === "recent" && !file.lastOpenedAt) return false;
      if (view === "vault" && category !== "All files" && file.category !== category) return false;
      if (tagFilter && !file.tags.includes(tagFilter)) return false;
      return !term || file.name.toLowerCase().includes(term) || file.tags.some((tag) => tag.toLowerCase().includes(term));
    });
    return files.sort((left, right) => {
      const leftDate = view === "recent" ? left.lastOpenedAt ?? left.createdAt : left.createdAt;
      const rightDate = view === "recent" ? right.lastOpenedAt ?? right.createdAt : right.createdAt;
      if (sortOrder === "oldest") return leftDate.localeCompare(rightDate);
      if (sortOrder === "name") return left.name.localeCompare(right.name);
      if (sortOrder === "largest") return right.size - left.size;
      if (sortOrder === "smallest") return left.size - right.size;
      if (sortOrder === "type") return left.category.localeCompare(right.category) || left.name.localeCompare(right.name);
      return rightDate.localeCompare(leftDate);
    });
  }, [data, category, query, sortOrder, tagFilter, view]);
  const visibleFolders = useMemo(
    () => view === "vault" ? foldersAt(matchingFiles, data?.folders ?? [], currentFolder, category === "All files" && !query && !tagFilter) : [],
    [matchingFiles, data?.folders, currentFolder, category, query, tagFilter, view],
  );
  const directFiles = useMemo(() => view === "vault" ? matchingFiles.filter((file) => (file.folderPath ?? "") === currentFolder) : matchingFiles, [matchingFiles, currentFolder, view]);
  const scopedFiles = useMemo(() => {
    if (view !== "vault" || !currentFolder) return matchingFiles;
    const prefix = `${currentFolder}/`;
    return matchingFiles.filter((file) => (file.folderPath ?? "") === currentFolder || (file.folderPath ?? "").startsWith(prefix));
  }, [matchingFiles, currentFolder, view]);
  const trashedFiles = data?.files.filter((file) => file.status === "trashed") ?? [];
  const activeTransfers = data?.transfers.filter((transfer) => !["complete", "failed"].includes(transfer.state)).length ?? 0;
  const historyTransfers = data?.transfers.filter((transfer) => ["complete", "failed"].includes(transfer.state)) ?? [];
  const allDirectFilesSelected = directFiles.length > 0 && directFiles.every((file) => selectedFileIds.has(file.id));
  const allHistorySelected = historyTransfers.length > 0 && historyTransfers.every((transfer) => selectedTransferIds.has(transfer.id));
  const uploadDestination = view === "vault" ? currentFolder : "";
  const choose = async () => { const paths = await api.chooseFiles(); if (paths.length) setUploadSelection({ paths, destinationFolder: uploadDestination }); };
  const chooseFolderUpload = async () => {
    try {
      const selection = await api.chooseUploadFolder();
      if (selection.paths.length) setUploadSelection({ ...selection, destinationFolder: uploadDestination });
    } catch (error) { alert(String(error)); }
  };
  const createFolder = async (name: string) => {
    await api.createFolder(currentFolder, name);
    await refresh();
  };
  const downloadFolder = async (path: string) => {
    try { await api.downloadFolder(path); await refresh(); }
    catch (error) { alert(String(error)); }
  };
  const deleteFile = async (file: VaultFile) => {
    if (!await confirmAction({ title: "Move this file to the Recycle Bin?", message: `“${file.name}” will remain recoverable for ${data?.recycleRetentionDays ?? 30} days. Its Telegram Saved Messages data will not be deleted yet.`, confirmLabel: "Move to Recycle Bin", tone: "warning" })) return;
    try { await api.deleteFile(file.id); if (selected?.id === file.id) setSelected(null); if (previewFile?.id === file.id) setPreviewFile(null); if (shareFile?.id === file.id) setShareFile(null); setSelectedFileIds((current) => { const next = new Set(current); next.delete(file.id); return next; }); await refresh(); }
    catch (error) { alert(String(error)); }
  };
  const toggleFavorite = async (file: VaultFile) => {
    try {
      await api.setFavorite(file.id, !file.favorite);
      if (selected?.id === file.id) setSelected({ ...selected, favorite: !file.favorite });
      await refresh();
    } catch (cause) { alert(String(cause)); }
  };
  const toggleFileSelection = (id: string) => setSelectedFileIds((current) => { const next = new Set(current); if (next.has(id)) next.delete(id); else next.add(id); return next; });
  const stopFileSelection = () => { setFileSelectionMode(false); setSelectedFileIds(new Set()); };
  const deleteSelectedFiles = async () => {
    const ids = [...selectedFileIds];
    if (!ids.length || !await confirmAction({ title: `Move ${ids.length} selected ${ids.length === 1 ? "file" : "files"} to the Recycle Bin?`, message: `The selected ${ids.length === 1 ? "file remains" : "files remain"} recoverable for ${data?.recycleRetentionDays ?? 30} days. Telegram data is retained until permanent deletion.`, confirmLabel: "Move selected", tone: "warning" })) return;
    try { await api.deleteFiles(ids); setSelected(null); if (previewFile && ids.includes(previewFile.id)) setPreviewFile(null); if (shareFile && ids.includes(shareFile.id)) setShareFile(null); stopFileSelection(); await refresh(); }
    catch (error) { await refresh(); alert(String(error)); }
  };
  const downloadSelectedFiles = async () => {
    const ids = [...selectedFileIds];
    if (!ids.length) return;
    try { await Promise.all(ids.map((id) => api.downloadFile(id))); await refresh(); }
    catch (error) { alert(String(error)); }
  };
  const deleteFolder = async (folder: VaultFolder) => {
    if (!data) return;
    const prefix = `${folder.path}/`;
    const files = data.files.filter((file) => file.status === "ready" && ((file.folderPath ?? "") === folder.path || (file.folderPath ?? "").startsWith(prefix)));
    const message = files.length
      ? `All ${files.length} ${files.length === 1 ? "file" : "files"} inside it will move to the Recycle Bin and remain recoverable for ${data.recycleRetentionDays} days. Telegram data is retained until permanent deletion.`
      : "This empty folder will be removed from the local catalogue.";
    if (!await confirmAction({ title: `Move the folder “${folder.name}” to the Recycle Bin?`, message, confirmLabel: "Move folder", tone: "warning" })) return;
    try {
      await api.deleteFolder(folder.path);
      if (selected && files.some((file) => file.id === selected.id)) setSelected(null);
      if (previewFile && files.some((file) => file.id === previewFile.id)) setPreviewFile(null);
      if (shareFile && files.some((file) => file.id === shareFile.id)) setShareFile(null);
      if (shareFolder?.path === folder.path || shareFolder?.path.startsWith(`${folder.path}/`)) setShareFolder(null);
      setSelectedFileIds((current) => new Set([...current].filter((id) => !files.some((file) => file.id === id))));
      await refresh();
    } catch (error) { await refresh(); alert(String(error)); }
  };
  const restoreTrashedFile = async (file: VaultFile) => {
    try { await api.restoreFile(file.id); await refresh(); }
    catch (cause) { alert(String(cause)); }
  };
  const permanentlyDeleteTrashedFile = async (file: VaultFile) => {
    if (!await confirmAction({ title: `Permanently delete “${file.name}”?`, message: "The file's unshared chunks and recovery manifest will be deleted from Telegram Saved Messages. This cannot be undone.", confirmLabel: "Delete permanently" })) return;
    try { await api.permanentlyDeleteFile(file.id); await refresh(); }
    catch (cause) { alert(String(cause)); }
  };
  const emptyRecycleBin = async () => {
    if (!trashedFiles.length || !await confirmAction({ title: "Empty the Recycle Bin?", message: `All ${trashedFiles.length} trashed ${trashedFiles.length === 1 ? "file" : "files"} will be permanently deleted from Telegram Saved Messages. This cannot be undone.`, confirmLabel: "Empty Recycle Bin" })) return;
    try { await api.emptyTrash(); await refresh(); }
    catch (cause) { alert(String(cause)); }
  };
  const toggleTransferSelection = (id: string) => setSelectedTransferIds((current) => { const next = new Set(current); if (next.has(id)) next.delete(id); else next.add(id); return next; });
  const stopTransferSelection = () => { setTransferSelectionMode(false); setSelectedTransferIds(new Set()); };
  const dismissOneTransfer = async (transfer: Transfer) => {
    const detail = transfer.state === "failed" && transfer.direction === "upload"
      ? "Any partial Telegram upload parts will also be cleaned up."
      : "The stored vault file and its Telegram copy will remain.";
    if (!await confirmAction({ title: "Remove this history entry?", message: `“${transfer.fileName}” will be removed from transfer history. ${detail}`, confirmLabel: "Remove history", tone: "warning" })) return;
    try { await api.dismissTransfer(transfer.id); await refresh(); }
    catch (error) { alert(String(error)); }
  };
  const cancelTransfer = async (transfer: Transfer) => {
    const detail = transfer.direction === "upload" ? "Any uploaded Telegram parts will also be deleted." : transfer.direction === "share" ? "The original vault file will remain, and temporary decrypted data will be removed." : "The Telegram vault file will remain.";
    if (!await confirmAction({ title: "Cancel this transfer?", message: `The “${transfer.fileName}” transfer will stop and be removed. ${detail}`, confirmLabel: "Cancel transfer", tone: "warning" })) return;
    try { await api.cancelTransfer(transfer.id); await refresh(); }
    catch (error) { alert(String(error)); }
  };
  const deleteSelectedTransferHistory = async () => {
    const ids = [...selectedTransferIds];
    if (!ids.length || !await confirmAction({ title: `Remove ${ids.length} history ${ids.length === 1 ? "entry" : "entries"}?`, message: "Completed vault files and Telegram copies will remain. Any remnants from failed uploads will be cleaned up.", confirmLabel: "Remove selected", tone: "warning" })) return;
    try { await api.dismissTransfers(ids); stopTransferSelection(); await refresh(); }
    catch (error) { await refresh(); alert(String(error)); }
  };
  const clearAllTransferHistory = async () => {
    if (!historyTransfers.length || !await confirmAction({ title: "Clear all transfer history?", message: `All ${historyTransfers.length} completed or failed history ${historyTransfers.length === 1 ? "entry" : "entries"} will be removed. Completed vault files and Telegram copies will remain; failed upload remnants will be cleaned up.`, confirmLabel: "Clear all history", tone: "warning" })) return;
    try { await api.clearTransferHistory(); stopTransferSelection(); await refresh(); }
    catch (error) { await refresh(); alert(String(error)); }
  };
  const setTheme = (theme: "light" | "dark" | "system") => { localStorage.setItem("televault.theme", theme); const resolved = theme === "system" ? (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light") : theme; document.documentElement.dataset.theme = resolved; };
  useEffect(() => setTheme((localStorage.getItem("televault.theme") as "light" | "dark" | "system") || "system"), []);

  if (lockStatus?.locked) return <LockScreen onUnlock={(status) => { setLockStatus(status); void refresh(); }} />;
  if (!lockStatus || !data) return <Skeleton />;
  const isLibraryView = view === "vault" || view === "favorites" || view === "recent";
  const libraryTitle = view === "favorites" ? "Favourites" : view === "recent" ? "Recent files" : currentFolder ? currentFolder.split("/").pop() ?? category : category;
  const nav = (target: View) => {
    if (target !== "vault") stopFileSelection();
    if (target !== "transfers") stopTransferSelection();
    setSelected(null);
    setView(target); setMobileNav(false);
  };
  return <div className={`app-shell ${selected ? "with-preview" : ""}`}>
    <aside className={`sidebar ${mobileNav ? "open" : ""}`}>
      <div className="brand"><img src="/assets/tivault-logo.png" /><span>TiVault</span></div>
      <nav>
        <button className={view === "vault" ? "active" : ""} onClick={() => { setCurrentFolder(""); stopFileSelection(); nav("vault"); }}><LayoutDashboard /> Vault</button>
        <button className={view === "transfers" ? "active" : ""} onClick={() => nav("transfers")}><RefreshCw /> Transfers {activeTransfers > 0 && <b>{activeTransfers}</b>}</button>
        <button className={view === "watch" ? "active" : ""} onClick={() => nav("watch")}><FolderSync /> Watch folders</button>
      </nav>
      <div className="nav-label">SMART VIEWS</div><nav><button className={view === "favorites" ? "active" : ""} onClick={() => { setCurrentFolder(""); nav("favorites"); }}><Star /> Favourites <span>{data.files.filter((file) => file.status === "ready" && file.favorite).length}</span></button><button className={view === "recent" ? "active" : ""} onClick={() => { setCurrentFolder(""); nav("recent"); }}><History /> Recent</button><button className={view === "trash" ? "active" : ""} onClick={() => nav("trash")}><Trash2 /> Recycle Bin <span>{trashedFiles.length}</span></button></nav>
      <div className="nav-label">LIBRARY</div><nav className="category-nav">{categories.slice(1).map((item) => { const Icon = item.icon; const count = data.files.filter((file) => file.category === item.name && file.status === "ready").length; return <button key={item.name} onClick={() => { setCategory(item.name); setCurrentFolder(""); stopFileSelection(); nav("vault"); }} className={view === "vault" && category === item.name ? "active" : ""}><Icon style={{ color: item.color }} /> {item.name}<span>{count}</span></button>; })}</nav>
      <div className="sidebar-bottom"><nav><button className={view === "accounts" ? "active" : ""} onClick={() => nav("accounts")}><Users /> Accounts</button><button className={view === "settings" ? "active" : ""} onClick={() => nav("settings")}><Settings /> Settings</button><button className={view === "about" ? "active" : ""} onClick={() => nav("about")}><Info /> About TiVault</button></nav><div className="vault-status"><div className="status-orb"><Cloud /></div><div><strong>{formatBytes(data.storedBytes)} stored</strong><span><span className="online-dot" /> {data.accounts.some((account) => account.connected) ? "Telegram connected" : "Reconnect Telegram"}</span></div></div><div className="developer-credit">Developed by <strong>H34TB145T</strong></div></div>
    </aside>
    <main className="main-content">
      <header className="topbar"><button className="icon-button mobile-menu" onClick={() => setMobileNav(!mobileNav)}><Menu /></button><div className="search"><Search /><input placeholder="Search your vault" value={query} onChange={(e) => setQuery(e.target.value)} /><kbd>⌘ K</kbd></div><div className="top-actions"><div className="connection-pill">{data.accounts.some((account) => account.connected) ? <><Wifi size={14} /> Online</> : <><WifiOff size={14} /> Telegram offline</>}</div>{view === "vault" && <button className="button ghost" onClick={() => setCreateFolderOpen(true)}><FolderPlus size={17} /> New folder</button>}<button className="button ghost" onClick={chooseFolderUpload}><FolderUp size={17} /> Upload folder</button><button className="button primary" onClick={choose}><Plus size={18} /> Add files</button><div className={`avatar ${avatarUrl ? "has-photo" : ""}`} title={primaryAccount?.name ?? "Telegram profile"}>{avatarUrl ? <img src={avatarUrl} alt={`${primaryAccount?.name ?? "Telegram"} profile`} /> : primaryAccount?.initials ?? <UserRound size={17} />}</div></div></header>
      {isLibraryView && <div className="vault-page"><div className="page-title"><div><span className="eyebrow">{view === "vault" ? "YOUR TELEGRAM CLOUD" : "SMART VIEW"}</span>{view === "vault" && currentFolder && <div className="breadcrumbs"><button onClick={() => { setCurrentFolder(""); stopFileSelection(); }}>{category}</button>{currentFolder.split("/").map((part, index, parts) => <span key={`${part}-${index}`}><ChevronRight size={12} /><button onClick={() => { setCurrentFolder(parts.slice(0, index + 1).join("/")); stopFileSelection(); }}>{part}</button></span>)}</div>}<h1>{libraryTitle}</h1><p>{scopedFiles.length} {scopedFiles.length === 1 ? "file" : "files"} · {formatBytes(scopedFiles.reduce((sum, file) => sum + file.size, 0))}</p></div><div className="page-tools">{fileSelectionMode ? <><span className="selection-count">{selectedFileIds.size} selected</span><button className="button ghost compact" disabled={!directFiles.length} onClick={() => setSelectedFileIds(allDirectFilesSelected ? new Set() : new Set(directFiles.map((file) => file.id)))}>{allDirectFilesSelected ? "Deselect all" : "Select all"}</button><button className="button ghost compact" disabled={!selectedFileIds.size} onClick={downloadSelectedFiles}><Download size={15} /> Download</button><button className="button danger compact" disabled={!selectedFileIds.size} onClick={deleteSelectedFiles}><Trash2 size={15} /> Delete</button><button className="button ghost compact" onClick={stopFileSelection}>Done</button></> : <>{view === "vault" && <><button className="button ghost compact" onClick={() => setCreateFolderOpen(true)}><FolderPlus size={15} /> New folder</button>{currentFolder && <button className="button ghost compact" onClick={() => downloadFolder(currentFolder)}><Download size={15} /> Download folder</button>}<button className="button ghost compact" onClick={chooseFolderUpload}><FolderUp size={15} /> Upload folder</button></>}<button className="button ghost compact" disabled={!directFiles.length} onClick={() => { setSelected(null); setFileSelectionMode(true); }}><CheckSquare2 size={15} /> Select</button></>}<select className="compact-select" value={tagFilter} onChange={(event) => setTagFilter(event.target.value)}><option value="">All tags</option>{availableTags.map((tag) => <option key={tag} value={tag}>{tag}</option>)}</select><select className="compact-select" value={sortOrder} onChange={(event) => setSortOrder(event.target.value as SortOrder)}><option value="newest">Newest</option><option value="oldest">Oldest</option><option value="name">Name</option><option value="largest">Largest</option><option value="smallest">Smallest</option><option value="type">Type</option></select><div className="segmented"><button className={layout === "grid" ? "active" : ""} onClick={() => setLayout("grid")}><Grid2X2 size={16} /></button><button className={layout === "list" ? "active" : ""} onClick={() => setLayout("list")}><List size={16} /></button></div></div></div>
        <section className="summary-strip"><div><span className="summary-icon blue"><Cloud /></span><span><small>Total cloud storage</small><strong>{formatBytes(data.storedBytes)}</strong></span></div><div><span className="summary-icon purple"><ShieldCheck /></span><span><small>Encrypted files</small><strong>{data.files.filter((f) => f.encrypted).length}</strong></span></div><div><span className="summary-icon green"><HardDrive /></span><span><small>Available offline</small><strong>{formatBytes(data.cacheUsed)}</strong></span></div><div><span className="summary-icon orange"><Zap /></span><span><small>Transfer mode</small><strong className="capitalize">{data.speedProfile}</strong></span></div></section>
        {visibleFolders.length === 0 && directFiles.length === 0 ? <EmptyState category={category} onUpload={choose} /> : layout === "grid" ? <div className="file-grid">{visibleFolders.map((folder) => <FolderCard key={folder.path} folder={folder} onOpen={() => { setCurrentFolder(folder.path); setSelected(null); stopFileSelection(); }} onDownload={() => downloadFolder(folder.path)} onShare={() => setShareFolder(folder)} onDelete={() => deleteFolder(folder)} />)}{directFiles.map((file) => <FileCard key={file.id} file={file} active={selected?.id === file.id} selectionMode={fileSelectionMode} checked={selectedFileIds.has(file.id)} onSelect={() => setSelected(file)} onToggleSelection={() => toggleFileSelection(file.id)} onDownload={() => api.downloadFile(file.id).then(refresh)} onDelete={() => deleteFile(file)} onFavorite={() => toggleFavorite(file)} />)}</div> : <div className="file-table"><div className="file-table-head"><span>Name</span><span>Type</span><span>Size</span><span>Account</span><span>Added</span><span>Status</span><span /></div>{visibleFolders.map((folder) => <FolderRow key={folder.path} folder={folder} onOpen={() => { setCurrentFolder(folder.path); setSelected(null); stopFileSelection(); }} onDownload={() => downloadFolder(folder.path)} onShare={() => setShareFolder(folder)} onDelete={() => deleteFolder(folder)} />)}{directFiles.map((file) => <FileRow key={file.id} file={file} active={selected?.id === file.id} selectionMode={fileSelectionMode} checked={selectedFileIds.has(file.id)} onSelect={() => setSelected(file)} onToggleSelection={() => toggleFileSelection(file.id)} onDownload={() => api.downloadFile(file.id).then(refresh)} onDelete={() => deleteFile(file)} onFavorite={() => toggleFavorite(file)} />)}</div>}
      </div>}
      {view === "transfers" && <div className="content-page"><div className="page-title"><div><span className="eyebrow">TRANSFER CENTRE</span><h1>Transfers</h1><p>Pause, resume and monitor every upload and download.</p></div><div className="page-tools">{transferSelectionMode ? <><span className="selection-count">{selectedTransferIds.size} selected</span><button className="button ghost compact" disabled={!historyTransfers.length} onClick={() => setSelectedTransferIds(allHistorySelected ? new Set() : new Set(historyTransfers.map((transfer) => transfer.id)))}>{allHistorySelected ? "Deselect all" : "Select all history"}</button><button className="button danger compact" disabled={!selectedTransferIds.size} onClick={deleteSelectedTransferHistory}><Trash2 size={15} /> Delete selected</button><button className="button ghost compact" onClick={stopTransferSelection}>Done</button></> : <><button className="button ghost" disabled={!historyTransfers.length} onClick={() => setTransferSelectionMode(true)}><CheckSquare2 size={16} /> Select history</button><button className="button danger" disabled={!historyTransfers.length} onClick={clearAllTransferHistory}><Trash2 size={16} /> Clear all history</button><button className="button ghost" onClick={chooseFolderUpload}><FolderUp size={16} /> Folder upload</button><button className="button primary" onClick={choose}><Upload size={16} /> File upload</button></>}</div></div><div className="panel"><div className="panel-head"><h3>Queue</h3><span>{data.transfers.length} transfers · {historyTransfers.length} history</span></div><div className="transfer-list full">{data.transfers.length ? data.transfers.map((transfer) => <TransferItem key={transfer.id} transfer={transfer} selectionMode={transferSelectionMode} checked={selectedTransferIds.has(transfer.id)} onToggleSelection={() => toggleTransferSelection(transfer.id)} onPause={() => api.pauseTransfer(transfer.id).then(refresh)} onResume={() => api.resumeTransfer(transfer.id).then(refresh)} onCancel={() => cancelTransfer(transfer)} onDismiss={() => dismissOneTransfer(transfer)} />) : <EmptyState category="All files" onUpload={choose} />}</div></div></div>}
      {view === "trash" && <RecycleBinView files={trashedFiles} onRestore={restoreTrashedFile} onDelete={permanentlyDeleteTrashedFile} onEmpty={emptyRecycleBin} />}
      {view === "watch" && <WatchFoldersView data={data} refresh={refresh} confirmAction={confirmAction} />}
      {view === "accounts" && <AccountsView data={data} openAdd={(account) => setAccountModal(account ?? "new")} refresh={refresh} confirmAction={confirmAction} />}
      {view === "settings" && <SettingsView data={data} onRefresh={refresh} onCacheCleared={() => setAvatarUrl(null)} setTheme={setTheme} onLock={(status) => { setLockStatus(status); setData(null); }} confirmAction={confirmAction} />}
      {view === "about" && <AboutView />}
    </main>
    {selected && <PreviewPane file={selected} onClose={() => setSelected(null)} onPreview={() => setPreviewFile(selected)} onShare={() => setShareFile(selected)} onDownload={() => api.downloadFile(selected.id).then(refresh)} onDelete={() => deleteFile(selected)} onManage={(action) => setManageFile({ file: selected, action })} onFavorite={() => toggleFavorite(selected)} onEditTags={() => setTagFile(selected)} />}
    {previewFile && <FilePreviewModal file={previewFile} onClose={() => setPreviewFile(null)} onDownload={() => api.downloadFile(previewFile.id).then(refresh)} />}
    {manageFile && <FileActionModal file={manageFile.file} action={manageFile.action} folders={data.folders} onClose={() => setManageFile(null)} onComplete={(file) => { setSelected(file); void refresh(); }} />}
    {tagFile && <TagEditorModal file={tagFile} onClose={() => setTagFile(null)} onSaved={() => { if (selected?.id === tagFile.id) setSelected(null); void refresh(); }} />}
    {shareFile && <ShareModal file={shareFile} confirmAction={confirmAction} onClose={() => setShareFile(null)} onQueued={() => { void refresh(); setView("transfers"); setSelected(null); }} />}
    {shareFolder && <ShareModal folder={shareFolder} folderFiles={data.files.filter((file) => file.status === "ready" && ((file.folderPath ?? "") === shareFolder.path || (file.folderPath ?? "").startsWith(`${shareFolder.path}/`)))} confirmAction={confirmAction} onClose={() => setShareFolder(null)} onQueued={() => { void refresh(); setView("transfers"); setSelected(null); setShareFolder(null); }} />}
    {dragging && <div className="drop-overlay"><div><Upload size={42} /><h2>Drop to upload</h2><p>TiVault will categorize, encrypt and chunk automatically.</p></div></div>}
    {uploadSelection && <UploadModal paths={uploadSelection.paths} folderRoot={uploadSelection.root} destinationFolder={uploadSelection.destinationFolder} accounts={data.accounts.filter((account) => account.connected)} onClose={() => setUploadSelection(null)} onQueued={refresh} />}
    {createFolderOpen && <CreateFolderModal parentPath={currentFolder} onClose={() => setCreateFolderOpen(false)} onCreate={createFolder} />}
    {accountModal && <AccountModal account={accountModal === "new" ? undefined : accountModal} onClose={() => setAccountModal(null)} onConnected={refresh} />}
    {confirmation && <ConfirmationModal confirmation={confirmation} onAnswer={answerConfirmation} />}
  </div>;
}

function AccountsView({ data, openAdd, refresh, confirmAction }: { data: Dashboard; openAdd: (account?: Account) => void; refresh: () => void; confirmAction: ConfirmAction }) {
  const disconnect = async (account: Account) => {
    if (!await confirmAction({ title: `Disconnect ${account.name}?`, message: "TiVault will close this local Telegram connection but preserve its protected local session, catalogue and vault. You can reconnect without rebuilding the account.", confirmLabel: "Disconnect", tone: "warning" })) return;
    try { await api.disconnectAccount(account.id); await refresh(); } catch (cause) { alert(String(cause)); }
  };
  const remove = async (account: Account) => {
    if (!await confirmAction({ title: `Remove ${account.name} from TiVault?`, message: "Telegram authorization will be revoked and its local session, catalogue entries, transfer history and watch-folder links will be erased. Files in Saved Messages are not deleted and can be recovered later with the recovery key.", confirmLabel: "Remove account" })) return;
    try { await api.removeAccount(account.id); await refresh(); } catch (cause) { alert(String(cause)); }
  };
  return <div className="content-page"><div className="page-title"><div><span className="eyebrow">VAULT PROFILES</span><h1>Telegram accounts</h1><p>Disconnect temporarily, or revoke and erase a local Telegram session.</p></div><button className="button primary" onClick={() => openAdd()}><Plus size={16} /> Connect account</button></div><div className="notice"><ShieldCheck size={17} /> Removing an account never silently deletes files from Telegram Saved Messages.</div><div className="account-grid">{data.accounts.map((account) => <div className="account-card" key={account.id}><div className="account-avatar" style={{ background: account.color }}>{account.initials}</div><div className="account-state">{account.connected ? <><Wifi size={14} /> Connected</> : <><WifiOff size={14} /> Offline</>}</div><h3>{account.name}</h3><p>{account.phone}</p><div className="account-stats"><span><small>Files</small><strong>{account.fileCount}</strong></span><span><small>Stored</small><strong>{formatBytes(account.storedBytes)}</strong></span></div>{account.connected ? <button className="button ghost wide" onClick={() => disconnect(account)}><LogOut size={15} /> Disconnect</button> : <button className="button ghost wide" onClick={() => openAdd(account)}><RefreshCw size={15} /> Reconnect account</button>}<button className="button danger wide" onClick={() => remove(account)}><UserX size={15} /> Remove account & session</button></div>)}<button className="add-account-card" onClick={() => openAdd()}><span><Plus /></span><strong>Connect another account</strong><small>Use a separate Telegram vault</small></button></div></div>;
}

function WatchFoldersView({ data, refresh, confirmAction }: { data: Dashboard; refresh: () => void; confirmAction: ConfirmAction }) {
  const add = async () => { const path = await api.chooseFolder(); if (!path || !data.accounts[0]) return; await api.addWatchFolder({ path, enabled: true, encrypt: true, accountId: data.accounts[0].id }); refresh(); };
  const remove = async (folder: WatchFolder) => {
    if (!await confirmAction({ title: "Stop watching this folder?", message: `“${folder.path.split(/[\\/]/).pop()}” will no longer upload new files automatically. Existing vault files and Telegram copies will remain.`, confirmLabel: "Stop watching", tone: "warning" })) return;
    try { await api.removeWatchFolder(folder.id); refresh(); } catch (error) { alert(String(error)); }
  };
  return <div className="content-page"><div className="page-title"><div><span className="eyebrow">AUTOMATION</span><h1>Watch folders</h1><p>New completed files are uploaded automatically. Partial downloads and temporary files are ignored.</p></div><button className="button primary" onClick={add}><Plus size={16} /> Add folder</button></div><div className="notice"><FolderSync size={18} /><span><strong>One-way protection</strong><br />Deleting a local file never removes its Telegram copy. Permanent cloud deletion always requires confirmation.</span></div><div className="watch-list">{data.watchFolders.length ? data.watchFolders.map((folder) => <WatchFolderRow key={folder.id} folder={folder} account={data.accounts.find((a) => a.id === folder.accountId)} onDelete={() => remove(folder)} />) : <div className="empty-state"><div className="empty-cloud"><FolderSync /></div><h3>No folders are being watched</h3><p>Add a downloads, camera, or project folder to protect new files automatically.</p><button className="button primary" onClick={add}><Plus size={16} /> Add first folder</button></div>}</div></div>;
}

function WatchFolderRow({ folder, account, onDelete }: { folder: WatchFolder; account?: Account; onDelete: () => void }) { return <div className="watch-row"><div className="watch-icon"><FolderSync /></div><div><strong>{folder.path.split(/[\\/]/).pop()}</strong><small>{folder.path}</small></div><span className="watch-account">{account?.name ?? "Unknown account"}</span><span>{folder.encrypt ? <><LockKeyhole size={13} /> Encrypted</> : "Standard"}</span><span>{folder.uploadedCount} uploaded</span><span>{folder.enabled ? "Enabled" : "Disabled"}</span><button className="icon-button danger" onClick={onDelete}><Trash2 size={16} /></button></div>; }
