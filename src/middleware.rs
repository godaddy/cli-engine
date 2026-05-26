use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

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

#[async_trait]
/// Authorization hook called after credential resolution and before business logic.
pub trait Authorizer: Send + Sync + std::fmt::Debug {
    /// Verifies whether `command_path` may run with the provided args, reason, and tier.
    async fn authorize(
        &self,
        command_path: &str,
        args: &ValueMap,
        credential: Option<&Credential>,
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
        F: FnOnce(Option<Credential>) -> Fut + Send,
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

        let credential = if no_auth {
            None
        } else {
            let provider_name = meta
                .provider()
                .filter(|provider| !provider.is_empty())
                .unwrap_or(&self.default_auth_provider);
            let resolved_env = meta.fixed_env().unwrap_or(&self.env);
            let tier_text = meta.auth_metadata.get("tier").map_or("", String::as_str);
            match self
                .auth
                .get_credential(provider_name, resolved_env, command_path, tier_text)
                .await
            {
                Ok(credential) => Some(credential),
                Err(err) => {
                    self.write_audit(command_path, &args, "", "auth-error")
                        .await;
                    self.emit_activity(
                        command_path,
                        &args,
                        None,
                        "auth-error",
                        provider_name,
                        &err.to_string(),
                        start,
                    )
                    .await;
                    return self.render_error(&err, command_path, start, &user_args, &args, "");
                }
            }
        };
        let identity = credential
            .as_ref()
            .map_or("", |credential| credential.identity.as_str());

        if no_auth
            && let Some(output) =
                self.render_schema_if_requested(command_path, start, &user_args, &args, identity)?
        {
            return Ok(output);
        }

        if let Some(authz) = &self.authz
            && let Err(err) = authz
                .authorize(
                    command_path,
                    &args,
                    credential.as_ref(),
                    &self.reason,
                    meta.tier(),
                )
                .await
        {
            self.write_audit(command_path, &args, identity, "denied")
                .await;
            self.emit_activity(
                command_path,
                &args,
                credential.as_ref(),
                "denied",
                command_path,
                &err.to_string(),
                start,
            )
            .await;
            return self.render_error(&err, command_path, start, &user_args, &args, identity);
        }

        if let Some(output) =
            self.render_schema_if_requested(command_path, start, &user_args, &args, identity)?
        {
            return Ok(output);
        }

        if self.dry_run && meta.dry_run_prompt {
            self.write_audit(command_path, &args, identity, "dry-run")
                .await;
            self.emit_activity(
                command_path,
                &args,
                credential.as_ref(),
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

        let result = match command(credential.clone()).await {
            Ok(result) => result.into(),
            Err(err) => {
                let error_system = err.system().unwrap_or(&command_system);
                self.write_audit(command_path, &args, identity, "error")
                    .await;
                self.emit_activity(
                    command_path,
                    &args,
                    credential.as_ref(),
                    "error",
                    error_system,
                    &err.to_string(),
                    start,
                )
                .await;
                return self.render_error(&err, error_system, start, &user_args, &args, identity);
            }
        };
        self.write_audit(command_path, &args, identity, "ok").await;
        self.emit_activity(
            command_path,
            &args,
            credential.as_ref(),
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
            async move |_credential| command().await,
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
