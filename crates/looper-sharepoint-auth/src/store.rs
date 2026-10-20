//! Refresh tokens in the OS keychain; non-secret account records in `<data_dir>/accounts.json`.
//!
//! Development builds (without the `release` feature) keep refresh tokens in owner-only files under
//! `<data_dir>/dev-tokens/` instead, because macOS asks for keychain access again after every rebuild of an unsigned
//! binary. `LOOPER_SHAREPOINT_TOKEN_STORE=keychain` uses the keychain anyway. Packaged builds are built with
//! `release`, which compiles the file store out entirely.
//!
//! Known risk (M5): Windows Credential Manager caps a credential at 2,560 bytes and Entra refresh tokens can approach
//! that. Fallback if needed: a DPAPI-encrypted file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::AuthError;

const SERVICE: &str = "looper-sharepoint";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    /// Tenant as written in config (GUID or domain); used to match config to account.
    pub tenant: String,
    pub client_id: String,
    /// Tenant GUID from the ID token.
    pub tenant_id: String,
    pub username: String,
}

impl AccountRecord {
    fn keychain_user(&self) -> String {
        format!("{}/{}/{}", self.tenant_id, self.client_id, self.username)
    }
}

pub struct TokenStore {
    accounts_file: PathBuf,
    #[cfg(not(feature = "release"))]
    data_dir: PathBuf,
}

impl TokenStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            accounts_file: data_dir.join("accounts.json"),
            #[cfg(not(feature = "release"))]
            data_dir,
        }
    }

    fn load(&self) -> Result<Vec<AccountRecord>, AuthError> {
        match std::fs::read_to_string(&self.accounts_file) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| AuthError::Other(format!("{}: {e}", self.accounts_file.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, accounts: &[AccountRecord]) -> Result<(), AuthError> {
        if let Some(d) = self.accounts_file.parent() {
            std::fs::create_dir_all(d)?;
        }
        let json = serde_json::to_string_pretty(accounts).map_err(|e| AuthError::Other(e.to_string()))?;
        std::fs::write(&self.accounts_file, json)?;
        Ok(())
    }

    /// The signed-in account for this tenant + app, if any.
    pub fn account(&self, tenant: &str, client_id: &str) -> Result<Option<AccountRecord>, AuthError> {
        Ok(self.load()?.into_iter().find(|a| {
            (a.tenant.eq_ignore_ascii_case(tenant) || a.tenant_id.eq_ignore_ascii_case(tenant))
                && a.client_id.eq_ignore_ascii_case(client_id)
        }))
    }

    /// Record a successful sign-in, replacing any previous account for the same tenant + app.
    pub fn sign_in(&self, account: &AccountRecord, refresh_token: &str) -> Result<(), AuthError> {
        if let Some(old) = self.account(&account.tenant, &account.client_id)? {
            self.forget(&old)?;
        }
        self.save_refresh_token(account, refresh_token)?;
        let mut all = self.load()?;
        all.push(account.clone());
        self.save(&all)
    }

    pub fn forget(&self, account: &AccountRecord) -> Result<(), AuthError> {
        #[cfg(not(feature = "release"))]
        if let Some(files) = self.dev_files() {
            return files.delete(account).and_then(|_| self.forget_record(account));
        }
        match keyring::Entry::new(SERVICE, &account.keychain_user())?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(e.into()),
        }
        self.forget_record(account)
    }

    fn forget_record(&self, account: &AccountRecord) -> Result<(), AuthError> {
        let all: Vec<_> = self.load()?.into_iter().filter(|a| a != account).collect();
        self.save(&all)
    }

    pub fn refresh_token(&self, account: &AccountRecord) -> Result<Option<String>, AuthError> {
        #[cfg(not(feature = "release"))]
        if let Some(files) = self.dev_files() {
            return files.get(account);
        }
        match keyring::Entry::new(SERVICE, &account.keychain_user())?.get_password() {
            Ok(t) => Ok(Some(t)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_refresh_token(&self, account: &AccountRecord, token: &str) -> Result<(), AuthError> {
        #[cfg(not(feature = "release"))]
        if let Some(files) = self.dev_files() {
            return files.set(account, token);
        }
        keyring::Entry::new(SERVICE, &account.keychain_user())?.set_password(token)?;
        Ok(())
    }

    /// The development file store, unless `LOOPER_SHAREPOINT_TOKEN_STORE=keychain`.
    #[cfg(not(feature = "release"))]
    fn dev_files(&self) -> Option<dev::FileStore> {
        (std::env::var("LOOPER_SHAREPOINT_TOKEN_STORE").as_deref() != Ok("keychain"))
            .then(|| dev::FileStore { dir: self.data_dir.join("dev-tokens") })
    }
}

/// Development builds only: one owner-only file per account.
#[cfg(not(feature = "release"))]
mod dev {
    use std::io::Write;
    use std::path::PathBuf;

    use super::AccountRecord;
    use crate::AuthError;

    pub struct FileStore {
        pub dir: PathBuf,
    }

    impl FileStore {
        fn path(&self, account: &AccountRecord) -> PathBuf {
            use sha2::Digest;
            let h = sha2::Sha256::digest(account.keychain_user().as_bytes());
            self.dir.join(h.iter().take(12).map(|b| format!("{b:02x}")).collect::<String>())
        }

        pub fn get(&self, account: &AccountRecord) -> Result<Option<String>, AuthError> {
            match std::fs::read_to_string(self.path(account)) {
                Ok(t) => Ok(Some(t)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        }

        pub fn set(&self, account: &AccountRecord, token: &str) -> Result<(), AuthError> {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| tracing::debug!(dir = %self.dir.display(), "development build: sign-in stored in a file, not the keychain"));
            std::fs::create_dir_all(&self.dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
                std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
                // Write a new owner-only file and rename it over the old one, so the token is never world-readable.
                let tmp = self.path(account).with_extension("tmp");
                let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
                f.write_all(token.as_bytes())?;
                f.sync_all()?;
                std::fs::rename(&tmp, self.path(account))?;
            }
            #[cfg(not(unix))]
            std::fs::File::create(self.path(account))?.write_all(token.as_bytes())?;
            Ok(())
        }

        pub fn delete(&self, account: &AccountRecord) -> Result<(), AuthError> {
            match std::fs::remove_file(self.path(account)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        }
    }
}

#[cfg(all(test, unix, not(feature = "release")))]
mod tests {
    use super::*;

    #[test]
    fn dev_file_store_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lsp-store-{}", std::process::id()));
        let files = dev::FileStore { dir: dir.join("dev-tokens") };
        let acct = AccountRecord { tenant: "t".into(), client_id: "c".into(), tenant_id: "tid".into(), username: "u".into() };
        assert_eq!(files.get(&acct).unwrap(), None);
        files.set(&acct, "secret-1").unwrap();
        files.set(&acct, "secret-2").unwrap();
        assert_eq!(files.get(&acct).unwrap().as_deref(), Some("secret-2"));
        let entry = std::fs::read_dir(&files.dir).unwrap().next().unwrap().unwrap();
        assert_eq!(entry.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&files.dir).unwrap().permissions().mode() & 0o777, 0o700);
        files.delete(&acct).unwrap();
        assert_eq!(files.get(&acct).unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
