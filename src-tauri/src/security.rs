use crate::error::{AppError, AppResult};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{aead::Aead, KeyInit, XChaCha20Poly1305, XNonce};
use rand::RngCore;
use std::fs;
use std::path::{Path, PathBuf};
use zeroize::Zeroize;

const KEYRING_SERVICE: &str = "app.televault.desktop";

#[derive(Clone, Default)]
pub struct TelegramCredentialStore;

impl TelegramCredentialStore {
    fn entry(account_id: &str) -> AppResult<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, &format!("telegram-api-hash:{account_id}")).map_err(
            |_| AppError::Crypto("The operating-system credential vault is unavailable".into()),
        )
    }

    pub fn api_hash(&self, account_id: &str) -> AppResult<Option<String>> {
        match Self::entry(account_id)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(AppError::Crypto(
                "TiVault could not read this Telegram API hash from the operating-system credential vault"
                    .into(),
            )),
        }
    }

    pub fn store_api_hash(&self, account_id: &str, api_hash: &str) -> AppResult<()> {
        if api_hash.trim().len() < 8 {
            return Err(AppError::Crypto(
                "The Telegram API hash is invalid and was not stored".into(),
            ));
        }
        let entry = Self::entry(account_id)?;
        entry.set_password(api_hash).map_err(|_| {
            AppError::Crypto(
                "TiVault could not protect the Telegram API hash in the operating-system credential vault"
                    .into(),
            )
        })?;
        let verified = entry.get_password().map_err(|_| {
            AppError::Crypto(
                "TiVault could not verify the Telegram API hash after storing it".into(),
            )
        })?;
        if verified != api_hash {
            return Err(AppError::Crypto(
                "The operating-system credential vault did not verify the Telegram API hash".into(),
            ));
        }
        Ok(())
    }

    pub fn remove_api_hash(&self, account_id: &str) -> AppResult<()> {
        match Self::entry(account_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(AppError::Crypto(
                "TiVault could not erase the Telegram API hash from the operating-system credential vault"
                    .into(),
            )),
        }
    }
}

#[derive(Clone)]
pub struct MasterKeyStore {
    path: PathBuf,
    key: [u8; 32],
    keychain_backed: bool,
}

impl MasterKeyStore {
    #[cfg(test)]
    pub fn load_or_create_for_test(data_dir: impl AsRef<Path>) -> AppResult<Self> {
        let path = data_dir.as_ref().join("vault.test.key");
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        fs::write(&path, key)?;
        set_private_permissions(&path)?;
        Ok(Self {
            path,
            key,
            keychain_backed: false,
        })
    }

    pub fn load_or_create(data_dir: impl AsRef<Path>) -> AppResult<Self> {
        let path = data_dir.as_ref().join("vault.key");
        let marker = data_dir.as_ref().join("vault.keychain");
        let keychain = keyring::Entry::new(KEYRING_SERVICE, "vault-master-key").ok();

        if marker.exists() {
            let encoded = keychain
                .as_ref()
                .ok_or_else(|| AppError::Crypto("The operating-system keychain is unavailable".into()))?
                .get_password()
                .map_err(|_| AppError::Crypto("TiVault could not read its recovery key from the operating-system keychain".into()))?;
            let key = decode_key(&encoded)?;
            return Ok(Self {
                path,
                key,
                keychain_backed: true,
            });
        }

        if !path.exists() {
            if let Some(entry) = keychain.as_ref() {
                if let Ok(encoded) = entry.get_password() {
                    if let Ok(key) = decode_key(&encoded) {
                        fs::write(&marker, b"keyring-v1")?;
                        set_private_permissions(&marker)?;
                        return Ok(Self {
                            path,
                            key,
                            keychain_backed: true,
                        });
                    }
                }
            }
        }

        let mut key = if path.exists() {
            let bytes = fs::read(&path)?;
            bytes
                .try_into()
                .map_err(|_| AppError::Crypto("The local vault key is invalid".into()))?
        } else {
            let mut generated = [0u8; 32];
            rand::rng().fill_bytes(&mut generated);
            generated
        };

        if let Some(entry) = keychain {
            let encoded = URL_SAFE_NO_PAD.encode(key);
            if entry.set_password(&encoded).is_ok()
                && entry.get_password().ok().as_deref() == Some(encoded.as_str())
            {
                fs::write(&marker, b"keyring-v1")?;
                set_private_permissions(&marker)?;
                if path.exists() {
                    fs::remove_file(&path)?;
                }
                return Ok(Self {
                    path,
                    key,
                    keychain_backed: true,
                });
            }
        }

        if !path.exists() {
            fs::write(&path, key)?;
        }
        set_private_permissions(&path)?;
        let result = Self {
            path,
            key,
            keychain_backed: false,
        };
        key.zeroize();
        Ok(result)
    }

