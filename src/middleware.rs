use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, OnceCell};

use crate::{
    CommandResult, Credential, CredentialRequest, Dispatcher, FlagPolicy, FlagRegistry, Result,
    SchemaRegistry, Tier,
    error::{CliCoreError, exit_code_for_error},
    output::{
        Envelope, HumanViewRegistry, OutputFormat, PipelineOpts, apply_pipeline,
        build_error_envelope, is_valid_output_format, render_human_with_registry_selected,
    },
};

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
    /// Search query, interpreted before command execution.
    pub search: String,
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
}

impl Middleware {
    /// Creates middleware with empty registries and default dependencies.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs the middleware chain for a command.
    pub async fn run<F, Fut, Output>(
        &self,
        request: MiddlewareRequest<'_>,
        command: F,
    ) -> Result<MiddlewareOutput>
    where
        F: FnOnce(CredentialResolver) -> Fut + Send,
        Fut: Future<Output = Result<Output>> + Send,
        Output: Into<CommandResult>,
    {
        let start = Instant::now();
        let MiddlewareRequest {
            meta,
            command_path,
            system,
            user_args,
            mut args,
            default_fields,
            view_id,
            auth,
        } = request;
        let no_auth = auth.is_none();
        let command_system = effective_request_system(system, command_path);
        if !no_auth && !self.env.is_empty() && !args.contains_key("env") {
            args.insert("env".to_owned(), Value::String(self.env.clone()));
        }

        // Build a lazy resolver instead of resolving eagerly. No auth flow runs
        // until a handler or authorizer actually asks for the credential, so
        // commands that never use it (and `--schema`/`--dry-run`) skip auth.
        let provider_name = meta
            .provider()
            .filter(|provider| !provider.is_empty())
            .unwrap_or(&self.default_auth_provider)
            .to_owned();
        let resolved_env = meta.fixed_env().unwrap_or(&self.env).to_owned();
        let tier_text = meta
            .auth_metadata
            .get("tier")
            .map_or("", String::as_str)
            .to_owned();
        let resolver = CredentialResolver::new(
            self.auth.clone(),
            provider_name.clone(),
            resolved_env,
            command_path.to_owned(),
            tier_text,
            no_auth,
            meta.clone(),
        );

        if no_auth
            && let Some(output) =
                self.render_schema_if_requested(command_path, start, &user_args, &args, "")?
        {
            return Ok(output);
        }

        if let Some(authz) = &self.authz
            && let Err(err) = authz
                .authorize(command_path, &args, &resolver, &self.reason, meta.tier())
                .await
        {
            // An authorizer may have resolved the credential to make its
            // decision; reflect whatever it resolved in audit identity.
            let identity = resolver.peek().map_or("", |cred| cred.identity.as_str());
            // Classify by the error the authorizer returned: a propagated
            // resolution failure is auth-typed; a policy denial is not.
            let had_auth_error = err.is_auth();
            let result_tag = if had_auth_error {
                "auth-error"
            } else {
                "denied"
            };
            // Attribute auth-provider failures to the provider so telemetry can
            // distinguish them from command backends.
            let backend = if had_auth_error {
                provider_name.as_str()
            } else {
                command_path
            };
            self.write_audit(command_path, &args, identity, result_tag)
                .await;
            self.emit_activity(
                command_path,
                &args,
                resolver.peek(),
                result_tag,
                backend,
                &err.to_string(),
                start,
            )
            .await;
            return self.render_error(&err, command_path, start, &user_args, &args, identity);
        }

        // If the authorizer resolved the credential, include its identity in the
        // schema output metadata. `peek()` never triggers resolution, so schema
        // still doesn't provoke auth on its own.
        let schema_identity = resolver.peek().map_or("", |cred| cred.identity.as_str());
        if let Some(output) = self.render_schema_if_requested(
            command_path,
            start,
            &user_args,
            &args,
            schema_identity,
        )? {
            return Ok(output);
        }

        if self.dry_run && meta.dry_run_prompt && !meta.handles_dry_run {
            let identity = resolver.peek().map_or("", |cred| cred.identity.as_str());
            self.write_audit(command_path, &args, identity, "dry-run")
                .await;
            self.emit_activity(
                command_path,
                &args,
                resolver.peek(),
                "dry-run",
                command_path,
                "",
                start,
            )
            .await;
            let envelope = Envelope::success(
                json!({
                    "command": command_path,
                    "action": "dry-run: would execute",
                }),
                command_path,
            )
            .with_dry_run();
            return self.render_envelope(
                envelope,
                "",
                "",
                command_path,
                start,
                &user_args,
                &args,
                identity,
            );
        }

        // Fail closed by default: for `Required` commands the engine resolves the
        // credential before the handler runs, so a command that must be
        // authenticated cannot execute unauthenticated even if its handler never
        // reads the credential, and its audit/activity identity is always
        // populated. `--schema`/`--dry-run` return above, so they never reach this
        // point; `Optional`/`None` commands defer resolution to the handler.
        if auth.is_required()
            && let Err(err) = resolver.resolve().await
        {
            // Mirror the handler-path auth-error treatment: classify as
            // `auth-error` and attribute the activity backend to the auth provider
            // so telemetry can distinguish auth-provider failures from command
            // backends. Resolution failed, so there is no identity to record.
            self.write_audit(command_path, &args, "", "auth-error")
                .await;
            self.emit_activity(
                command_path,
                &args,
                resolver.peek(),
                "auth-error",
                provider_name.as_str(),
                &err.to_string(),
                start,
            )
            .await;
            return self.render_error(&err, command_path, start, &user_args, &args, "");
        }

        let result = match command(resolver.clone()).await {
            Ok(result) => result.into(),
            Err(err) => {
                // A deferred `resolve()` failure surfaces as a handler error;
                // classify it as `auth-error` when the error the handler returned
                // is itself auth-typed. A handler that swallows a resolution
                // failure and then fails for another reason returns a non-auth
                // error here, so it is not misclassified.
                let identity = resolver.peek().map_or("", |cred| cred.identity.as_str());
                let (result_tag, error_system, activity_backend) = if err.is_auth() {
                    // Render against the command path, but attribute the activity
                    // backend to the auth provider so telemetry can distinguish
                    // auth-provider failures from command backends.
                    ("auth-error", command_path, provider_name.as_str())
                } else {
                    let system = err.system().unwrap_or(&command_system);
                    ("error", system, system)
                };
                self.write_audit(command_path, &args, identity, result_tag)
                    .await;
                self.emit_activity(
                    command_path,
                    &args,
                    resolver.peek(),
                    result_tag,
                    activity_backend,
                    &err.to_string(),
                    start,
                )
                .await;
                return self.render_error(&err, error_system, start, &user_args, &args, identity);
            }
        };
        // The handler may have resolved the credential; surface its identity.
        let identity = resolver.peek().map_or("", |cred| cred.identity.as_str());
        let CommandResult { data, metadata } = result;
        // A `handles_dry_run` handler that tagged its result via
        // `CommandResult::with_dry_run` reports a `dry-run` outcome instead of
        // `ok`, matching the generic short-circuit's audit/activity tagging.
        // Gated on `self.dry_run` too: the tag is handler-supplied, untrusted
        // input, so a handler bug that sets it on a real (non-dry-run) run
        // must not mis-tag that execution as a dry-run in the audit trail.
        let is_dry_run = self.dry_run && metadata.dry_run;
        let outcome = if is_dry_run { "dry-run" } else { "ok" };
        self.write_audit(command_path, &args, identity, outcome)
            .await;
        self.emit_activity(
            command_path,
            &args,
            resolver.peek(),
            outcome,
            &command_system,
            "",
            start,
        )
        .await;

        let mut envelope =
            Envelope::success(data, command_system).with_next_actions(metadata.next_actions);
        if is_dry_run {
            envelope = envelope.with_dry_run();
        }
        self.render_envelope(
            envelope,
            default_fields,
            view_id.unwrap_or_default(),
            command_path,
            start,
            &user_args,
            &args,
            identity,
        )
    }

