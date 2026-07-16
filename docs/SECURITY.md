# Security policy and model

## Reporting vulnerabilities

Do not open a public issue containing credentials, session files, private filenames or recovery keys. Until a dedicated security address is configured, open a minimal issue requesting a private maintainer contact.

## Encryption boundaries

- Encrypted uploads are transformed before Telegram receives any file bytes.
- Each encrypted frame is authenticated; modified or truncated data fails reconstruction.
- Every Telegram chunk and final plaintext is checked with SHA-256.
- Telegram cloud chats are not end-to-end encrypted when the per-upload encryption toggle is off.
- “Send via Telegram” creates a separate, normal readable chat document. For an encrypted vault file, TiVault requires an explicit warning and decrypts a temporary copy locally; the recipient never receives the recovery key.
- File sizes, transfer timing, chunk counts and TiVault marker captions remain visible to Telegram.

The recovery key is generated locally and stored through the operating system credential vault: macOS Keychain, Windows Credential Manager or Linux Secret Service. A successful migration is verified before TiVault removes the legacy key file. If a credential vault is unavailable on first setup, TiVault keeps a mode-`0600` fallback key and reports that state in Settings. Once the keychain marker exists, failure to read the credential vault is fatal; TiVault never silently generates a replacement key.

The optional app lock stores only an Argon2id password verifier. When locked, both Tauri commands and localhost API routes reject vault operations. The idle timeout is driven by real keyboard and pointer activity; background dashboard polling does not keep the app unlocked. App lock is a privacy barrier inside the application, not a substitute for OS login security or full-disk encryption.

Telegram API hashes are stored per account in macOS Keychain, Windows Credential Manager or Linux Secret Service. Existing SQLite values are migrated only after the credential vault write is verified, then scrubbed from the catalogue. If a platform credential vault is temporarily unavailable, TiVault retains the legacy value rather than destroying a working login. Telegram session databases remain in the private application-data directory. TiVault enforces owner-only modes on Unix and a protected owner-only ACL on Windows for its private directory tree and SQLite files. Any process already running as the same OS user may still be able to read application data, so full-disk encryption and a locked OS account remain important.

## Web companion

The web server binds only to IPv4 loopback. CORS permits the packaged origin and local development origin. It is not intended to be exposed through router port forwarding, a public reverse proxy or an untrusted multi-user computer.

Preview URLs contain random 128-bit capability tokens, expire after a configurable 5–60 idle minutes and are never public links. Preview responses disable caching and MIME sniffing. On-disk preview blocks use private permissions, a configurable 128–512 MiB LRU limit and automatic removal on close, expiry, manual cache clearing or restart. Video and audio are not autoplayed. PDF frames are sandboxed, and Office documents are rendered only as locally extracted plain text.

Telegram recipient capabilities are single-use, expire after five minutes and are bound to a specific file or folder anchor, account and chat. The visible recipient identity must be confirmed before sending. Folder sharing is rejected when its files span multiple Telegram accounts. Telegram bots can retain or process received files outside Telegram, so the UI displays an additional bot warning.

Sharing may temporarily require the plaintext file size plus one Telegram part and a free-space reserve. The share workspace is private and removed on success, cancellation, failure or restart. A recipient's copy is independent: later deleting the vault file from Saved Messages cannot revoke a copy already delivered to another chat.

## Recovery

Losing the vault recovery key makes encrypted files unrecoverable. Export it once, store it outside Telegram, and never include it in bug reports or screenshots.

New uploads write a versioned manifest document to Saved Messages. Encrypted manifests seal the original filename, folder, MIME type and category with the vault recovery key. Recovery scans only TiVault-labelled manifest documents, limits each manifest to 2 MiB, validates identifiers, hashes and ordered chunk maps, verifies wrapped encryption keys, and imports only missing file IDs. The scan never forwards or deletes Telegram messages.
