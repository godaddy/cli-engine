use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{Mutex, OnceCell};

use crate::{
    Credential, CredentialRequest, Dispatcher, FlagPolicy, FlagRegistry, Result, SchemaRegistry,
    Tier,
    error::CliCoreError,
    output::{Envelope, HumanViewRegistry},
};

mod run;

/// JSON object map used for command args and metadata.
pub type ValueMap = Map<String, Value>;

/// Per-command metadata consumed by middleware.
///
/// Command specs build this metadata automatically. Applications can also
/// adjust it through `CliConfig::meta_resolver`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandMeta {
    /// Whether `--dry-run` should short-circuit command business logic.
    pub dry_run_prompt: bool,
    /// Whether the command handles `--dry-run` itself instead of being
    /// generically short-circuited. See
    /// [`CommandSpec::handles_dry_run`](crate::CommandSpec::handles_dry_run).
    pub handles_dry_run: bool,
    /// Provider-specific auth metadata.
    pub auth_metadata: BTreeMap<String, String>,
    /// OAuth-style scopes derived from `auth_metadata["scopes"]`.
    pub scopes: Vec<String>,
}

impl CommandMeta {
    /// Returns the selected auth provider, if one is present.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        self.auth_metadata.get("provider").map(String::as_str)
    }

    /// Returns the risk tier, defaulting to [`Tier::Read`].
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.auth_metadata
            .get("tier")
            .and_then(|value| value.parse::<Tier>().ok())
            .unwrap_or(Tier::Read)
    }

    /// Returns a fixed auth environment override, if present.
    #[must_use]
    pub fn fixed_env(&self) -> Option<&str> {
        self.auth_metadata.get("fixed_env").map(String::as_str)
    }

    /// Sets the OAuth scopes, keeping [`scopes`](CommandMeta::scopes) and
    /// `auth_metadata["scopes"]` consistent.
    ///
    /// `scopes` is documented as derived from `auth_metadata["scopes"]`, so any
    /// code that synthesizes or widens scopes (e.g. runtime step-up) should use
    /// this rather than assigning the field directly, so metadata-aware providers
    /// reading `auth_metadata` see the same set. An empty list removes the key.
    pub fn set_scopes(&mut self, scopes: Vec<String>) {
        if scopes.is_empty() {
            self.auth_metadata.remove("scopes");
        } else {
            self.auth_metadata
                .insert("scopes".to_owned(), scopes.join(" "));
        }
        self.scopes = scopes;
    }
}

/// Declares whether a command requires an authenticated credential.
///
/// This is the policy that the engine enforces; it is separate from the
/// *mechanism* of resolution (see [`CredentialResolver`]). The default is
/// [`Required`](AuthRequirement::Required), which fails closed: the engine
/// resolves the credential before the handler runs, so a command that should be
/// gated behind authentication cannot execute unauthenticated even if its
/// handler never reads the credential, and audit/activity identity is always
/// populated for it.
///
/// `--schema` and `--dry-run` short-circuit before the engine resolves a
/// `Required` credential, so they never trigger an authentication flow on their
/// own regardless of requirement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum AuthRequirement {
    /// The engine resolves the credential before the handler runs (fail-closed).
    ///
    /// A failure to resolve is rendered as an `auth-error` and the handler never
    /// runs. This is the default.
    #[default]
    Required,
    /// Resolution is deferred to the handler.
    ///
    /// The engine does not resolve a credential on the command's behalf; the
    /// handler (or an authorizer) triggers the auth flow only by calling
    /// [`CredentialResolver::resolve`]/[`try_resolve`](CredentialResolver::try_resolve).
    /// Use for commands that behave differently when authenticated but must still
    /// run when the user is logged out.
    Optional,
    /// The command never authenticates and has no credential.
    ///
    /// Equivalent to the legacy `no_auth(true)` marker: default-env injection is
    /// suppressed and [`CredentialResolver::resolve`] returns an error.
    None,
}

impl AuthRequirement {
    /// Returns `true` when the command never authenticates.
    #[must_use]
    pub fn is_none(self) -> bool {
        matches!(self, Self::None)
    }

    /// Returns `true` when the engine must resolve the credential before the handler runs.
    #[must_use]
    pub fn is_required(self) -> bool {
        matches!(self, Self::Required)
    }

    /// Returns `true` when resolution is deferred to the handler.
    #[must_use]
    pub fn is_optional(self) -> bool {
        matches!(self, Self::Optional)
    }
}

