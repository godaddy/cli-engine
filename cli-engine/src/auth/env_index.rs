//! Which environments hold a stored credential, for one app and provider.
//!
//! A [`CredentialStorage`] can store, load and delete a credential given its
//! `(app, provider, env)` key, but it cannot answer "which environments have one?":
//! the system keychain has no way to list items by prefix, and a directory scan of
//! [`FileStorage`](super::FileStorage) is ambiguous, since `<provider>-<env>.json`
//! reads the same for provider `a` with env `b-c` as for provider `a-b` with env
//! `c`. So `auth status` after a restart had nothing to enumerate.
//!
//! [`EnvironmentIndex`] answers it by keeping the list itself, as one small entry in
//! the same backend, so it behaves identically for the keychain, the file store and
//! the automatic fallback between them.
//!
//! The index is a hint, not a source of truth. It can lag the credentials it names
//! (a crash between two writes, a credential deleted outside the CLI, or two
//! processes updating it at once), so a caller should check each name it returns
//! with [`CredentialStorage::load`] before relying on it.

use std::{collections::BTreeSet, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::storage::{CredentialKey, CredentialStorage};
use crate::{Result, error::CliCoreError};

/// Most environments one index will hold, so a damaged or tampered entry cannot make
/// a caller probe an unbounded list.
const MAX_ENVIRONMENTS: usize = 256;

/// Longest environment name the index accepts, in bytes.
const MAX_NAME_BYTES: usize = 128;

/// The environment slot of the index's own key. With the two suffixes below it puts
/// the index at `("<app>.index", "<provider>.index", "index")`.
const INDEX_ENV: &str = "index";

/// Suffix that turns a provider name into the one the index is stored under.
const INDEX_PROVIDER_SUFFIX: &str = ".index";

/// Suffix that turns an app id into the one the index is stored under. The file
/// backend keeps each app's entries in a directory named after it, so this puts the
/// index in a directory no credential of `<app>` is written to.
const INDEX_APP_SUFFIX: &str = ".index";

/// The on-storage shape of the index. Unknown fields are ignored, so a later version
/// can add some without an older one reading the entry as damaged and overwriting it.
/// A blob that is not an index at all (a credential, say) reads as an empty one.
#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexDocument {
    #[serde(default)]
    environments: Vec<String>,
}

/// Remembers which environments have a stored credential for one
/// `(app_id, provider)`, in the same [`CredentialStorage`] the credentials use.
///
/// Call [`record`](Self::record) when a credential is saved and
/// [`forget`](Self::forget) when it is deleted, and [`list`](Self::list) to
/// enumerate. See the module documentation for why the list is a hint.
///
/// The index is stored under a key derived from the app and the provider, never from
/// an environment, so no environment name, which can come from a user, can address
/// it or overwrite it. For the file backend that means its own directory,
/// `<app>.index`, rather than a file beside the credentials, where a name like
/// `b.index-index` would reach it. The one way to collide with it is another app, or
/// provider, whose own name is this one plus `.index`; do not name them that way.
/// Updates within one process are serialized; two processes updating at the same
/// moment can lose one of the two additions, which a later [`record`](Self::record)
/// of that environment repairs.
///
/// # Examples
///
/// ```
/// # async fn demo() -> cli_engine::Result<()> {
/// use std::sync::Arc;
/// use cli_engine::{EnvironmentIndex, FileStorage};
///
/// let index = EnvironmentIndex::new(Arc::new(FileStorage::new()), "my-cli", "primary");
/// // After saving a credential for `prod`:
/// index.record("prod").await?;
/// assert_eq!(index.list().await, ["prod"]);
/// // After deleting it:
/// index.forget("prod").await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct EnvironmentIndex {
    storage: Arc<dyn CredentialStorage>,
    index_app: String,
    index_provider: String,
    /// Serializes read-modify-write cycles within this process.
    write_lock: Mutex<()>,
}

