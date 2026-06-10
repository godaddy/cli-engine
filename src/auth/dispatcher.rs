use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use super::{AuthProvider, Credential, CredentialRequest};
use crate::middleware::CommandMeta;
use crate::{CliCoreError, Result};

/// Routes auth operations to registered providers by name.
///
/// Clones share the same provider registry, so provider facades and transport
/// injectors see later registration or replacement.
#[derive(Clone, Debug, Default)]
pub struct Dispatcher {
    inner: Arc<RwLock<DispatcherInner>>,
}

#[derive(Clone, Debug, Default)]
struct DispatcherInner {
    providers: Vec<(String, Arc<dyn AuthProvider>)>,
}

/// Status row produced while querying all providers.
#[derive(Clone, Debug)]
pub struct StatusEntry {
    /// Provider name.
    pub provider: String,
    /// Environment name.
    pub env: String,
    /// Cached credential when status succeeded.
    pub credential: Option<Credential>,
    /// Status error text when status failed.
    pub error: Option<String>,
}

impl Dispatcher {
    /// Creates an empty dispatcher.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(DispatcherInner::default())),
        }
    }

    /// Registers or replaces a provider under its [`AuthProvider::name`].
    pub fn register(&mut self, provider: Arc<dyn AuthProvider>) {
        let name = provider.name().to_owned();
        let mut inner = self.write_inner();
        if let Some((_, existing)) = inner
            .providers
            .iter_mut()
            .find(|(existing_name, _)| existing_name == &name)
        {
            *existing = provider;
            return;
        }
        inner.providers.push((name, provider));
    }

    /// Returns provider names in registration order.
    #[must_use]
    pub fn registered_names(&self) -> Vec<String> {
        self.read_inner()
            .providers
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Gets a credential from a named provider.
    pub async fn get_credential(
        &self,
        name: &str,
        env: &str,
        command: &str,
        tier: &str,
    ) -> Result<Credential> {
        self.get(name)?.get_credential(env, command, tier).await
    }

    /// Gets a credential from a named provider, passing the command's full
    /// [`CredentialRequest`] so metadata-aware providers (e.g. OAuth scope
    /// step-up) can act on it.
    pub async fn get_credential_for(
        &self,
        name: &str,
        req: &CredentialRequest<'_>,
    ) -> Result<Credential> {
        self.get(name)?.get_credential_for(req).await
    }

    /// Clears any cached credential, ignoring logout failures, then authenticates.
    pub async fn login(&self, name: &str, env: &str) -> Result<Credential> {
        self.login_with_scopes(name, env, &[]).await
    }

    /// Like [`login`](Dispatcher::login), but requests `additional_scopes` on top
    /// of the provider's defaults.
    ///
    /// The scopes are carried as [`CommandMeta::scopes`] on a synthesized
    /// request; providers without scope support ignore them.
    pub async fn login_with_scopes(
        &self,
        name: &str,
        env: &str,
        additional_scopes: &[String],
    ) -> Result<Credential> {
        let provider = self.get(name)?;
        if let Err(err) = provider.logout(env).await {
            tracing::debug!(provider = name, error = %err, "ignoring logout error before login");
        }
        let mut meta = CommandMeta::default();
        meta.set_scopes(additional_scopes.to_vec());
        let req = CredentialRequest {
            env,
            command: "",
            tier: "",
            meta: &meta,
        };
        provider.get_credential_for(&req).await
    }

    /// Gets cached credential status from a named provider.
    pub async fn status(&self, name: &str, env: &str) -> Result<Credential> {
        self.get(name)?.status(env).await
    }

    /// Clears cached credentials for a named provider and environment.
    pub async fn logout(&self, name: &str, env: &str) -> Result<()> {
        self.get(name)?.logout(env).await
    }

    /// Queries every provider for every cached environment it reports.
    pub async fn all_statuses(&self) -> Vec<StatusEntry> {
        let mut entries = Vec::new();
        let providers = self.read_inner().providers.clone();
        for (name, provider) in providers {
            let Ok(envs) = provider.list_environments().await else {
                continue;
            };
            for env in envs {
                match provider.status(&env).await {
                    Ok(credential) => entries.push(StatusEntry {
                        provider: name.clone(),
                        env,
                        credential: Some(credential),
                        error: None,
                    }),
                    Err(err) => entries.push(StatusEntry {
                        provider: name.clone(),
                        env,
                        credential: None,
                        error: Some(err.to_string()),
                    }),
                }
            }
        }
        entries
    }

    /// Returns an auth-provider facade backed by this dispatcher.
    #[must_use]
    pub fn for_provider(&self, name: impl Into<String>) -> SingleProvider {
        SingleProvider {
            dispatcher: self.clone(),
            name: name.into(),
        }
    }

    fn get(&self, name: &str) -> Result<Arc<dyn AuthProvider>> {
        self.read_inner()
            .providers
            .iter()
            .find(|(existing_name, _)| existing_name == name)
            .map(|(_, provider)| Arc::clone(provider))
            .ok_or_else(|| CliCoreError::MissingAuthProvider(name.to_owned()))
    }

    fn read_inner(&self) -> std::sync::RwLockReadGuard<'_, DispatcherInner> {
        match self.inner.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn write_inner(&self) -> std::sync::RwLockWriteGuard<'_, DispatcherInner> {
        match self.inner.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Single-provider facade over a shared [`Dispatcher`].
#[derive(Clone, Debug)]
pub struct SingleProvider {
    dispatcher: Dispatcher,
    name: String,
}

#[async_trait]
impl AuthProvider for SingleProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn get_credential(&self, env: &str, command: &str, tier: &str) -> Result<Credential> {
        self.dispatcher
            .get_credential(&self.name, env, command, tier)
            .await
    }

    async fn get_credential_for(&self, req: &CredentialRequest<'_>) -> Result<Credential> {
        self.dispatcher.get_credential_for(&self.name, req).await
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        self.dispatcher.status(&self.name, env).await
    }

    async fn logout(&self, env: &str) -> Result<()> {
        self.dispatcher.logout(&self.name, env).await
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        self.dispatcher.get(&self.name)?.list_environments().await
    }
}
