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
//! config file, or the `Auto` default); see [`crate::config`].

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
// async_trait's expansion attaches a bare `#[must_use]` to each generated
// method, duplicating the one already implied by `Option`/`Result`.
#[allow(clippy::double_must_use)]
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

/// Reads the credential file at `path`, or `None` when it is absent or not safe to
/// trust.
///
/// On Unix the file is opened without following a symlink and must be a regular file
/// owned by the effective user with no group or other permission bits. A file that
/// fails that is not read: it may have been exposed to, or planted by, someone else,
/// so the caller gets no credential and the user has to log in again, which replaces
/// the file with a private one. `FileStorage` holds plaintext and is used where no
/// keychain is reachable, so nothing else protects the file, and it does not take the
/// file's word for it.
///
/// Elsewhere (Windows) the file is read without these checks: it lives under the
/// user's profile, whose access control already keeps other users out.
async fn read_credential_file(path: &std::path::Path) -> Option<String> {
    match read_private_file(path).await {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "credential file was not read; log in again to replace it"
            );
            None
        }
    }
}

#[cfg(unix)]
async fn read_private_file(path: &std::path::Path) -> std::io::Result<String> {
    read_private_file_as(path, rustix::process::geteuid().as_raw()).await
}

/// [`read_private_file`] for a given effective user, which a test sets to someone
/// other than the file's owner, since it cannot create a file owned by another user.
#[cfg(unix)]
async fn read_private_file_as(
    path: &std::path::Path,
    effective_user: u32,
) -> std::io::Result<String> {
    use rustix::fs::OFlags;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use tokio::io::AsyncReadExt as _;

    // `NOFOLLOW` refuses a symlink at the path; `NONBLOCK` keeps a FIFO planted there
    // from hanging the open (it is then refused as not a regular file).
    let flags = i32::try_from((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits())
        .map_err(std::io::Error::other)?;
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true).custom_flags(flags);
    let mut file = options.open(path).await?;
    // Checked on the open handle, not the path, so nothing can be swapped in between.
    let metadata = file.metadata().await?;
    if let Some(problem) = untrusted_file_problem(
        metadata.is_file(),
        metadata.uid(),
        effective_user,
        metadata.permissions().mode(),
    ) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            problem,
        ));
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    Ok(contents)
}

/// Why a credential file with these attributes must not be trusted, if it must not:
/// it is not a regular file, belongs to a user other than the one running, or can be
/// reached by group or others. Split out so each rule can be tested without needing
/// another user or root.
#[cfg(unix)]
fn untrusted_file_problem(
    is_file: bool,
    owner: u32,
    effective_user: u32,
    mode: u32,
) -> Option<&'static str> {
    if !is_file {
        Some("not a regular file")
    } else if owner != effective_user {
        Some("owned by another user")
    } else if mode & 0o077 != 0 {
        Some("readable or writable by group or others")
    } else {
        None
    }
}

#[cfg(not(unix))]
async fn read_private_file(path: &std::path::Path) -> std::io::Result<String> {
    tokio::fs::read_to_string(path).await
}

#[async_trait]
impl CredentialStorage for FileStorage {
    async fn load(&self, key: &CredentialKey<'_>) -> Option<String> {
        let path = Self::path_for(key)?;
        read_credential_file(&path).await
    }

