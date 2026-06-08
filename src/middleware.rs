use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::OnceCell;

use crate::{
    CommandResult, Credential, Dispatcher, Result, SchemaRegistry, Tier,
    error::{CliCoreError, exit_code_for_error},
    output::{
        Envelope, HumanViewRegistry, OutputFormat, PipelineOpts, apply_pipeline,
        build_error_envelope, is_valid_output_format, render_human_with_registry_for_schema,
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
}

/// Lazily resolves the credential for a single command invocation.
///
/// Credential resolution — including any interactive browser/OAuth flow — is
/// deferred until a handler or authorizer actually calls [`resolve`](Self::resolve)
/// or [`try_resolve`](Self::try_resolve). Commands that never ask for a credential
/// therefore never trigger an authentication flow, and `--schema`/`--dry-run`
/// short-circuit before any resolution happens.
///
/// The resolved credential is memoized: a handler and an authorizer that both
/// ask share a single resolution. Clones share the same underlying state, so the
/// engine can observe (via [`peek`](Self::peek)) whatever a handler resolved.
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
    cell: OnceCell<Credential>,
    auth_error: AtomicBool,
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
    ) -> Self {
        Self {
            inner: Arc::new(ResolverInner {
                auth,
                provider,
                env,
                command_path,
                tier,
                no_auth,
                cell: OnceCell::new(),
                auth_error: AtomicBool::new(false),
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
        let inner = &self.inner;
        let credential = inner
            .cell
            .get_or_try_init(async || {
                inner
                    .auth
                    .get_credential(
                        &inner.provider,
                        &inner.env,
                        &inner.command_path,
                        &inner.tier,
                    )
                    .await
                    // Track the outcome of the latest attempt so a retry that
                    // succeeds after an earlier failure does not leave the flag
                    // stale and misclassify a later non-auth error.
                    .inspect(|_| inner.auth_error.store(false, Ordering::Relaxed))
                    .inspect_err(|_| inner.auth_error.store(true, Ordering::Relaxed))
            })
            .await?;
        Ok(credential.clone())
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

    /// Reports whether a resolution attempt failed, so the engine can classify
    /// the outcome as `auth-error` rather than a generic command error.
    fn had_auth_error(&self) -> bool {
        self.inner.auth_error.load(Ordering::Relaxed)
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
    /// Whether credential resolution should be skipped.
    pub no_auth: bool,
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
            no_auth,
        } = request;
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
            let had_auth_error = resolver.had_auth_error();
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

        if self.dry_run && meta.dry_run_prompt {
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
                command_path,
                start,
                &user_args,
                &args,
                identity,
            );
        }

        let result = match command(resolver.clone()).await {
            Ok(result) => result.into(),
            Err(err) => {
                // A lazy `resolve()` failure surfaces as a handler error; keep
                // classifying it as `auth-error` so audits match the prior
                // eager behavior.
                let identity = resolver.peek().map_or("", |cred| cred.identity.as_str());
                let (result_tag, error_system, activity_backend) = if resolver.had_auth_error() {
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
        self.write_audit(command_path, &args, identity, "ok").await;
        self.emit_activity(
            command_path,
            &args,
            resolver.peek(),
            "ok",
            &command_system,
            "",
            start,
        )
        .await;

        let CommandResult { data, metadata } = result;
        self.render_envelope(
            Envelope::success(data, command_system).with_next_actions(metadata.next_actions),
            default_fields,
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
                no_auth: true,
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
        if self.schema
            && let Some(schema) = self.schema_registry.get_by_path(command_path)
        {
            return self
                .render_envelope(
                    Envelope::success(schema, self.app_id.clone()),
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
        let mut fields = if self.fields.is_empty() {
            default_fields
        } else {
            &self.fields
        };
        if output_format == OutputFormat::Human && self.fields.is_empty() {
            fields = "";
        }
        if let Some(data) = &mut envelope.data {
            let pagination = apply_pipeline(
                data,
                &PipelineOpts {
                    filter: self.filter.clone(),
                    limit: self.limit,
                    offset: self.offset,
                    expr: self.expr.clone(),
                    fields: fields.to_owned(),
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
        let system = envelope
            .metadata
            .as_ref()
            .map(|metadata| metadata.system.as_str())
            .unwrap_or_default()
            .to_owned();
        let prepared = envelope.prepare_for_render(&self.verbose);
        let rendered = if output_format == OutputFormat::Human {
            render_human_with_registry_for_schema(&prepared, &self.human_views, &system)
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