    pub fn is_ready(&self) -> bool {
        self.keychain_backed || self.path.exists()
    }
    pub fn keychain_backed(&self) -> bool {
        self.keychain_backed
    }
    pub fn export_recovery(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key)
    }

    pub fn verify_recovery(&self, candidate: &str) -> bool {
        let Ok(decoded) = URL_SAFE_NO_PAD.decode(candidate.trim()) else {
            return false;
        };
        if decoded.len() != self.key.len() {
            return false;
        }
        let mut difference = 0u8;
        for (left, right) in decoded.iter().zip(self.key.iter()) {
            difference |= left ^ right;
        }
        let mut decoded = decoded;
        decoded.zeroize();
        difference == 0
    }

    pub fn wrap_file_key(&self, file_key: &[u8; 32]) -> AppResult<(String, String)> {
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let mut nonce = [0u8; 24];
        rand::rng().fill_bytes(&mut nonce);
        let wrapped = cipher
            .encrypt(XNonce::from_slice(&nonce), file_key.as_ref())
            .map_err(|_| AppError::Crypto("Could not protect the file key".into()))?;
        Ok((
            URL_SAFE_NO_PAD.encode(wrapped),
            URL_SAFE_NO_PAD.encode(nonce),
        ))
    }

    pub fn unwrap_file_key(&self, wrapped: &str, nonce: &str) -> AppResult<[u8; 32]> {
        let wrapped = URL_SAFE_NO_PAD
            .decode(wrapped)
            .map_err(|_| AppError::Crypto("Invalid wrapped key".into()))?;
        let nonce = URL_SAFE_NO_PAD
            .decode(nonce)
            .map_err(|_| AppError::Crypto("Invalid key nonce".into()))?;
        if nonce.len() != 24 {
            return Err(AppError::Crypto("Invalid key nonce length".into()));
        }
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let plain = cipher
            .decrypt(XNonce::from_slice(&nonce), wrapped.as_ref())
            .map_err(|_| AppError::Crypto("The recovery key cannot unlock this file".into()))?;
        plain
            .try_into()
            .map_err(|_| AppError::Crypto("Invalid file key length".into()))
    }

    pub fn seal_metadata(&self, plaintext: &[u8]) -> AppResult<(String, String)> {
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let mut nonce = [0u8; 24];
        rand::rng().fill_bytes(&mut nonce);
        let sealed = cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext)
            .map_err(|_| AppError::Crypto("Could not protect private file metadata".into()))?;
        Ok((
            URL_SAFE_NO_PAD.encode(sealed),
            URL_SAFE_NO_PAD.encode(nonce),
        ))
    }

    pub fn open_metadata(&self, sealed: &str, nonce: &str) -> AppResult<Vec<u8>> {
        let sealed = URL_SAFE_NO_PAD
            .decode(sealed)
            .map_err(|_| AppError::Crypto("Invalid private metadata".into()))?;
        let nonce = URL_SAFE_NO_PAD
            .decode(nonce)
            .map_err(|_| AppError::Crypto("Invalid private metadata nonce".into()))?;
        if nonce.len() != 24 {
            return Err(AppError::Crypto(
                "Invalid private metadata nonce length".into(),
            ));
        }
        XChaCha20Poly1305::new((&self.key).into())
            .decrypt(XNonce::from_slice(&nonce), sealed.as_ref())
            .map_err(|_| AppError::Crypto("The recovery key cannot open private metadata".into()))
    }
}

fn decode_key(encoded: &str) -> AppResult<[u8; 32]> {
    URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| AppError::Crypto("The keychain recovery key is invalid".into()))?
        .try_into()
        .map_err(|_| AppError::Crypto("The keychain recovery key has an invalid length".into()))
}

pub fn harden_private_tree(path: &Path) -> AppResult<()> {
    fs::create_dir_all(path)?;
    set_private_directory_permissions(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            harden_private_tree(&entry.path())?;
        } else if metadata.is_file() {
            set_private_file_permissions(&entry.path())?;
        }
    }
    Ok(())
}

pub fn set_private_directory_permissions(path: &Path) -> AppResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    set_windows_owner_only_acl(path, true)?;
    Ok(())
}

pub fn set_private_file_permissions(path: &Path) -> AppResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    set_windows_owner_only_acl(path, false)?;
    Ok(())
}

#[cfg(windows)]
fn set_windows_owner_only_acl(path: &Path, directory: bool) -> AppResult<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
        SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SUB_CONTAINERS_AND_OBJECTS_INHERIT,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    let mut wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut owner: PSID = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();

    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32).into());
    }

    let explicit = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: if directory {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        },
        Trustee: windows_sys::Win32::Security::Authorization::TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: owner.cast(),
        },
    };
    let mut acl: *mut ACL = null_mut();
    let acl_status = unsafe { SetEntriesInAclW(1, &explicit, null(), &mut acl) };
    if acl_status != ERROR_SUCCESS {
        unsafe {
            LocalFree(descriptor);
        }
        return Err(std::io::Error::from_raw_os_error(acl_status as i32).into());
    }

    let set_status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl,
            null(),
        )
    };
    unsafe {
        LocalFree(acl.cast());
        LocalFree(descriptor);
    }
    if set_status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(set_status as i32).into());
    }
    Ok(())
}

fn set_private_permissions(path: &Path) -> AppResult<()> {
    set_private_file_permissions(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exported_recovery_key_is_verified_without_accepting_malformed_keys() {
        let temp = tempfile::tempdir().unwrap();
        let store = MasterKeyStore::load_or_create_for_test(temp.path()).unwrap();
        assert!(store.verify_recovery(&store.export_recovery()));
        assert!(store.verify_recovery(&format!("  {}  ", store.export_recovery())));
        assert!(!store.verify_recovery("not-a-recovery-key"));
        let mut different = store.export_recovery();
        different.replace_range(0..1, if &different[0..1] == "A" { "B" } else { "A" });
        assert!(!store.verify_recovery(&different));
    }

    #[cfg(unix)]
    #[test]
    fn private_tree_permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("sessions/account");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("session.bin");
        fs::write(&file, b"private").unwrap();

        harden_private_tree(temp.path()).unwrap();

        assert_eq!(
            fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
