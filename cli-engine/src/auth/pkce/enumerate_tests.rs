//! Listing the environments that hold a credential, across a simulated restart, for
//! each storage backend `auth status` can end up on.

use std::sync::Arc;

use super::{
    PkceAuthProvider, test_provider,
    tests::{MemoryStorage, valid_token},
};
use crate::auth::{
    AuthProvider, Dispatcher, EnvironmentIndex,
    storage::{CredentialKey, CredentialStorage},
};
use crate::config::CredentialStore;
use crate::environments::{EnvTable, Environments};

/// A provider over `storage`. Each call builds a brand-new provider, so what carries
/// from one to the next is exactly what a restart would keep: the backend.
fn provider_on(storage: &Arc<dyn CredentialStorage>) -> PkceAuthProvider {
    test_provider()
        .with_app_id("app")
        .with_storage(storage.clone())
}

fn memory() -> Arc<dyn CredentialStorage> {
    Arc::new(MemoryStorage::default())
}

async fn listed(provider: &PkceAuthProvider) -> Vec<String> {
    let mut envs = provider.list_environments().await.expect("list");
    envs.sort();
    envs
}

#[tokio::test]
async fn environments_are_listed_after_a_restart() {
    let storage = memory();
    let first = provider_on(&storage);
    first
        .save_stored("dev", &valid_token("a"))
        .await
        .expect("save");
    first
        .save_stored("prod", &valid_token("b"))
        .await
        .expect("save");
    assert_eq!(listed(&first).await, ["dev", "prod"]);
    drop(first);

    // A new process: empty cache, same backend.
    let restarted = provider_on(&storage);
    assert!(
        restarted.cache.read().await.is_empty(),
        "no in-process state"
    );
    assert_eq!(listed(&restarted).await, ["dev", "prod"]);
}

#[tokio::test]
async fn a_deleted_credential_leaves_the_listing_after_a_restart() {
    let storage = memory();
    let first = provider_on(&storage);
    first
        .save_stored("dev", &valid_token("a"))
        .await
        .expect("save");
    first
        .save_stored("prod", &valid_token("b"))
        .await
        .expect("save");
    first.logout("dev").await.expect("logout");
    assert_eq!(listed(&first).await, ["prod"]);
    drop(first);

    assert_eq!(listed(&provider_on(&storage)).await, ["prod"]);
}

/// Looked at directly, before any listing: a listing drops names whose credential is
/// gone, so it would hide a logout that failed to update the index.
#[tokio::test]
async fn logging_out_updates_the_index_itself() {
    let storage = memory();
    let index = EnvironmentIndex::new(storage.clone(), "app", "test");
    let provider = provider_on(&storage);
    provider
        .save_stored("dev", &valid_token("a"))
        .await
        .expect("save");
    provider
        .save_stored("prod", &valid_token("b"))
        .await
        .expect("save");
    assert_eq!(index.list().await, ["dev", "prod"]);

    provider.logout("dev").await.expect("logout");
    assert_eq!(
        index.list().await,
        ["prod"],
        "the index no longer names dev"
    );

    provider.logout("prod").await.expect("logout");
    assert!(index.list().await.is_empty());
    assert!(
        storage
            .load(&CredentialKey::new("app", "test.index", "index"))
            .await
            .is_none(),
        "an empty index leaves no entry behind"
    );
}

#[tokio::test]
async fn saving_the_same_environment_again_lists_it_once() {
    let storage = memory();
    let provider = provider_on(&storage);
    for _ in 0..3 {
        provider
            .save_stored("dev", &valid_token("a"))
            .await
            .expect("save");
    }
    assert_eq!(listed(&provider).await, ["dev"]);
}

#[tokio::test]
async fn a_name_whose_credential_was_deleted_outside_is_dropped_and_forgotten() {
    let storage = memory();
    let provider = provider_on(&storage);
    provider
        .save_stored("dev", &valid_token("a"))
        .await
        .expect("save");
    provider
        .save_stored("prod", &valid_token("b"))
        .await
        .expect("save");

    // Removed behind the provider's back, as another tool or a manual delete might.
    storage
        .delete(&CredentialKey::new("app", "test", "dev"))
        .await;

    let restarted = provider_on(&storage);
    assert_eq!(listed(&restarted).await, ["prod"]);
    let index = EnvironmentIndex::new(storage.clone(), "app", "test");
    assert_eq!(
        index.list().await,
        ["prod"],
        "the stale name is removed too"
    );
}

#[tokio::test]
async fn a_credential_with_no_index_entry_is_found_through_declared_environments() {
    // Saved by a version that kept no index: write the credential directly.
    let storage = memory();
    let token = serde_json::to_string(&valid_token("old")).expect("serialize");
    storage
        .save(&CredentialKey::new("app", "test", "prod"), &token)
        .await
        .expect("seed");

    let environments = Arc::new(
        Environments::new("prod")
            .with_environment("prod", EnvTable::new())
            .with_environment("dev", EnvTable::new()),
    );
    let provider = provider_on(&storage).with_environments(environments);
    assert_eq!(listed(&provider).await, ["prod"], "dev has nothing stored");

    // Found once, so it is indexed and needs no probe from then on.
    let index = EnvironmentIndex::new(storage.clone(), "app", "test");
    assert_eq!(index.list().await, ["prod"]);
}