/// Resolves the credential for a single command invocation, memoizing the result.
///
/// Resolution — including any interactive browser/OAuth flow — runs once for a
/// given scope set: a handler and an authorizer that both ask share a single
/// resolution, and the engine resolves it up front for
/// [`AuthRequirement::Required`] commands. For [`Optional`](AuthRequirement::Optional)
/// commands resolution is deferred until a handler or authorizer calls
/// [`resolve`](Self::resolve) or [`try_resolve`](Self::try_resolve), and
/// `--schema`/`--dry-run` short-circuit before any resolution happens.
///
/// [`resolve_with_scopes`](Self::resolve_with_scopes) may trigger an *additional*
/// resolution when it needs scopes the memoized credential does not yet cover
/// (OAuth scope step-up); a scope-aware provider then re-authenticates for the
/// wider set. Resolutions are serialized, so concurrent callers never launch
/// overlapping interactive flows.
///
/// The resolved credential is memoized: callers that need no new scopes share a
/// single resolution. Clones share the same underlying state, so the engine can
/// observe (via [`peek`](Self::peek)) whatever a handler resolved.
#[derive(Clone)]
pub struct CredentialResolver {
    inner: Arc<ResolverInner>,
}

#[derive(Debug)]
struct ResolverInner {
    auth: Dispatcher,
    provider: String,
    env: String,
    command_path: String,
    tier: String,
    no_auth: bool,
    /// Static command metadata; `meta.scopes` are always requested.
    meta: CommandMeta,
    /// Authoritative resolved credential plus the scopes it was requested with.
    /// Serializes concurrent resolution and lets scope step-up replace a
    /// previously-resolved (narrower) credential.
    state: Mutex<ResolveState>,
    /// Write-once mirror of the first resolved credential so [`CredentialResolver::peek`]
    /// can lend a reference without holding a lock. `peek` (used for audit/activity
    /// identity) therefore reflects the *first* resolved credential and is not
    /// replaced by a later step-up. That is sound because step-up is required to
    /// re-authenticate the *same* identity: [`resolve_scopes`](CredentialResolver::resolve_scopes)
    /// aborts if a step-up returns a different account, so the mirrored identity
    /// always matches the identity that performed every action in the command.
    cell: OnceCell<Credential>,
}

#[derive(Debug, Default)]
struct ResolveState {
    credential: Option<Credential>,
    requested: Vec<String>,
}

impl std::fmt::Debug for CredentialResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialResolver")
            .field("provider", &self.inner.provider)
            .field("env", &self.inner.env)
            .field("no_auth", &self.inner.no_auth)
            .field("resolved", &self.inner.cell.get().is_some())
            .finish_non_exhaustive()
    }
}

impl CredentialResolver {
    fn new(
        auth: Dispatcher,
        provider: String,
        env: String,
        command_path: String,
        tier: String,
        no_auth: bool,
        meta: CommandMeta,
    ) -> Self {
        Self {
            inner: Arc::new(ResolverInner {
                auth,
                provider,
                env,
                command_path,
                tier,
                no_auth,
                meta,
                state: Mutex::new(ResolveState::default()),
                cell: OnceCell::new(),
            }),
        }
    }

    /// Resolves the credential, memoizing the result after the first success.
    ///
    /// # Errors
    ///
    /// Returns an error when the command is marked [`no_auth`](crate::CommandSpec::no_auth)
    /// (such commands have no credential), or when the auth provider fails to
    /// produce one.
    pub async fn resolve(&self) -> Result<Credential> {
        if self.inner.no_auth {
            return Err(CliCoreError::message(
                "command is marked no_auth and has no credential",
            ));
        }
        self.resolve_scopes(&[]).await
    }

    /// Resolves a credential that additionally covers `extra` scopes (on top of
    /// the command's declared [`CommandMeta::scopes`]).
    ///
    /// Used by handlers whose required scopes are only known at runtime (for
    /// example a generic `api call` that derives scopes from the target
    /// endpoint). A scope-aware auth provider re-authenticates when the cached
    /// token does not already cover the requested set.
    ///
    /// # Ordering with the transport injector
    ///
    /// The HTTP transport's bearer injector resolves its token through the
    /// provider's scope-*unaware* path and caches the first token it sees for the
    /// injector's lifetime. So when a handler both steps up scopes and makes HTTP
    /// calls through that injector, call `resolve_with_scopes` (or
    /// [`CommandContext::credential_with_scopes`](crate::CommandContext::credential_with_scopes))
    /// **before** the first request: that populates the provider cache with the
    /// wider-scoped token, which the injector then picks up. Resolving after the
    /// injector's first `inject` would send the narrower token.
    ///
    /// # Errors
    ///
    /// Returns an error when the command is marked
    /// [`no_auth`](crate::CommandSpec::no_auth), or when the auth provider fails
    /// to produce a credential.
    pub async fn resolve_with_scopes(&self, extra: &[String]) -> Result<Credential> {
        if self.inner.no_auth {
            return Err(CliCoreError::message(
                "command is marked no_auth and has no credential",
            ));
        }
        self.resolve_scopes(extra).await
    }

