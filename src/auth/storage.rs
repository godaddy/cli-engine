//! Injectable credential storage backends.
//!
//! Auth providers persist credentials through the [`CredentialStorage`] trait
//! rather than talking to a keychain or the filesystem directly. This decouples
//! *what* is stored (a provider's serialized token) from *where* it is stored,
//! so a single storage backend can be shared across providers and swapped out —
//! for tests, or to disable the system keychain on machines where it is
//! unavailable (headless Linux, WSL).
//!
//! Backends map one-to-one onto [`CredentialStore`](crate::config::CredentialStore) modes:
//!
//! - [`FileStorage`] — unencrypted JSON under the config base directory. Always
//!   available; needs no system dependencies.
//! - `KeyringStorage` — the system keychain only (requires the `pkce-auth`
//!   feature and the `keyring` crate).
//! - `AutoStorage` — keychain with a transparent file fallback when the
//!   keychain backend is unavailable (requires `pkce-auth`).
//!
//! [`default_storage`] picks a backend from the resolved
//! [`CredentialStore`](crate::config::CredentialStore) mode (CLI flag, env var,
//! config file, or the `Keyring` default); see [`crate::config`].

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::config::CredentialStore;
use crate::fs::{config_base_dir, is_safe_path_component};

/// Identifies a single stored credential.
///
/// Backends derive their storage location from this key: the keychain service
/// name and the file path are both functions of `(app_id, provider, env)`.
#[derive(Clone, Copy, Debug)]
pub struct CredentialKey<'key> {
    /// Application id; namespaces credentials across CLIs sharing a keychain.
    pub app_id: &'key str,
    /// Auth provider name.
    pub provider: &'key str,
    /// Target environment name.
    pub env: &'key str,
}

impl<'key> CredentialKey<'key> {
    /// Creates a key from its parts.
    #[must_use]
    pub fn new(app_id: &'key str, provider: &'key str, env: &'key str) -> Self {
        Self {
            app_id,
            provider,
            env,
        }
    }
}

/// A pluggable place to persist a provider's serialized credential.
///
/// Values are opaque strings (typically JSON); the backend never interprets
/// them, so it stays independent of any provider's token shape. Callers own
/// (de)serialization and any validity/expiry checks.
#[async_trait]
pub trait CredentialStorage: Send + Sync + std::fmt::Debug {
    /// Loads the stored blob for `key`, or `None` when absent or unreadable.
    ///
    /// Backends own the policy for distinguishing "no entry" from "store
    /// unavailable" (see `AutoStorage`); both collapse to `None` here.
    async fn load(&self, key: &CredentialKey<'_>) -> Option<String>;

    /// Persists `value` for `key`, replacing any existing value.
    ///
    /// # Errors
    /// Returns an error when the backend cannot durably store the value.
    async fn save(&self, key: &CredentialKey<'_>, value: &str) -> Result<()>;

    /// Removes the blob for `key`. Best-effort: absence is not an error.
    async fn delete(&self, key: &CredentialKey<'_>);

    /// Lists environment names with stored credentials, if the backend supports
    /// enumeration. The default returns an empty list (the keychain cannot be
    /// enumerated by service prefix).
    ///
    /// # Errors
    /// Returns an error when enumeration is attempted but fails.
    async fn list(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Unencrypted file-based credential storage.
///
/// Stores each credential as JSON at
/// `<config-base>/<app>/credentials/<provider>-<env>.json`, where `<app>` is the
/// key's `app_id` (or `provider` when `app_id` is empty) and `<config-base>` is
/// resolved by [`config_base_dir`]. On Unix the file is created `0600` and the
/// parent directory is best-effort restricted to `0700`.
///
/// Credentials are written in clear text, so prefer the system keychain where
/// one is available.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileStorage;

impl FileStorage {
    /// Creates a file-storage backend.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Resolves the on-disk path for `key`, or `None` when the config base
    /// directory is unavailable or any key component is unsafe as a path
    /// segment.
    fn path_for(key: &CredentialKey<'_>) -> Option<std::path::PathBuf> {
        let app = if key.app_id.is_empty() {
            key.provider
        } else {
            key.app_id
        };
        if !is_safe_path_component(app)
            || !is_safe_path_component(key.provider)
            || !is_safe_path_component(key.env)
        {
            tracing::warn!(
                app,
                provider = key.provider,
                env = key.env,
                "refusing credential path with unsafe component"
            );
            return None;
        }
        let base = config_base_dir()?;
        Some(
            base.join(app)
                .join("credentials")
                .join(format!("{}-{}.json", key.provider, key.env)),
        )
    }
}

#[async_trait]
impl CredentialStorage for FileStorage {
    async fn load(&self, key: &CredentialKey<'_>) -> Option<String> {
        let path = Self::path_for(key)?;
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "credential file read failed");
                None
            }
        }
    }

    async fn save(&self, key: &CredentialKey<'_>, value: &str) -> Result<()> {
        let path = Self::path_for(key).ok_or_else(|| {
            crate::error::CliCoreError::message("could not determine credential file path")
        })?;
        let value = value.to_owned();
        tokio::task::spawn_blocking(move || crate::fs::write_string_atomic(&path, &value))
            .await
            .map_err(|e| {
                crate::error::CliCoreError::message(format!(
                    "credential file write task {}: {e}",
                    if e.is_cancelled() {
                        "cancelled"
                    } else {
                        "panicked"
                    }
                ))
            })?
    }

    async fn delete(&self, key: &CredentialKey<'_>) {
        let Some(path) = Self::path_for(key) else {
            return;
        };
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to delete credential file");
            }
        }
    }
}