#[tokio::test]
async fn a_credential_with_no_index_and_no_declared_environment_is_not_found() {
    // The honest limit of backfilling: with nothing naming the environment there is
    // nothing to probe. A fresh login or any save indexes it.
    let storage = memory();
    let token = serde_json::to_string(&valid_token("old")).expect("serialize");
    storage
        .save(&CredentialKey::new("app", "test", "prod"), &token)
        .await
        .expect("seed");
    assert!(listed(&provider_on(&storage)).await.is_empty());
}

#[tokio::test]
async fn providers_sharing_a_backend_do_not_see_each_others_environments() {
    let storage = memory();
    let a = PkceAuthProvider::new("a", "https://x/auth", "https://x/token", "c", &["openid"])
        .with_app_id("app")
        .with_storage(storage.clone());
    let a_b = PkceAuthProvider::new("a-b", "https://x/auth", "https://x/token", "c", &["openid"])
        .with_app_id("app")
        .with_storage(storage.clone());
    a.save_stored("b-c", &valid_token("1")).await.expect("save");
    a_b.save_stored("c", &valid_token("2")).await.expect("save");

    // Both land on the same file name under a directory scan; the index does not
    // confuse them.
    assert_eq!(listed(&a).await, ["b-c"]);
    assert_eq!(listed(&a_b).await, ["c"]);
}

#[tokio::test]
async fn apps_sharing_a_backend_do_not_see_each_others_environments() {
    let storage = memory();
    let one = test_provider()
        .with_app_id("one")
        .with_storage(storage.clone());
    let two = test_provider()
        .with_app_id("two")
        .with_storage(storage.clone());
    one.save_stored("dev", &valid_token("1"))
        .await
        .expect("save");

    assert_eq!(listed(&one).await, ["dev"]);
    assert!(listed(&two).await.is_empty());
}

#[tokio::test]
async fn a_corrupt_or_foreign_index_is_treated_as_empty_and_repaired_by_the_next_save() {
    let storage = memory();
    let index_key = CredentialKey::new("app", "test.index", "index");
    for garbage in [
        "not json",
        "[]",
        "{\"environments\": 7}",
        // A credential blob stored where the index lives is not an index.
        &serde_json::to_string(&valid_token("secret")).expect("serialize"),
    ] {
        storage.save(&index_key, garbage).await.expect("seed");
        let provider = provider_on(&storage);
        assert!(listed(&provider).await.is_empty(), "{garbage}");
        provider
            .save_stored("dev", &valid_token("a"))
            .await
            .expect("save");
        assert_eq!(listed(&provider_on(&storage)).await, ["dev"], "{garbage}");
    }
}

#[tokio::test]
async fn an_oversized_index_is_bounded() {
    let storage = memory();
    let many: Vec<String> = (0..10_000).map(|n| format!("env-{n}")).collect();
    let document = serde_json::json!({ "environments": many }).to_string();
    storage
        .save(&CredentialKey::new("app", "test.index", "index"), &document)
        .await
        .expect("seed");

    let index = EnvironmentIndex::new(storage.clone(), "app", "test");
    assert!(
        index.list().await.len() <= 256,
        "a damaged index cannot make a caller probe thousands"
    );
}

#[tokio::test]
async fn an_environment_that_cannot_be_indexed_does_not_fail_the_save() {
    let storage = memory();
    let provider = provider_on(&storage);
    let long = "x".repeat(300);
    provider
        .save_stored(&long, &valid_token("a"))
        .await
        .expect("the save itself succeeds");
    assert!(
        storage
            .load(&CredentialKey::new("app", "test", &long))
            .await
            .is_some()
    );
    assert!(
        listed(&provider).await.is_empty(),
        "too long to index, so not listed"
    );
}

#[tokio::test]
async fn an_index_that_cannot_be_written_does_not_fail_the_save() {
    #[derive(Debug)]
    struct RefusesIndexWrites(MemoryStorage);
    #[async_trait::async_trait]
    impl CredentialStorage for RefusesIndexWrites {
        async fn load(&self, key: &CredentialKey<'_>) -> Option<String> {
            self.0.load(key).await
        }
        async fn save(&self, key: &CredentialKey<'_>, value: &str) -> crate::Result<()> {
            if key.provider.ends_with(".index") {
                return Err(crate::error::CliCoreError::message("read-only index"));
            }
            self.0.save(key, value).await
        }
        async fn delete(&self, key: &CredentialKey<'_>) {
            self.0.delete(key).await;
        }
    }
    let storage: Arc<dyn CredentialStorage> =
        Arc::new(RefusesIndexWrites(MemoryStorage::default()));
    let provider = provider_on(&storage);
    provider
        .save_stored("dev", &valid_token("a"))
        .await
        .expect("the credential is saved");
    assert!(provider.has_stored("dev").await);
}