    async fn save(&self, key: &CredentialKey<'_>, value: &str) -> Result<()> {
        let path = Self::path_for(key).ok_or_else(|| {
            crate::error::CliCoreError::message("could not determine credential file path")
        })?;
        let value = value.to_owned();
        tokio::task::spawn_blocking(move || crate::fs::write_private_file_atomic(&path, &value))
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
    /// Reads and writes the OS keychain only. `load` returns `None` for both
    /// "no entry" and "keychain backend unavailable" — callers cannot
    /// distinguish the two states. `save` is a hard error when the write
    /// fails. No file is ever written — use [`AutoStorage`] for a file
    /// fallback or [`super::FileStorage`] to skip the keychain entirely.
    ///
    /// If you need the three-state distinction (entry present / reachable but
    /// empty / backend unavailable), use the crate-internal `read_three_state`
    /// helper (as [`AutoStorage`] does internally).
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
/// Resolution consults (in priority order): the `--credential-store` CLI flag
/// stored in a per-thread latch by [`crate::cli::Cli::run`]; the
/// `${PREFIX}_CREDENTIAL_STORE` env var; the config file; and finally the
/// default [`CredentialStore::Keyring`].
///
/// **Note**: this function reads thread-local state set by `Cli::run`. Calling
/// it outside of a `Cli::run` execution (e.g. in a standalone binary without a
/// `Cli`) always sees `None` for the flag and relies solely on the env var and
/// config file. Prefer [`storage_for`] with an explicit [`CredentialStore`]
/// when you don't need the full resolution chain.
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

    #[cfg(unix)]
    mod hardening {
        use super::*;
        use std::os::unix::fs::PermissionsExt as _;

        const SECRET: &str = "{\"token\":\"abc\"}";

        fn mode_of(path: &std::path::Path) -> u32 {
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        }

        fn chmod(path: &std::path::Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        }

        /// A `FileStorage` rooted in a fresh config directory, the credential key,
        /// and the path its file lives at. The guards hold the process-wide lock and
        /// environment for the test.
        struct Fixture {
            // Fields drop in declaration order: the environment must be restored while
            // the lock is still held, so `_env` comes first.
            _env: EnvVarGuard,
            _lock: std::sync::MutexGuard<'static, ()>,
            dir: tempfile::TempDir,
            store: FileStorage,
            path: std::path::PathBuf,
        }

        fn fixture() -> Fixture {
            let dir = tempfile::tempdir().expect("tempdir");
            let lock = lock();
            let env = EnvVarGuard::set("XDG_CONFIG_HOME", Some(dir.path()));
            let path = dir
                .path()
                .join("app")
                .join("credentials")
                .join("prov-dev.json");
            Fixture {
                _env: env,
                _lock: lock,
                dir,
                store: FileStorage::new(),
                path,
            }
        }

        fn key() -> CredentialKey<'static> {
            CredentialKey::new("app", "prov", "dev")
        }

        #[tokio::test]
        // The guards serialize the process-wide environment across the awaits.
        #[allow(clippy::await_holding_lock)]
        async fn a_private_file_is_read() {
            let f = fixture();
            f.store.save(&key(), SECRET).await.expect("save");
            assert_eq!(f.store.load(&key()).await.as_deref(), Some(SECRET));
            // Owner-only read access is still private.
            chmod(&f.path, 0o400);
            assert_eq!(f.store.load(&key()).await.as_deref(), Some(SECRET));
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn a_file_that_group_or_others_can_reach_is_not_read() {
            let f = fixture();
            f.store.save(&key(), SECRET).await.expect("save");
            for mode in [0o644, 0o640, 0o604, 0o660, 0o666, 0o610, 0o601] {
                chmod(&f.path, mode);
                assert_eq!(f.store.load(&key()).await, None, "{mode:o}");
            }
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn a_symlink_at_the_credential_path_is_not_followed() {
            let f = fixture();
            let elsewhere = f.dir.path().join("elsewhere.json");
            std::fs::write(&elsewhere, SECRET).expect("write target");
            chmod(&elsewhere, 0o600);
            std::fs::create_dir_all(f.path.parent().expect("parent")).expect("mkdir");
            std::os::unix::fs::symlink(&elsewhere, &f.path).expect("symlink");
            assert_eq!(
                f.store.load(&key()).await,
                None,
                "a private target does not make the link trustworthy"
            );
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn something_that_is_not_a_regular_file_is_refused_without_blocking() {
            let f = fixture();
            std::fs::create_dir_all(f.path.parent().expect("parent")).expect("mkdir");
            rustix::fs::mknodat(
                rustix::fs::CWD,
                &f.path,
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::from_raw_mode(0o600),
                0,
            )
            .expect("mkfifo");
            // A blocking open of a FIFO with no writer never returns. If it would, give
            // it a writer after the deadline so the blocked thread is released and the
            // test fails instead of hanging the whole run.
            let path = f.path.clone();
            let credential_key = key();
            let load = f.store.load(&credential_key);
            tokio::pin!(load);
            let loaded = tokio::select! {
                loaded = &mut load => Some(loaded),
                () = tokio::time::sleep(std::time::Duration::from_secs(3)) => {
                    drop(std::fs::OpenOptions::new().write(true).open(&path));
                    None
                }
            };
            assert!(loaded.is_some(), "load blocked on a FIFO");
            let loaded = loaded.flatten();
            assert_eq!(loaded, None);
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn a_file_owned_by_another_user_is_not_read() {
            let f = fixture();
            f.store.save(&key(), SECRET).await.expect("save");
            let me = rustix::process::geteuid().as_raw();
            assert_eq!(
                read_private_file_as(&f.path, me).await.expect("own file"),
                SECRET
            );
            // The same private file, as seen by a different user running the CLI.
            let refused = read_private_file_as(&f.path, me.wrapping_add(1))
                .await
                .expect_err("another user's file");
            assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData);
        }

        #[test]
        fn the_ownership_and_mode_rules() {
            // (is_file, owner, effective user, mode) -> trusted?
            let mine = 1000;
            assert_eq!(untrusted_file_problem(true, mine, mine, 0o100_600), None);
            assert_eq!(untrusted_file_problem(true, mine, mine, 0o100_400), None);
            assert!(
                untrusted_file_problem(true, 0, mine, 0o100_600).is_some(),
                "root's"
            );
            assert!(
                untrusted_file_problem(true, 2000, mine, 0o100_600).is_some(),
                "another user's"
            );
            assert!(untrusted_file_problem(true, mine, mine, 0o100_640).is_some());
            assert!(untrusted_file_problem(true, mine, mine, 0o100_604).is_some());
            assert!(
                untrusted_file_problem(false, mine, mine, 0o600).is_some(),
                "not a file"
            );
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn saving_repairs_a_loose_file_and_directory() {
            let f = fixture();
            f.store.save(&key(), SECRET).await.expect("save");
            let dir = f.path.parent().expect("parent").to_owned();
            // As a careless `chmod`, or a file from before these checks, may leave them.
            chmod(&dir, 0o755);
            chmod(&f.path, 0o644);
            assert_eq!(f.store.load(&key()).await, None, "loose file refused");

            f.store.save(&key(), SECRET).await.expect("save again");
            assert_eq!(mode_of(&f.path), 0o600, "the file comes back private");
            assert_eq!(mode_of(&dir), 0o700, "the existing directory is tightened");
            assert_eq!(f.store.load(&key()).await.as_deref(), Some(SECRET));
        }

        /// A link planted at the credentials directory must not turn a save into a
        /// permission change on, or a write into, whatever it points at.
        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn a_symlinked_credentials_directory_is_refused_and_its_target_untouched() {
            let f = fixture();
            let target = f.dir.path().join("somewhere-else");
            std::fs::create_dir(&target).expect("mkdir");
            chmod(&target, 0o755);
            let credentials = f.path.parent().expect("parent").to_owned();
            std::fs::create_dir_all(credentials.parent().expect("app dir")).expect("mkdir");
            std::os::unix::fs::symlink(&target, &credentials).expect("symlink");

            assert!(
                f.store.save(&key(), SECRET).await.is_err(),
                "saving through a symlinked directory is refused"
            );
            assert_eq!(mode_of(&target), 0o755, "the target's mode is not changed");
            assert_eq!(
                std::fs::read_dir(&target).expect("read_dir").count(),
                0,
                "nothing is written into the target"
            );
        }

        /// A credentials directory the owner can write and search but not read (`0300`)
        /// cannot be opened for reading, yet a save into it works and repairs it.
        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn a_write_only_credentials_directory_is_still_saved_into_and_repaired() {
            let f = fixture();
            f.store.save(&key(), SECRET).await.expect("save");
            let dir = f.path.parent().expect("parent").to_owned();
            chmod(&dir, 0o300);

            f.store
                .save(&key(), SECRET)
                .await
                .expect("a 0300 directory is writable");
            assert_eq!(mode_of(&dir), 0o700, "and is repaired");
            assert_eq!(f.store.load(&key()).await.as_deref(), Some(SECRET));
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn saving_leaves_no_temp_file_beside_the_credential() {
            let f = fixture();
            f.store.save(&key(), SECRET).await.expect("save");
            f.store.save(&key(), SECRET).await.expect("save again");
            let names: Vec<_> = std::fs::read_dir(f.path.parent().expect("parent"))
                .expect("read_dir")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["prov-dev.json"]);
        }
    }
}
