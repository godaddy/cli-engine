//! Where credentials are stored: the [`CredentialStore`] mode and how the
//! effective mode is resolved from flag, environment, and config file.

use std::cell::Cell;
use std::str::FromStr;

use serde::{Deserialize, Deserializer};

use super::{EngineConfig, load};

/// Where an auth provider stores credentials.
///
/// The variant selects a concrete storage backend
/// (see [`crate::auth::storage`]). `Auto` is the default and tries the system
/// keychain first, falling back to an unencrypted file when the keychain is
/// unavailable (headless Linux, WSL).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialStore {
    /// Try the system keychain; transparently fall back to an unencrypted file
    /// when the keychain backend is unavailable. This is the default.
    #[default]
    Auto,
    /// System keychain only. A keychain failure is a hard error and no file is
    /// ever written.
    Keyring,
    /// File only: never contact the system keychain. Credentials are written as
    /// unencrypted JSON under the config base directory.
    File,
}

impl CredentialStore {
    /// Returns the lowercase canonical name (`auto`, `keyring`, or `file`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialStore::Auto => "auto",
            CredentialStore::Keyring => "keyring",
            CredentialStore::File => "file",
        }
    }
}

impl std::fmt::Display for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string does not name a [`CredentialStore`] variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCredentialStoreError(String);

impl std::fmt::Display for ParseCredentialStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid credential store {:?} (expected one of: auto, keyring, file)",
            self.0
        )
    }
}

impl std::error::Error for ParseCredentialStoreError {}

impl FromStr for CredentialStore {
    type Err = ParseCredentialStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(CredentialStore::Auto),
            // `keychain` is accepted as an alias for the keychain-only mode.
            "keyring" | "keychain" => Ok(CredentialStore::Keyring),
            "file" => Ok(CredentialStore::File),
            _ => Err(ParseCredentialStoreError(s.to_owned())),
        }
    }
}