#[cfg(feature = "pkce-auth")]
pub use keychain::{AutoStorage, KeyringStorage};

#[cfg(feature = "pkce-auth")]
mod keychain {
    use super::{CredentialKey, CredentialStorage, FileStorage, Result, async_trait};

    const KEYCHAIN_USER: &str = "token";

    /// Derives the keychain service name for `key`:
    /// `<app_id>/<provider>/<env>`, or `<provider>/<env>` when `app_id` is empty.
    fn keychain_service(key: &CredentialKey<'_>) -> String {
        if key.app_id.is_empty() {
            format!("{}/{}", key.provider, key.env)
        } else {
            format!("{}/{}/{}", key.app_id, key.provider, key.env)
        }
    }

    /// System-keychain credential storage.
    ///
    /// Reads and writes the OS keychain only. A `load` returns `None` both when
    /// no entry exists and when the keychain backend is unavailable; a `save`
    /// failure is a hard error. No file is ever written — use [`AutoStorage`]
    /// for a file fallback or [`super::FileStorage`] to skip the keychain.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct KeyringStorage;

    impl KeyringStorage {
        /// Creates a keychain-storage backend.
        #[must_use]
        pub fn new() -> Self {
            Self
        }

        /// Three-state keychain read used by [`AutoStorage`] to decide whether
        /// to consult a file fallback.
        ///
        /// `Some(Some(json))` = entry found; `Some(None)` = keychain reachable
        /// but empty; `None` = keychain backend unavailable.
        pub(super) async fn read_three_state(
            &self,
            key: &CredentialKey<'_>,
        ) -> Option<Option<String>> {
            let service = keychain_service(key);
            match tokio::task::spawn_blocking({
                let service = service.clone();
                move || keychain_read_blocking(&service, KEYCHAIN_USER)
            })
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    let reason = if e.is_cancelled() {
                        "cancelled"
                    } else {
                        "panicked"
                    };
                    tracing::warn!(service, error = %e, reason, "keychain read task failed");
                    None
                }
            }
        }

        /// Writes to the keychain, returning whether the write succeeded.
        pub(super) async fn write_raw(&self, key: &CredentialKey<'_>, value: &str) -> bool {
            let service = keychain_service(key);
            let value = value.to_owned();
            match tokio::task::spawn_blocking({
                let service = service.clone();
                move || keychain_write_blocking(&service, KEYCHAIN_USER, &value)
            })
            .await
            {
                Ok(saved) => saved,
                Err(e) => {
                    let reason = if e.is_cancelled() {
                        "cancelled"
                    } else {
                        "panicked"
                    };
                    tracing::warn!(service, error = %e, reason, "keychain write task failed");
                    false
                }
            }
        }

        /// Best-effort keychain entry deletion.
        pub(super) async fn delete_entry(&self, key: &CredentialKey<'_>) {
            let service = keychain_service(key);
            let service_for_warn = service.clone();
            if let Err(e) =
                tokio::task::spawn_blocking(move || match keyring::Entry::new(&service, KEYCHAIN_USER) {
                    Err(e) => {
                        tracing::warn!(service, error = %e, "keychain entry creation failed on delete");
                    }
                    Ok(entry) => match entry.delete_credential() {
                        Ok(()) | Err(keyring::Error::NoEntry) => {}
                        Err(e) => {
                            tracing::warn!(service, error = %e, "keychain delete failed");
                        }
                    },
                })
                .await
            {
                let reason = if e.is_cancelled() {
                    "cancelled"
                } else {
                    "panicked"
                };
                tracing::warn!(service = service_for_warn, error = %e, reason, "keychain delete task failed");
            }
        }
    }

    #[async_trait]
    impl CredentialStorage for KeyringStorage {
        async fn load(&self, key: &CredentialKey<'_>) -> Option<String> {
            // Collapse "no entry" and "unavailable" to None: keychain-only mode
            // never falls back to a file.
            self.read_three_state(key).await.flatten()
        }

        async fn save(&self, key: &CredentialKey<'_>, value: &str) -> Result<()> {
            if self.write_raw(key, value).await {
                Ok(())
            } else {
                Err(crate::error::CliCoreError::message(
                    "failed to save token to keychain — check logs for the underlying error, \
                     ensure your system keychain (e.g. gnome-keyring, macOS Keychain) is running \
                     and unlocked, or select file storage (credential store \"file\" or \"auto\")",
                ))
            }
        }

        async fn delete(&self, key: &CredentialKey<'_>) {
            self.delete_entry(key).await;
        }
    }

    /// Keychain storage with a transparent unencrypted-file fallback.
    ///
    /// Preferred when a keychain is usually present but may be missing (WSL,
    /// headless sessions). Behavior:
    /// - `load`: keychain entry present → use it; keychain reachable but empty →
    ///   `None` (the file is stale or absent, force re-auth); keychain
    ///   unavailable → read the file.
    /// - `save`: try the keychain; on success remove any stale file; on failure
    ///   write the file.
    /// - `delete`: remove from both the keychain and the file.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct AutoStorage {
        file: FileStorage,
        keyring: KeyringStorage,
    }

    impl AutoStorage {
        /// Creates an auto (keychain-with-file-fallback) backend.
        #[must_use]
        pub fn new() -> Self {
            Self {
                file: FileStorage::new(),
                keyring: KeyringStorage::new(),
            }
        }
    }

    #[async_trait]
    impl CredentialStorage for AutoStorage {
        async fn load(&self, key: &CredentialKey<'_>) -> Option<String> {
            match self.keyring.read_three_state(key).await {
                // Keychain has the entry.
                Some(Some(json)) => Some(json),
                // Keychain is reachable but empty: skip the file and force login.
                Some(None) => None,
                // Keychain backend unavailable: fall back to the file.
                None => self.file.load(key).await,
            }
        }

        async fn save(&self, key: &CredentialKey<'_>, value: &str) -> Result<()> {
            if self.keyring.write_raw(key, value).await {
                // Keychain is the source of truth now; drop any stale file copy.
                self.file.delete(key).await;
                return Ok(());
            }
            self.file.save(key, value).await
        }

        async fn delete(&self, key: &CredentialKey<'_>) {
            self.keyring.delete(key).await;
            self.file.delete(key).await;
        }
    }

    /// Reads a token JSON string from the system keychain. Sync; call inside
    /// `spawn_blocking`.
    fn keychain_read_blocking(service: &str, user: &str) -> Option<Option<String>> {
        match keyring::Entry::new(service, user) {
            Err(e) => {
                tracing::warn!(service, error = %e, "keychain entry creation failed");
                None
            }
            Ok(entry) => match entry.get_password() {
                Err(keyring::Error::NoEntry) => {
                    tracing::debug!(service, "no stored token in keychain");
                    Some(None)
                }
                Err(e) => {
                    tracing::warn!(service, error = %e, "keychain read failed");
                    None
                }
                Ok(json) => Some(Some(json)),
            },
        }
    }

    /// Writes a token JSON string to the system keychain. Sync; call inside
    /// `spawn_blocking`.
    fn keychain_write_blocking(service: &str, user: &str, json: &str) -> bool {
        match keyring::Entry::new(service, user) {
            Err(e) => {
                tracing::warn!(service, error = %e, "keychain entry creation failed");
                false
            }
            Ok(entry) => match entry.set_password(json) {
                Err(e) => {
                    tracing::warn!(service, error = %e, "keychain write failed");
                    false
                }
                Ok(()) => {
                    tracing::debug!(service, "token saved to keychain");
                    true
                }
            },
        }
    }
}