    /// Shared resolution: returns the memoized credential when it already covers
    /// the wanted scopes, otherwise (re)authenticates requesting the union and
    /// updates the memoized credential.
    async fn resolve_scopes(&self, extra: &[String]) -> Result<Credential> {
        let inner = &self.inner;
        let mut want = inner.meta.scopes.clone();
        for scope in extra {
            if !want.contains(scope) {
                want.push(scope.clone());
            }
        }

        let mut state = inner.state.lock().await;
        if let Some(credential) = &state.credential
            && want.iter().all(|scope| state.requested.contains(scope))
        {
            return Ok(credential.clone());
        }

        let mut requested = state.requested.clone();
        for scope in &want {
            if !requested.contains(scope) {
                requested.push(scope.clone());
            }
        }
        let mut meta = inner.meta.clone();
        meta.set_scopes(requested.clone());
        let req = CredentialRequest::new(&inner.env, &inner.command_path, &inner.tier, &meta);
        let credential = inner
            .auth
            .get_credential_for(&inner.provider, &req)
            .await
            // Mark resolution failures so the engine can classify them as
            // `auth-error` based on the error a handler actually returns.
            .map_err(|source| auth_resolution_error(&inner.provider, source))?;
        // Guard against a step-up that re-authenticates as a *different* identity.
        // `peek` (audit/activity identity) reflects the first resolution, so a
        // silent account switch would misattribute the elevated action. Abort
        // rather than proceed under a mismatched identity.
        if let Some(previous) = &state.credential {
            let previous_key = identity_key(previous);
            let new_key = identity_key(&credential);
            if !previous_key.is_empty() && !new_key.is_empty() && previous_key != new_key {
                return Err(CliCoreError::message(format!(
                    "scope step-up authenticated as a different identity \
                     (was {previous_key:?}, now {new_key:?}); aborting"
                )));
            }
        }
        state.credential = Some(credential.clone());
        state.requested = requested;
        // Mirror the first resolution for `peek`; ignored once already set.
        drop(inner.cell.set(credential.clone()));
        Ok(credential)
    }

    /// Resolves the credential when one is available.
    ///
    /// Returns `Ok(None)` for no-auth commands, `Ok(Some(_))` on success, and
    /// propagates the provider error on failure. Use this for commands whose
    /// auth is genuinely optional; most commands should call
    /// [`resolve`](Self::resolve) instead.
    ///
    /// # Errors
    ///
    /// Propagates the auth provider error when resolution is attempted and fails.
    pub async fn try_resolve(&self) -> Result<Option<Credential>> {
        if self.inner.no_auth {
            return Ok(None);
        }
        self.resolve().await.map(Some)
    }

    /// Returns the memoized credential without triggering resolution.
    ///
    /// Yields `None` until something resolves the credential. Used by the engine
    /// to record identity in audit/activity output after a handler runs.
    #[must_use]
    pub fn peek(&self) -> Option<&Credential> {
        self.inner.cell.get()
    }
}

/// Marks a credential-resolution failure so its auth origin is detectable via
/// [`CliCoreError::is_auth`], leaving errors that are already auth-typed
/// unchanged. Display is preserved except for the `auth: provider …:` prefix that
/// the [`AuthProvider`](CliCoreError::AuthProvider) wrapper adds.
fn auth_resolution_error(provider: &str, source: CliCoreError) -> CliCoreError {
    match source {
        auth @ (CliCoreError::MissingAuthProvider(_) | CliCoreError::AuthProvider { .. }) => auth,
        other => CliCoreError::AuthProvider {
            provider: provider.to_owned(),
            source: Box::new(other),
        },
    }
}

/// Stable identity discriminator for a credential: the subject (`sub`) when set,
/// otherwise the human identity. Empty when the provider exposes neither, in
/// which case the step-up identity guard cannot (and does not) compare.
fn identity_key(credential: &Credential) -> &str {
    if credential.sub.is_empty() {
        credential.identity.as_str()
    } else {
        credential.sub.as_str()
    }
}

