# TiVault architecture

## Data flow

```text
React UI (Tauri IPC or localhost HTTP)
        │
        ▼
Rust Core ── SQLite catalogue / transfer journal
   │   │
   │   ├── streaming chunker + SHA-256
   │   ├── optional authenticated encryption
   │   └── preview/cache and Watch Folder scanner
   │
   ▼
Telegram MTProto client
   │
   ▼
Saved Messages: chunk documents + manifest document
```

The browser interface binds only to `127.0.0.1:7468`. It cannot run independently of the desktop core because a normal browser cannot safely retain Telegram authorization sessions or monitor arbitrary folders.

## Upload format

Each logical file has a UUID. Unencrypted files under the safe Telegram size are uploaded directly. Larger files are split into documents no larger than 1 GB, keeping temporary local disk use bounded.

Encrypted files use the `TVENC001` container:

```text
8-byte magic
repeat until EOF:
  4-byte little-endian plaintext length
  24-byte XChaCha20 nonce
  ciphertext plus 16-byte Poly1305 authentication tag
```

Every file receives a random 256-bit key. The key is wrapped with the vault recovery key using XChaCha20-Poly1305. The version-2 manifest contains ordered Telegram message IDs, chunk sizes and SHA-256 hashes. For encrypted uploads, the Telegram-visible manifest replaces original filename, MIME type and category with opaque values and places the real metadata in an XChaCha20-Poly1305 sealed field. Recovery scans these manifest documents to rebuild missing local catalogue entries.

## Persistence

The SQLite database uses WAL mode and stores:

- account/profile and session locations;
- logical file metadata;
- Telegram message IDs for every chunk;
- resumable transfer state;
- Watch Folders and already-seen file fingerprints;
- UI, cache, app-lock and speed preferences.

Active transfers are changed to paused during startup so a crash never silently reports success.

## Preview flow

Preview sessions use a random 128-bit capability token and expire after a configurable idle interval. The content endpoint is available only on `127.0.0.1`. Images and small PDFs can be returned as one bounded response; audio, video and large content use byte ranges. Vault thumbnails use the same lazy range-backed preview sessions and release their tokens when scrolled out of view.

Remote files are fetched in independently useful 8 MiB blocks. Encrypted blocks map to the corresponding authenticated `TVENC001` frame and are decrypted only after Poly1305 verification. Plaintext preview blocks are stored in a private per-session directory, evicted with an LRU limit of 128–512 MiB, and deleted when the preview closes, expires or the app restarts. Text is escaped by React. Supported Word-like documents are converted to plain text by macOS `textutil`; macros and scripts are not executed.

## Telegram sharing flow

The UI resolves an exact public username or lists existing private chats. Resolution creates a random, single-use, five-minute capability bound to the source file, Telegram account and destination chat. The UI then asks for a second confirmation before queuing a transfer.

TiVault verifies or reconstructs the original file locally and sends it as a normal Telegram document. Encrypted vault files require an explicit readable-copy confirmation and are decrypted only inside a private temporary directory. Each source chunk and the final plaintext hash are checked, temporary data is removed on completion, cancellation or error, and a late cancellation removes a just-sent destination message when possible.

Folder sharing verifies one recipient capability, requires every contained file to use the same Telegram account, and creates one independently cancellable transfer per file. Telegram chats do not have a portable folder object, so the relative TiVault folder path is included in each document caption.

## Transfer strategy

TiVault streams input and uses bounded buffers. It never loads a complete large file into memory. Telegram's MTProto client performs each document's internal 512 KB upload-part scheduling. TiVault controls concurrency at the logical-file level and always respects server wait errors.

Before an upload begins, TiVault checks for a ready file with the same size. Only then does it stream a SHA-256 pass; Telegram transfer is skipped only for an exact same-account hash match. Logical copies reuse immutable verified Telegram chunk messages but receive a new manifest. Reference-aware deletion preserves shared chunks until the last logical copy is removed.