impl EnvironmentIndex {
    /// Creates the index for `provider` under `app_id`, stored in `storage`.
    #[must_use]
    pub fn new(
        storage: Arc<dyn CredentialStorage>,
        app_id: impl AsRef<str>,
        provider: &str,
    ) -> Self {
        // An empty app id is meaningful to the file backend (it names the directory
        // after the provider), so it stays empty rather than becoming `.index`.
        let app_id = app_id.as_ref();
        let index_app = if app_id.is_empty() {
            String::new()
        } else {
            format!("{app_id}{INDEX_APP_SUFFIX}")
        };
        Self {
            storage,
            index_app,
            index_provider: format!("{provider}{INDEX_PROVIDER_SUFFIX}"),
            write_lock: Mutex::new(()),
        }
    }

    fn key(&self) -> CredentialKey<'_> {
        CredentialKey::new(&self.index_app, &self.index_provider, INDEX_ENV)
    }

    /// The environments recorded, sorted. Empty when nothing is recorded or the entry
    /// cannot be read. A name here has no stored credential only if it was deleted
    /// outside this index, so check it with [`CredentialStorage::load`].
    pub async fn list(&self) -> Vec<String> {
        self.read().await.into_iter().collect()
    }

    /// Records that `env` has a stored credential. Does nothing, and writes nothing,
    /// when it is already recorded, so calling it on every save is cheap.
    ///
    /// # Errors
    ///
    /// Returns an error when `env` is empty or longer than 128 bytes, when 256
    /// environments are already recorded, or when the index cannot be written.
    pub async fn record(&self, env: &str) -> Result<()> {
        if !is_indexable(env) {
            return Err(CliCoreError::message(
                "environment name is empty or too long to be indexed",
            ));
        }
        let _writing = self.write_lock.lock().await;
        let mut environments = self.read().await;
        if environments.contains(env) {
            return Ok(());
        }
        if environments.len() >= MAX_ENVIRONMENTS {
            return Err(CliCoreError::message(format!(
                "cannot index more than {MAX_ENVIRONMENTS} environments"
            )));
        }
        environments.insert(env.to_owned());
        self.write(&environments).await
    }

    /// Records that `env` no longer has a stored credential. Does nothing when it was
    /// not recorded. Removing the last one deletes the index entry altogether.
    ///
    /// # Errors
    ///
    /// Returns an error when the index cannot be written.
    pub async fn forget(&self, env: &str) -> Result<()> {
        let _writing = self.write_lock.lock().await;
        self.forget_locked(env).await
    }

    /// [`forget`](Self::forget), unless `present` reports the credential back. The
    /// check runs while the index is locked, so a save that lands after a caller last
    /// looked, and records itself, is waited for and then seen instead of undone.
    #[cfg(feature = "pkce-auth")]
    pub(crate) async fn forget_unless_present(
        &self,
        env: &str,
        present: impl AsyncFnOnce() -> bool,
    ) -> Result<()> {
        let _writing = self.write_lock.lock().await;
        if present().await {
            return Ok(());
        }
        self.forget_locked(env).await
    }

    /// The body of [`forget`](Self::forget); the caller holds `write_lock`.
    async fn forget_locked(&self, env: &str) -> Result<()> {
        let mut environments = self.read().await;
        if !environments.remove(env) {
            return Ok(());
        }
        if environments.is_empty() {
            self.storage.delete(&self.key()).await;
            return Ok(());
        }
        self.write(&environments).await
    }

    async fn read(&self) -> BTreeSet<String> {
        let Some(raw) = self.storage.load(&self.key()).await else {
            return BTreeSet::new();
        };
        match serde_json::from_str::<IndexDocument>(&raw) {
            Ok(document) => document
                .environments
                .into_iter()
                .filter(|name| is_indexable(name))
                .take(MAX_ENVIRONMENTS)
                .collect(),
            Err(err) => {
                tracing::warn!(error = %err, "environment index is unreadable; treating it as empty");
                BTreeSet::new()
            }
        }
    }

    async fn write(&self, environments: &BTreeSet<String>) -> Result<()> {
        let document = IndexDocument {
            environments: environments.iter().cloned().collect(),
        };
        let json = serde_json::to_string(&document).map_err(CliCoreError::from)?;
        self.storage.save(&self.key(), &json).await
    }
}

fn is_indexable(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES
}
