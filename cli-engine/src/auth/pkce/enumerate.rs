//! Which environments have a stored credential, for `auth status`.
//!
//! The storage backends cannot enumerate (see [`EnvironmentIndex`](crate::auth::EnvironmentIndex)), so the provider
//! keeps an index of what it has saved and this module reads it back. The index is a
//! hint, so every name is checked against the credential itself before it is listed.

use std::collections::BTreeSet;

use super::PkceAuthProvider;

/// Records that `env` has a stored credential. A failure to write the index is
/// logged and otherwise ignored: the credential itself is saved, the login is
/// complete, and the worst outcome is that `auth status` does not list it until the
/// next save.
pub(super) async fn record(provider: &PkceAuthProvider, env: &str) {
    if let Err(err) = provider.env_index().await.record(env).await {
        tracing::warn!(env, error = %err, "could not record the environment in the credential index");
    }
}

/// Records that `env` no longer has a stored credential, unless one is stored after
/// all. The check is made under the index's lock, so a login that saved the credential
/// after the caller deleted it, or last probed it, is not undone. A write failure is
/// logged and ignored for the same reason as [`record`]; a stale name is harmless,
/// since listing checks it.
pub(super) async fn forget(provider: &PkceAuthProvider, env: &str) {
    let result = provider
        .env_index()
        .await
        .forget_unless_present(env, async || provider.has_stored(env).await)
        .await;
    if let Err(err) = result {
        tracing::warn!(env, error = %err, "could not remove the environment from the credential index");
    }
}

/// Every environment with a stored credential, sorted.
///
/// Candidates are the in-process cache, the index, and the environments the provider
/// was told about (`with_environments`). The last is a backfill: a credential saved
/// by a version that kept no index is still found, and recorded, the first time it is
/// looked for. Each candidate is kept only if the backend really holds a credential
/// for it, so a name left behind by a crash or an outside deletion is dropped.
///
/// Probing declared environments reads `environments.toml`, which is blocking file
/// I/O, and loads from the backend once per declared environment that the index does
/// not already name.
pub(super) async fn environments_with_credentials(provider: &PkceAuthProvider) -> Vec<String> {
    let index = provider.env_index().await;
    let indexed: BTreeSet<String> = index.list().await.into_iter().collect();

    let mut candidates: BTreeSet<String> = indexed.clone();
    candidates.extend(provider.cache.read().await.keys().cloned());
    let declared: BTreeSet<String> = provider
        .environments
        .as_ref()
        .map(|environments| environments.list().into_iter().collect())
        .unwrap_or_default();
    candidates.extend(declared.iter().cloned());

    let mut present = Vec::new();
    for env in candidates {
        if provider.has_stored(&env).await {
            if !indexed.contains(&env) {
                // Found without an index entry (an older save, or a lost update):
                // record it so the next listing needs no probe.
                record(provider, &env).await;
            }
            present.push(env);
        } else if indexed.contains(&env) {
            // Named by the index but gone: drop the stale name, unless a login
            // has saved it again since the probe above.
            forget(provider, &env).await;
        }
    }
    present
}