impl<'de> Deserialize<'de> for CredentialStore {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

// Per-thread override from the `--credential-store` global flag.
//
// Stored in a `thread_local!` `Cell` so concurrent `Cli::run` calls on
// different OS threads cannot interfere with each other. Each thread writes
// its own flag at the start of `Cli::run` (via `set_credential_store_flag`)
// and clears it at the end (via `clear_credential_store_flag`).
//
// Limitation: concurrent `Cli::run` calls sharing the same OS thread (e.g.
// concurrent tokio tasks on a single-threaded runtime) are not supported —
// the second call will observe the first run's flag. This scenario is atypical
// for a CLI library.
thread_local! {
    static CREDENTIAL_STORE_FLAG: Cell<u8> = const { Cell::new(0) };
}

fn encode_store(store: Option<CredentialStore>) -> u8 {
    match store {
        None => 0,
        Some(CredentialStore::Auto) => 1,
        Some(CredentialStore::Keyring) => 2,
        Some(CredentialStore::File) => 3,
    }
}

fn decode_store(byte: u8) -> Option<CredentialStore> {
    match byte {
        1 => Some(CredentialStore::Auto),
        2 => Some(CredentialStore::Keyring),
        3 => Some(CredentialStore::File),
        _ => None,
    }
}

/// Records the value of the `--credential-store` flag for the current thread.
///
/// Called at the start of each CLI run with the parsed flag value (`None` when
/// the flag was not supplied). Crate-internal: only the engine publishes
/// per-run flag state, so library consumers cannot mutate this latch.
pub(crate) fn set_credential_store_flag(store: Option<CredentialStore>) {
    CREDENTIAL_STORE_FLAG.with(|f| f.set(encode_store(store)));
}

/// Clears the thread-local flag set by [`set_credential_store_flag`].
///
/// Called at the end of each `Cli::run` so the flag does not leak into
/// subsequent sequential runs on the same thread.
pub(crate) fn clear_credential_store_flag() {
    CREDENTIAL_STORE_FLAG.with(|f| f.set(0));
}

/// Returns the flag override recorded by [`set_credential_store_flag`], if any.
/// Crate-internal accessor for the per-thread flag latch.
#[must_use]
pub(crate) fn credential_store_flag() -> Option<CredentialStore> {
    CREDENTIAL_STORE_FLAG.with(|f| decode_store(f.get()))
}

/// Derives the credential-store override env var from an app id, e.g.
/// `godaddy` -> `GODADDY_CREDENTIAL_STORE`.
#[must_use]
pub fn credential_store_env_var(app_id: &str) -> String {
    format!(
        "{}_CREDENTIAL_STORE",
        crate::flags::app_id_env_prefix(app_id)
    )
}

/// Resolves the effective [`CredentialStore`] from explicit inputs.
///
/// Pure and side-effect free so the precedence is unit-testable without touching
/// process state. Precedence (highest first): CLI `flag`, then `env` (an invalid
/// value is logged and ignored, falling through), then the config `file`, then
/// the default [`CredentialStore::Keyring`].
#[must_use]
pub fn resolve_credential_store_with(
    flag: Option<CredentialStore>,
    env: Option<&str>,
    file: &EngineConfig,
) -> CredentialStore {
    if let Some(store) = flag {
        return store;
    }
    if let Some(raw) = env {
        match raw.parse::<CredentialStore>() {
            Ok(store) => return store,
            Err(e) => tracing::warn!(error = %e, "ignoring invalid credential-store env var"),
        }
    }
    if let Some(store) = file.credentials.store {
        return store;
    }
    CredentialStore::default()
}

/// Resolves the effective [`CredentialStore`] for `app_id` against process state.
///
/// Reads the CLI-flag override (`credential_store_flag`), the
/// `${PREFIX}_CREDENTIAL_STORE` env var via the injected `var` getter, and the
/// config file ([`load`]), then applies [`resolve_credential_store_with`]. The
/// `var` getter is injected so callers/tests can supply environment lookups
/// without mutating the process environment.
pub fn resolve_credential_store(
    app_id: &str,
    var: impl Fn(&str) -> Option<String>,
) -> CredentialStore {
    let env = var(&credential_store_env_var(app_id));
    let file = load(app_id);
    resolve_credential_store_with(credential_store_flag(), env.as_deref(), &file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CredentialsConfig;

    #[test]
    fn parses_known_variants_case_insensitively() {
        assert_eq!("auto".parse(), Ok(CredentialStore::Auto));
        assert_eq!("Keyring".parse(), Ok(CredentialStore::Keyring));
        assert_eq!("KEYCHAIN".parse(), Ok(CredentialStore::Keyring));
        assert_eq!("  file  ".parse(), Ok(CredentialStore::File));
    }

    #[test]
    fn rejects_unknown_variant() {
        let err = "vault"
            .parse::<CredentialStore>()
            .expect_err("should reject");
        assert!(err.to_string().contains("vault"));
    }

    #[test]
    fn display_round_trips_through_from_str() {
        for store in [
            CredentialStore::Auto,
            CredentialStore::Keyring,
            CredentialStore::File,
        ] {
            assert_eq!(store.to_string().parse(), Ok(store));
        }
    }

    #[test]
    fn env_var_name_is_derived_from_app_id() {
        assert_eq!(
            credential_store_env_var("godaddy"),
            "GODADDY_CREDENTIAL_STORE"
        );
        assert_eq!(
            credential_store_env_var("my-cli"),
            "MY_CLI_CREDENTIAL_STORE"
        );
    }

    #[test]
    fn deserialize_rejects_bad_store_value() {
        let result = toml_edit::de::from_str::<EngineConfig>("[credentials]\nstore = \"nope\"\n");
        assert!(result.is_err(), "bad store value should fail to parse");
    }

    #[test]
    fn resolution_precedence_flag_beats_env_beats_file() {
        let file = EngineConfig {
            credentials: CredentialsConfig {
                store: Some(CredentialStore::Keyring),
            },
            ..Default::default()
        };
        // flag wins over everything
        assert_eq!(
            resolve_credential_store_with(Some(CredentialStore::Auto), Some("file"), &file),
            CredentialStore::Auto
        );
        // env wins over file
        assert_eq!(
            resolve_credential_store_with(None, Some("file"), &file),
            CredentialStore::File
        );
        // file wins over default
        assert_eq!(
            resolve_credential_store_with(None, None, &file),
            CredentialStore::Keyring
        );
    }

    #[test]
    fn resolution_defaults_to_auto() {
        assert_eq!(
            resolve_credential_store_with(None, None, &EngineConfig::default()),
            CredentialStore::Auto
        );
    }

    #[test]
    fn resolution_ignores_invalid_env_and_falls_through() {
        let file = EngineConfig {
            credentials: CredentialsConfig {
                store: Some(CredentialStore::File),
            },
            ..Default::default()
        };
        // invalid env is ignored, so the file value applies
        assert_eq!(
            resolve_credential_store_with(None, Some("garbage"), &file),
            CredentialStore::File
        );
        // invalid env with no file falls through to the default
        assert_eq!(
            resolve_credential_store_with(None, Some("garbage"), &EngineConfig::default()),
            CredentialStore::Auto
        );
    }

    #[test]
    fn credential_store_flag_encodes_round_trips() {
        for store in [
            None,
            Some(CredentialStore::Auto),
            Some(CredentialStore::Keyring),
            Some(CredentialStore::File),
        ] {
            assert_eq!(decode_store(encode_store(store)), store);
        }
    }
}
