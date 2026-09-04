use std::{collections::BTreeSet, future::Future, time::Instant};

use serde_json::{Value, json};

use super::{
    AuthRequirement, CommandMeta, CredentialResolver, Middleware, MiddlewareOutput,
    MiddlewareRequest, ValueMap, effective_request_system, fallback_system,
};
use crate::{
    CommandResult, Credential, Result,
    error::{CliCoreError, exit_code_for_error},
    output::{
        Envelope, NextAction, OutputFormat, PipelineOpts, apply_pipeline, build_error_envelope,
        is_valid_output_format, render_human_with_registry_selected, unknown_fields_message,
    },
};

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
            raw_output,
            pagination_command,
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
                None,
                false,
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
        // Gated on `self.dry_run` and `meta.handles_dry_run` too: the tag is
        // handler-supplied, untrusted input, so a handler bug that sets it on
        // a real (non-dry-run) run — or on a command that never opted into
        // handler-driven dry-run at all (e.g. a `Tier::Read` handler that
        // always runs, dry-run or not) — must not mis-tag that execution as a
        // dry-run in the audit trail.
        let is_dry_run = self.dry_run && meta.handles_dry_run && metadata.dry_run;
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
            pagination_command.as_deref(),
            raw_output && !is_dry_run,
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
                raw_output: false,
                pagination_command: None,
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
        let event = super::ActivityEvent {
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
                    None,
                    false,
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
        pagination_command: Option<&str>,
        raw_output: bool,
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
        if raw_output {
            match &envelope.data {
                Some(Value::String(text)) => {
                    // Guarantee exactly one trailing newline without doubling
                    // one the handler already included (e.g. text read from
                    // a file that already ends in "\n").
                    let body = text.strip_suffix('\n').unwrap_or(text);
                    let rendered = format!("{body}\n");
                    envelope.with_context(
                        command_path,
                        &self.env,
                        identity,
                        start.elapsed(),
                        Some(Value::Object(user_args.clone())),
                        Some(Value::Object(effective_args.clone())),
                    );
                    let prepared = envelope.prepare_for_render(&self.verbose);
                    return Ok(MiddlewareOutput {
                        envelope: prepared,
                        rendered,
                        exit_code: 0,
                    });
                }
                other => {
                    debug_assert!(
                        false,
                        "command {command_path:?} set raw_output but its handler returned \
                         non-string data ({other:?}); rendering normally instead"
                    );
                }
            }
        }
        let output_format = self.output_format.parse::<OutputFormat>()?;
        // The effective field selection: an explicit `--fields` wins —
        // including an explicit empty string, which keeps everything, same
        // as `all`/`*` — otherwise the command's `default_fields` is the
        // default. Gated on `fields_explicit` rather than
        // `self.fields.is_empty()`: once a command has `default_fields` set,
        // clap fills `self.fields` with that same non-empty string whether
        // or not the user typed `--fields`, so emptiness can't tell "user
        // explicitly cleared it" apart from "user never touched it" — only
        // `value_source` (what `fields_explicit` is built from) can. The
        // same selection is applied two ways: with a registered human view,
        // it narrows which of the view's columns show, so the view reads
        // the full payload — the data is not projected, which would
        // otherwise blank out the kept columns. Everywhere else (JSON/TOON,
        // or generic human output) it projects the output data.
        let effective_fields = if self.fields_explicit {
            self.fields.as_str()
        } else {
            default_fields
        };
        let human_view = output_format == OutputFormat::Human && self.human_views.has_view(view_id);
        // `apply_pipeline` never sees `effective_fields` for a registered
        // view (`projection_fields` below is forced to `""` so the view reads
        // the full payload), and the view's own column narrowing
        // (`select_columns` in `human.rs`) silently skips a name with no
        // matching column — the same "typo produces an empty/partial table
        // instead of an error" gap `apply_pipeline`'s field validation
        // closes elsewhere. So an explicit `--fields` (never a
        // `default_fields` fallback — same reasoning as
        // `PipelineOpts::fields_are_default`) is checked against the view's
        // column catalog here instead.
        if human_view
            && self.fields_explicit
            && let Some(columns) = self.human_views.columns(view_id)
        {
            let fields = effective_fields.trim();
            if !fields.is_empty() && fields != "all" && fields != "*" {
                let known: BTreeSet<String> =
                    columns.iter().map(|column| column.field.clone()).collect();
                let unknown: BTreeSet<&str> = fields
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty() && !known.contains(*part))
                    .collect();
                if !unknown.is_empty() {
                    let unknown: Vec<&str> = unknown.into_iter().collect();
                    let err = CliCoreError::message(unknown_fields_message(&unknown, &known));
                    return self.render_error(
                        &err,
                        &self.app_id,
                        start,
                        user_args,
                        effective_args,
                        identity,
                    );
                }
            }
        }
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
                    fields_are_default: !self.fields_explicit,
                },
            )?;
            if let Some(pagination) = pagination {
                if pagination.has_more
                    && let Some(base) = pagination_command
                {
                    let next_offset = pagination.offset + pagination.count;
                    envelope.next_actions.push(NextAction::new(
                        format!("{base} --limit {} --offset {next_offset}", pagination.limit),
                        format!(
                            "View the next page (offset {next_offset} of {} total)",
                            pagination.total
                        ),
                    ));
                }
                envelope.pagination = Some(pagination);
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