/// Builds the credential storage backend for `mode`.
///
/// `File` always yields a [`FileStorage`]. `Keyring`/`Auto` yield the keychain
/// backends when the `pkce-auth` feature is enabled; without it they log a
/// warning and degrade to [`FileStorage`], since no keychain backend is
/// compiled in.
#[must_use]
pub fn storage_for(mode: CredentialStore) -> Arc<dyn CredentialStorage> {
    match mode {
        CredentialStore::File => Arc::new(FileStorage::new()),
        #[cfg(feature = "pkce-auth")]
        CredentialStore::Keyring => Arc::new(KeyringStorage::new()),
        #[cfg(feature = "pkce-auth")]
        CredentialStore::Auto => Arc::new(AutoStorage::new()),
        #[cfg(not(feature = "pkce-auth"))]
        mode => {
            tracing::warn!(
                %mode,
                "keyring backends unavailable (pkce-auth feature disabled); using file storage"
            );
            Arc::new(FileStorage::new())
        }
    }
}

/// Resolves the configured [`CredentialStore`] for `app_id` and builds the
/// matching backend.
///
/// Resolution consults the CLI flag, the `${PREFIX}_CREDENTIAL_STORE` env var,
/// and the config file, defaulting to [`CredentialStore::Keyring`]; see
/// [`crate::config::resolve_credential_store`].
#[must_use]
pub fn default_storage(app_id: &str) -> Arc<dyn CredentialStorage> {
    let mode = crate::config::resolve_credential_store(app_id, |k| std::env::var(k).ok());
    storage_for(mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_env::{EnvVarGuard, lock, with_xdg_config_home};

    #[test]
    fn file_path_uses_app_id_and_provider() {
        let dir = std::env::temp_dir().join("cli-engine-storage-test-xdg");
        with_xdg_config_home(&dir, || {
            let key = CredentialKey::new("myapp", "prov", "prod");
            assert_eq!(
                FileStorage::path_for(&key),
                Some(dir.join("myapp").join("credentials").join("prov-prod.json"))
            );
            // empty app_id falls back to provider as the dir
            let key2 = CredentialKey::new("", "prov", "prod");
            assert_eq!(
                FileStorage::path_for(&key2),
                Some(dir.join("prov").join("credentials").join("prov-prod.json"))
            );
        });
    }

    #[test]
    fn file_path_rejects_unsafe_components() {
        for env in ["../../etc/passwd", "dev/subdir", "dev\\subdir", ".."] {
            let key = CredentialKey::new("app", "prov", env);
            assert_eq!(
                FileStorage::path_for(&key),
                None,
                "{env:?} should be rejected"
            );
        }
    }

    #[test]
    fn file_path_rejects_relative_base_dir() {
        with_xdg_config_home(std::path::Path::new("."), || {
            let key = CredentialKey::new("app", "prov", "dev");
            assert_eq!(FileStorage::path_for(&key), None);
        });
    }

    #[tokio::test]
    // The guard is intentionally held across awaits to serialize env mutation.
    #[allow(clippy::await_holding_lock)]
    async fn file_storage_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Hold the shared lock + env guard across the awaits (tokio::test uses a
        // current-thread runtime, so the non-Send guard is fine).
        let _lock = lock();
        let _env = EnvVarGuard::set("XDG_CONFIG_HOME", Some(dir.path()));

        let store = FileStorage::new();
        let key = CredentialKey::new("app", "prov", "dev");
        assert_eq!(store.load(&key).await, None);
        store.save(&key, "{\"token\":\"abc\"}").await.expect("save");
        assert_eq!(
            store.load(&key).await.as_deref(),
            Some("{\"token\":\"abc\"}")
        );
        store.delete(&key).await;
        assert_eq!(store.load(&key).await, None);
    }

    #[test]
    fn storage_for_file_is_always_available() {
        // Just assert it constructs without panicking; behavior covered above.
        let store = storage_for(CredentialStore::File);
        assert!(format!("{store:?}").contains("FileStorage"));
    }
}