#[tokio::test]
async fn auth_status_across_all_providers_lists_stored_environments_after_a_restart() {
    let storage = memory();
    provider_on(&storage)
        .save_stored("prod", &valid_token("tok"))
        .await
        .expect("save");

    let mut dispatcher = Dispatcher::new();
    dispatcher.register(Arc::new(provider_on(&storage)));
    let statuses = dispatcher.all_statuses().await;
    assert_eq!(statuses.len(), 1, "{statuses:?}");
    assert_eq!(statuses[0].env, "prod");
    assert_eq!(
        statuses[0].credential.as_ref().expect("credential").token,
        "tok"
    );
}

/// The real file backend, which is what `Auto` falls back to without a keychain.
#[tokio::test]
// The guard serializes the process-wide config directory across the awaits below.
#[allow(clippy::await_holding_lock)]
async fn the_file_backend_lists_environments_after_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _lock = crate::config::test_env::lock();
    let _env = crate::config::test_env::EnvVarGuard::set("XDG_CONFIG_HOME", Some(dir.path()));
    let file_provider = || {
        test_provider()
            .with_app_id("app")
            .with_credential_store(CredentialStore::File)
    };

    let first = file_provider();
    first
        .save_stored("dev", &valid_token("a"))
        .await
        .expect("save");
    first
        .save_stored("prod", &valid_token("b"))
        .await
        .expect("save");
    drop(first);
    assert_eq!(listed(&file_provider()).await, ["dev", "prod"]);

    let restarted = file_provider();
    restarted.logout("dev").await.expect("logout");
    assert_eq!(listed(&file_provider()).await, ["prod"]);
}

/// What `AutoStorage` does, with the keychain replaced by a switchable fake: while
/// the keychain is up, reads and writes go to it and the file copy is dropped; while
/// it is down, they fall back to the file. Modelled here because the real keychain is
/// not available to tests.
#[derive(Debug)]
struct SwitchableStorage {
    keychain: MemoryStorage,
    file: MemoryStorage,
    keychain_up: std::sync::atomic::AtomicBool,
}

impl SwitchableStorage {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            keychain: MemoryStorage::default(),
            file: MemoryStorage::default(),
            keychain_up: std::sync::atomic::AtomicBool::new(true),
        })
    }

    fn set_keychain(&self, up: bool) {
        self.keychain_up
            .store(up, std::sync::atomic::Ordering::SeqCst);
    }

    fn up(&self) -> bool {
        self.keychain_up.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl CredentialStorage for SwitchableStorage {
    async fn load(&self, key: &CredentialKey<'_>) -> Option<String> {
        if self.up() {
            self.keychain.load(key).await
        } else {
            self.file.load(key).await
        }
    }

    async fn save(&self, key: &CredentialKey<'_>, value: &str) -> crate::Result<()> {
        if self.up() {
            self.keychain.save(key, value).await?;
            self.file.delete(key).await;
            Ok(())
        } else {
            self.file.save(key, value).await
        }
    }

    async fn delete(&self, key: &CredentialKey<'_>) {
        self.keychain.delete(key).await;
        self.file.delete(key).await;
    }
}

#[tokio::test]
async fn the_index_and_the_credentials_move_together_when_the_keychain_goes_away() {
    let backend = SwitchableStorage::new();
    let storage: Arc<dyn CredentialStorage> = backend.clone();

    // Saved while the keychain is available.
    let provider = provider_on(&storage);
    provider
        .save_stored("prod", &valid_token("a"))
        .await
        .expect("save");
    assert_eq!(listed(&provider_on(&storage)).await, ["prod"]);

    // The keychain becomes unavailable (a headless session, a locked keyring): the
    // credential and its index entry are both unreadable, so nothing is listed that
    // cannot be used, rather than a name that leads nowhere.
    backend.set_keychain(false);
    assert!(listed(&provider_on(&storage)).await.is_empty());

    // A login in that state lands both in the file, and is listed.
    let fallback = provider_on(&storage);
    fallback
        .save_stored("dev", &valid_token("b"))
        .await
        .expect("save");
    assert_eq!(listed(&provider_on(&storage)).await, ["dev"]);

    // The keychain returns: its own entries are listed again, and the file's are not,
    // matching what `load` returns for them.
    backend.set_keychain(true);
    assert_eq!(listed(&provider_on(&storage)).await, ["prod"]);
}

#[tokio::test]
async fn a_reachable_empty_keychain_lists_nothing_and_is_not_an_error() {
    let backend = SwitchableStorage::new();
    let storage: Arc<dyn CredentialStorage> = backend;
    let provider = provider_on(&storage);
    assert!(listed(&provider).await.is_empty());
    // Listing wrote nothing: there was nothing to record.
    assert!(
        storage
            .load(&CredentialKey::new("app", "test.index", "index"))
            .await
            .is_none()
    );
}