#[async_trait]
/// Authorization hook called before business logic.
///
/// The authorizer receives a [`CredentialResolver`] rather than an
/// already-resolved credential so authorization remains lazy: an authorizer that
/// does not need identity never triggers a credential/auth flow. Call
/// [`CredentialResolver::try_resolve`] only when a decision actually depends on
/// the credential.
pub trait Authorizer: Send + Sync + std::fmt::Debug {
    /// Verifies whether `command_path` may run with the provided args, reason, and tier.
    async fn authorize(
        &self,
        command_path: &str,
        args: &ValueMap,
        credential: &CredentialResolver,
        reason: &str,
        tier: Tier,
    ) -> Result<()>;
}

#[async_trait]
/// Audit hook called for success, error, denied, auth-error, and dry-run outcomes.
pub trait Auditor: Send + Sync + std::fmt::Debug {
    /// Appends an audit record.
    async fn append(
        &self,
        command_path: &str,
        args: &ValueMap,
        identity: &str,
        result: &str,
        reason: &str,
    ) -> Result<()>;
}

#[async_trait]
/// Activity hook for structured command lifecycle events.
pub trait ActivityEmitter: Send + Sync + std::fmt::Debug {
    /// Emits one completed command event.
    async fn emit(&self, event: ActivityEvent) -> Result<()>;
}

/// Structured activity event emitted after command execution paths.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActivityEvent {
    /// UTC timestamp in RFC3339 seconds format.
    pub timestamp: String,
    /// CLI application id.
    pub app: String,
    /// Colon-separated command path.
    pub command: String,
    /// Selected environment.
    pub env: String,
    /// Backend/system id.
    pub backend: String,
    /// Human identity from the resolved credential.
    pub identity: String,
    /// Subject identifier from the resolved credential.
    pub sub: String,
    /// Account type from the resolved credential.
    pub account_type: String,
    /// Outcome such as `ok`, `error`, `denied`, `auth-error`, or `dry-run`.
    pub status: String,
    /// Error message for failed outcomes.
    pub error: String,
    /// User-provided reason.
    pub reason: String,
    /// Effective command args.
    pub args: ValueMap,
    /// Command duration in milliseconds.
    pub duration_ms: i64,
    /// Reserved extension metadata.
    pub meta: ValueMap,
}

/// Cross-cutting command execution state and dependencies.
///
/// Middleware is intentionally a plain, cloneable struct so tests and command
/// handlers can inspect what will be used for a run. Application setup usually
/// mutates it through `CliConfig` hooks or `ModuleContext`.
#[derive(Clone, Debug, Default)]
pub struct Middleware {
    /// Optional authorization provider.
    pub authz: Option<Arc<dyn Authorizer>>,
    /// Auth provider dispatcher.
    pub auth: Dispatcher,
    /// Optional audit sink.
    pub auditor: Option<Arc<dyn Auditor>>,
    /// Optional activity sink.
    pub activity: Option<Arc<dyn ActivityEmitter>>,
    /// Application id used in output metadata.
    pub app_id: String,
    /// Fallback auth provider for commands without an explicit provider.
    pub default_auth_provider: String,
    /// Output format: `json`, `human`, or `toon`.
    pub output_format: String,
    /// Selected environment.
    pub env: String,
    /// Metadata verbosity selector.
    pub verbose: String,
    /// Whether mutating commands should short-circuit.
    pub dry_run: bool,
    /// User field projection.
    pub fields: String,
    /// Whether `fields` came from an explicit `--fields` flag rather than a
    /// command's `default_fields` fallback. See
    /// [`GlobalFlags::fields_explicit`](crate::GlobalFlags::fields_explicit).
    pub fields_explicit: bool,
    /// JMESPath per-item list predicate.
    pub filter: String,
    /// JMESPath whole-result expression.
    pub expr: String,
    /// Client-side page size.
    pub limit: i64,
    /// Client-side page offset.
    pub offset: i64,
    /// User reason passed to authorization and audit.
    pub reason: String,
    /// Whether schema rendering was requested.
    pub schema: bool,
    /// Optional command deadline.
    pub timeout: Option<Duration>,
    /// Debug selector, interpreted by applications.
    pub debug: String,
    /// Whether the invocation is running in interactive mode.
    pub interactive: bool,
    /// Output schema registry.
    pub schema_registry: SchemaRegistry,
    /// Human output view registry.
    pub human_views: HumanViewRegistry,
    /// Loaded per-application config file, shared across the run.
    ///
    /// Populated once at startup from `<config-base>/<app_id>/config.toml`.
    /// Command handlers read it via
    /// [`CommandContext::config`](crate::command::CommandContext::config) and
    /// module registration via
    /// [`ModuleContext::config`](crate::module::ModuleContext::config).
    pub config: Arc<crate::config::ConfigFile>,
    /// Optional first-class environment system.
    ///
    /// Set by [`CliConfig::with_environments`](crate::CliConfig::with_environments)
    /// and cloned into each per-run middleware snapshot. Handlers resolve the
    /// active environment through
    /// [`CommandContext::environment`](crate::command::CommandContext::environment).
    pub environments: Option<Arc<crate::environments::Environments>>,
    /// Merged feature-flag visibility policy for this run.
    ///
    /// Set by [`CliConfig`](crate::CliConfig)'s `min_stage`/`feature_overrides`
    /// (via its private `flag_policy()` helper) when [`Cli::new`](crate::Cli::new)
    /// builds middleware, before any module or group is registered. Command-tree
    /// pruning consults this to decide which flagged commands, groups, and
    /// modules remain mounted.
    pub flag_policy: FlagPolicy,
    /// Every flagged module/group/command path discovered while pruning the
    /// command tree, populated as modules and groups are registered.
    ///
    /// Powers `flags list`/`flags info` introspection.
    pub flag_registry: FlagRegistry,
}