    #[doc(hidden)]
    pub async fn run_no_auth<F, Fut>(
        &self,
        meta: CommandMeta,
        command_path: &str,
        user_args: ValueMap,
        args: ValueMap,
        default_fields: &str,
        command: F,
    ) -> Result<MiddlewareOutput>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<CommandResult>> + Send,
    {
        self.run(
            MiddlewareRequest {
                meta,
                command_path,
                system: fallback_system(command_path),
                user_args,
                args,
                default_fields,
                view_id: None,
                auth: AuthRequirement::None,
            },
            async move |_resolver| command().await,
        )
        .await
    }

    async fn write_audit(&self, command_path: &str, args: &ValueMap, identity: &str, result: &str) {
        if let Some(auditor) = &self.auditor
            && let Err(err) = auditor
                .append(command_path, args, identity, result, &self.reason)
                .await
        {
            tracing::warn!(command = command_path, error = %err, "audit log write failed");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn emit_activity(
        &self,
        command_path: &str,
        args: &ValueMap,
        credential: Option<&Credential>,
        result: &str,
        backend: &str,
        error: &str,
        start: Instant,
    ) {
        let Some(activity) = &self.activity else {
            return;
        };
        let (identity, sub, account_type) = credential.map_or_else(
            || (String::new(), String::new(), String::new()),
            |credential| {
                (
                    credential.identity.clone(),
                    credential.sub.clone(),
                    credential.account_type.clone(),
                )
            },
        );
        let duration_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
        let event = ActivityEvent {
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            app: self.app_id.clone(),
            command: command_path.to_owned(),
            env: self.env.clone(),
            backend: backend.to_owned(),
            identity,
            sub,
            account_type,
            status: result.to_owned(),
            error: error.to_owned(),
            reason: self.reason.clone(),
            args: args.clone(),
            duration_ms,
            meta: ValueMap::new(),
        };
        if let Err(err) = activity.emit(event).await {
            tracing::warn!(command = command_path, error = %err, "activity emit failed");
        }
    }

    fn render_schema_if_requested(
        &self,
        command_path: &str,
        start: Instant,
        user_args: &ValueMap,
        effective_args: &ValueMap,
        identity: &str,
    ) -> Result<Option<MiddlewareOutput>> {
        if self.schema {
            // Registered schema: dump it. Otherwise don't silently run the
            // command — report that no schema exists. (We deliberately don't
            // suggest "run it with --fields all" here: that would execute the
            // command, which is exactly wrong for a mutation.)
            let envelope = match self.schema_registry.get_by_path(command_path) {
                Some(schema) => Envelope::success(schema, self.app_id.clone()),
                // Shared with the `Cli::run` `--schema` bypass so both paths emit
                // an identical no-schema body: the same `{command, fields}` shape
                // as a real SchemaInfo response (empty `fields`) plus an additive
                // `message`.
                None => Envelope::success(
                    crate::output::no_schema_response(command_path),
                    self.app_id.clone(),
                ),
            };
            return self
                .render_envelope(
                    envelope,
                    "",
                    "",
                    command_path,
                    start,
                    user_args,
                    effective_args,
                    identity,
                )
                .map(Some);
        }
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_envelope(
        &self,
        mut envelope: Envelope,
        default_fields: &str,
        view_id: &str,
        command_path: &str,
        start: Instant,
        user_args: &ValueMap,
        effective_args: &ValueMap,
        identity: &str,
    ) -> Result<MiddlewareOutput> {
        if !is_valid_output_format(&self.output_format) {
            let err = CliCoreError::InvalidOutputFormat(self.output_format.clone());
            return self.render_error(
                &err,
                &self.app_id,
                start,
                user_args,
                effective_args,
                identity,
            );
        }
        let output_format = self.output_format.parse::<OutputFormat>()?;
        // The effective field selection: an explicit `--fields` wins, otherwise
        // the command's `default_fields` is the default. The same selection is
        // applied two ways. With a registered human view, it narrows which of the
        // view's columns show, so the view reads the full payload — the data is
        // not projected, which would otherwise blank out the kept columns.
        // Everywhere else (JSON/TOON, or generic human output) it projects the
        // output data. Empty / `all` / `*` keeps everything.
        let effective_fields = if self.fields.is_empty() {
            default_fields
        } else {
            self.fields.as_str()
        };
        let human_view = output_format == OutputFormat::Human && self.human_views.has_view(view_id);
        let projection_fields = if human_view { "" } else { effective_fields };
        if let Some(data) = &mut envelope.data {
            let pagination = apply_pipeline(
                data,
                &PipelineOpts {
                    filter: self.filter.clone(),
                    limit: self.limit,
                    offset: self.offset,
                    expr: self.expr.clone(),
                    fields: projection_fields.to_owned(),
                },
            )?;
            if let Some(pagination) = pagination
                && let Some(metadata) = &mut envelope.metadata
            {
                metadata.pagination = Some(pagination);
            }
        }
        envelope.with_context(
            command_path,
            &self.env,
            identity,
            start.elapsed(),
            Some(Value::Object(user_args.clone())),
            Some(Value::Object(effective_args.clone())),
        );
        let prepared = envelope.prepare_for_render(&self.verbose);
        let rendered = if output_format == OutputFormat::Human {
            render_human_with_registry_selected(
                &prepared,
                &self.human_views,
                view_id,
                effective_fields,
            )
        } else {
            crate::output::render(output_format, &prepared)?
        };
        Ok(MiddlewareOutput {
            envelope: prepared,
            rendered,
            exit_code: 0,
        })
    }

    fn render_error(
        &self,
        err: &(dyn std::error::Error + 'static),
        system: &str,
        start: Instant,
        user_args: &ValueMap,
        effective_args: &ValueMap,
        identity: &str,
    ) -> Result<MiddlewareOutput> {
        let mut envelope = build_error_envelope(err, system);
        envelope.with_context(
            "",
            &self.env,
            identity,
            start.elapsed(),
            Some(Value::Object(user_args.clone())),
            Some(Value::Object(effective_args.clone())),
        );
        let prepared = envelope.prepare_for_render(&self.verbose);
        let rendered = crate::output::render_format(&self.output_format, &prepared)?;
        Ok(MiddlewareOutput {
            envelope: prepared,
            rendered,
            exit_code: exit_code_for_error(err),
        })
    }
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
