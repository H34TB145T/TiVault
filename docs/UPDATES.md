# Signed TiVault updates

TiVault release artifacts use Tauri's mandatory updater signatures. This signature is independent from Apple or Microsoft commercial code signing: it lets an installed TiVault build reject an update that was not signed by the maintainer's protected updater key.

The public verification key and GitHub Releases endpoint are committed in `src-tauri/tauri.release.conf.json`. The encrypted private key and its password exist only in the maintainer's protected local key store and the repository secrets named `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.

Release builds set two non-secret compile markers, `TELEVAULT_UPDATE_PUBLIC_KEY` and `TELEVAULT_UPDATE_ENDPOINT`, which enable the updater runtime and Settings UI. The actual trusted key and HTTPS endpoint come from the release configuration.

## Release process

1. Update the version consistently in `package.json`, `src-tauri/Cargo.toml` and `src-tauri/tauri.conf.json`.
2. Run the complete local build and test suite.
3. Create and push a matching `v*` tag.
4. GitHub Actions builds Linux x64, Windows x64 and a universal macOS application with `src-tauri/tauri.release.conf.json`.
5. Each matrix job uploads signed updater artifacts and a platform-specific SHA-256 checksum file to a draft release.
6. The release is published only after every platform job succeeds. Tauri Action generates `latest.json` for the static updater endpoint.

Never commit, print or send the private updater key or password. Losing either prevents installed copies from trusting future updates. Rotate a key only before the first public release or through an update signed by the old key.