/// Rendered result produced by middleware.
#[derive(Clone, Debug, PartialEq)]
pub struct MiddlewareOutput {
    /// Prepared output envelope.
    pub envelope: Envelope,
    /// Rendered output string.
    pub rendered: String,
    /// Process-style exit code.
    pub exit_code: i32,
}

/// Inputs for one middleware-managed command execution.
#[derive(Clone, Debug, PartialEq)]
pub struct MiddlewareRequest<'request> {
    /// Per-command metadata used by authentication, authorization, dry-run, audit, and activity.
    pub meta: CommandMeta,
    /// Colon-separated command path.
    pub command_path: &'request str,
    /// Backend/system id used in output metadata and generic error attribution.
    pub system: &'request str,
    /// Arguments explicitly supplied by the user.
    pub user_args: ValueMap,
    /// Effective arguments, including defaults.
    pub args: ValueMap,
    /// Default field projection when `--fields` is absent.
    pub default_fields: &'request str,
    /// Id of the human view this command declared, if any.
    ///
    /// The command path for an inline [`with_view`](crate::CommandSpec::with_view),
    /// or the shared id from [`with_view_id`](crate::CommandSpec::with_view_id).
    /// `None` renders generic human output.
    pub view_id: Option<&'request str>,
    /// Authentication requirement enforced by the engine for this command.
    pub auth: AuthRequirement,
    /// Mirrors [`CommandSpec::raw_output`](crate::CommandSpec::raw_output):
    /// when `true`, a successful string result renders verbatim, bypassing
    /// the format/pipeline machinery entirely.
    pub raw_output: bool,
    /// The invoked command replayed as `--flag value` text — command path
    /// plus every flag the user explicitly passed, using clap's own
    /// long-flag names — with `--limit`/`--offset` deliberately omitted.
    ///
    /// `Some` only for a command that opted into
    /// [`CommandSpec::with_pagination`](crate::CommandSpec::with_pagination);
    /// the engine appends `--limit`/`--offset` for the next page and surfaces
    /// it as a `next_actions` entry when the response has more data. `None`
    /// for every other command, and for any caller driving [`Middleware`]
    /// directly (e.g. [`Middleware::run_no_auth`]) instead of through
    /// [`Cli`](crate::Cli), which is what computes this.
    pub pagination_command: Option<String>,
}

/// Convenience helper for building a JSON object map.
#[must_use]
pub fn value_map(entries: impl IntoIterator<Item = (impl Into<String>, Value)>) -> ValueMap {
    entries
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect()
}

fn effective_request_system(system: &str, command_path: &str) -> String {
    if system.is_empty() {
        return fallback_system(command_path).to_owned();
    }
    system.to_owned()
}

fn fallback_system(command_path: &str) -> &str {
    command_path
        .split_once(':')
        .map_or(command_path, |(system, _)| system)
}

impl From<CliCoreError> for Value {
    fn from(error: CliCoreError) -> Self {
        Value::String(error.to_string())
    }
}

#[cfg(test)]
mod env_wire_tests {
    use super::*;

    #[test]
    fn middleware_carries_optional_environments() {
        use std::sync::Arc;
        let mut mw = Middleware::new();
        assert!(mw.environments.is_none());
        mw.environments = Some(Arc::new(crate::environments::Environments::new("prod")));
        assert_eq!(
            mw.environments
                .as_ref()
                .map(|envs| envs.default_env().to_owned()),
            Some("prod".to_owned())
        );
    }
}
