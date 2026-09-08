//! The core execution pipeline: argument normalization, unknown-command
//! correction, built-in command dispatch, and handler invocation.

use std::{future::Future, io::Write, process::ExitCode, sync::Arc, time::Duration};

use clap::ArgMatches;

use super::{
    Cli, CliRunOutput, InitFailure,
    argv0::Argv0Outcome,
    builtins::{completion_args, guide_args, help_args, search_args},
    correction::{
        command_keyword_count, detect_unknown_group_command, format_did_you_mean,
        full_command_correction, group_help_target_parts, inject_subcommand_after_command_path,
        positional_command_tokens, rewrite_group_help_args, rewrite_group_help_if_needed,
        single_leaf_subcommand,
    },
    flags_apply::{
        apply_global_flags, apply_pagination_flags, install_debug_transport_logger,
        pagination_command_base, parse_command_timeout,
    },
    lookup::{
        find_command_by_colon_path, has_root_version_flag,
        normalize_optional_global_flags_before_command,
    },
    render::{
        render_bare_group_discovery, render_cli_error, render_completion_print, render_guide,
        render_help_command, render_root, render_schema, render_search,
    },
    tree_render,
};
use crate::{
    CliCoreError, MiddlewareRequest, Result,
    command::{
        CommandContext, StreamSender, command_args_from_matches, command_path_from_matches,
        leaf_matches,
    },
    error::exit_code_for_error,
    flags::{
        derive_bool_flags, derive_value_flags, extract_command_path, extract_output_format,
        global_flags_from_matches, has_true_schema_flag, output_env_var,
        resolve_default_output_format,
    },
};

/// Executes the CLI with process arguments and process stdout/stderr.
pub(super) async fn execute(cli: &Cli) -> ExitCode {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    match execute_from(cli, std::env::args_os(), &mut stdout, &mut stderr).await {
        Ok(code) => code,
        Err(err) => {
            drop(writeln!(stderr, "{err}"));
            ExitCode::from(1)
        }
    }
}

/// Executes the CLI with caller-provided args and output writers.
pub(super) async fn execute_from<I, S, O, E>(
    cli: &Cli,
    args: I,
    stdout: &mut O,
    stderr: &mut E,
) -> std::io::Result<ExitCode>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
    O: Write,
    E: Write,
{
    execute_from_until_signal(cli, args, stdout, stderr, shutdown_signal()).await
}

/// Executes the CLI until either command completion or a shutdown signal future resolves.
pub(super) async fn execute_from_until_signal<I, S, O, E, Shutdown>(
    cli: &Cli,
    args: I,
    stdout: &mut O,
    stderr: &mut E,
    shutdown: Shutdown,
) -> std::io::Result<ExitCode>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
    O: Write,
    E: Write,
    Shutdown: Future<Output = ()>,
{
    cli.install_default_user_agent();
    let output = run_until_signal(cli.run(args), shutdown).await;
    if output.exit_code == 130
        && output.rendered == "command interrupted\n"
        && let Some(on_shutdown) = &cli.on_shutdown
    {
        on_shutdown();
    }
    if output.exit_code == 0 {
        stdout.write_all(output.rendered.as_bytes())?;
    } else {
        stderr.write_all(output.rendered.as_bytes())?;
    }
    Ok(process_exit_code(output.exit_code))
}

/// Runs the CLI like [`Cli::run`](super::Cli::run), threading the `argv0` dispatch recursion
/// `depth` so a chain of personality hand-offs is bounded by [`MAX_ARGV0_DEPTH`](super::argv0::MAX_ARGV0_DEPTH).
pub(super) async fn run_with_depth<I, S>(cli: &Cli, args: I, depth: usize) -> CliRunOutput
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    let raw_args = args
        .into_iter()
        .map(Into::into)
        .collect::<Vec<std::ffi::OsString>>();
    let text_args = raw_args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let text_args = match super::argv0::resolve_argv0(cli, text_args, depth).await {
        Argv0Outcome::Handled(output) => return output,
        Argv0Outcome::Proceed(args) => args,
    };
    let mut clap_args = normalize_optional_global_flags_before_command(&cli.root, &text_args);
    if has_root_version_flag(&text_args, &cli.root, &cli.config.name) {
        return finish_run(
            cli,
            CliRunOutput {
                exit_code: 0,
                rendered: format!(
                    "{} version {}\n",
                    cli.config.name,
                    cli.config.build.version_string()
                ),
            },
        );
    }
    if let Some(output) = try_run_schema_bypass(cli, &text_args) {
        return output;
    }
    // Resolve the positional command path once and share it between the
    // group-help rewrite and the unknown-command check below.
    let bool_flags = derive_bool_flags(&cli.root);
    let value_flags = derive_value_flags(&cli.root);
    let positionals =
        positional_command_tokens(&text_args, &cli.config.name, &bool_flags, &value_flags);
    let command_keyword_count =
        command_keyword_count(&text_args, &cli.config.name, &bool_flags, &value_flags);
    if let Some(parts) = group_help_target_parts(&cli.root, &positionals, command_keyword_count) {
        // Rewrite `<group> help [sub...]` into the canonical
        // `help <group> [sub...]` so it flows through the curated root
        // `help` command, which also runs global-flag parsing and the
        // `pre_run` hook (matching `help <group>` and bare-group help).
        // Only the positional command tokens are reordered; every flag and
        // its value is preserved in place so e.g. `--output json` survives.
        clap_args = rewrite_group_help_args(
            &clap_args,
            &cli.config.name,
            &bool_flags,
            &value_flags,
            &parts,
        );
    } else if let Some(unknown) =
        detect_unknown_group_command(&cli.root, &positionals[..command_keyword_count])
    {
        // Hint/re-dispatch only when the whole path resolves to one command.
        if let Some(corrections) =
            full_command_correction(&cli.root, &positionals[..command_keyword_count])
        {
            let display = super::correction::correction_display(
                &cli.config.name,
                &positionals[..command_keyword_count],
                &corrections,
            );
            let full_fix_message = format_did_you_mean(&unknown.base, &display);
            match crate::prompt::confirm_command_correction(
                &clap_args,
                &display,
                cli.config.auto_interactive,
            ) {
                crate::prompt::CommandCorrection::Accepted => {
                    for (index, replacement) in &corrections {
                        clap_args = super::correction::replace_positional_command_token(
                            &clap_args,
                            &cli.config.name,
                            &bool_flags,
                            &value_flags,
                            *index,
                            replacement,
                        );
                    }
                    clap_args = rewrite_group_help_if_needed(
                        &cli.root,
                        &clap_args,
                        &cli.config.name,
                        &bool_flags,
                        &value_flags,
                    );
                }
                crate::prompt::CommandCorrection::Declined => {
                    return finish_run(
                        cli,
                        CliRunOutput {
                            exit_code: 1,
                            rendered: full_fix_message,
                        },
                    );
                }
                crate::prompt::CommandCorrection::Cancelled => {
                    return finish_run(
                        cli,
                        CliRunOutput {
                            exit_code: 130,
                            rendered: "Cancelled.".to_owned(),
                        },
                    );
                }
            }
        } else {
            return finish_run(
                cli,
                CliRunOutput {
                    exit_code: 1,
                    rendered: unknown.base,
                },
            );
        }
    }

    let matches = match cli.root.clone().try_get_matches_from(&clap_args) {
        Ok(matches) => matches,
        Err(err) => {
            // Attempt interactive recovery for missing required arguments.
            if let Some(recovery) = crate::prompt::try_recover_missing_args(
                &err,
                &clap_args,
                &cli.root,
                &cli.config.name,
                cli.config.auto_interactive,
            ) {
                match recovery {
                    crate::prompt::RecoveryResult::Recovered { args } => {
                        match cli.root.clone().try_get_matches_from(args) {
                            Ok(m) => m,
                            Err(retry_err) => {
                                return finish_run(
                                    cli,
                                    CliRunOutput {
                                        exit_code: retry_err.exit_code(),
                                        rendered: retry_err.to_string(),
                                    },
                                );
                            }
                        }
                    }
                    crate::prompt::RecoveryResult::Cancelled { resume } => {
                        return finish_run(
                            cli,
                            CliRunOutput {
                                exit_code: 130,
                                rendered: format!("Cancelled. Resume with:\n  {resume}\n"),
                            },
                        );
                    }
                }
            } else {
                return finish_run(
                    cli,
                    CliRunOutput {
                        exit_code: err.exit_code(),
                        rendered: err.to_string(),
                    },
                );
            }
        }
    };

    let default_format = resolve_run_output_format(cli);
    let flags = global_flags_from_matches(&matches, &default_format, cli.config.auto_interactive);
    // Publish the --credential-store override so auth providers resolving
    // their storage backend see it at the top of the precedence chain.
    crate::config::set_credential_store_flag(flags.credential_store);
    let command_timeout = match parse_command_timeout(&flags.timeout) {
        Ok(timeout) => timeout,
        Err(err) => {
            return finish_run(
                cli,
                render_cli_error(&cli.middleware, &err, &cli.config.app_id),
            );
        }
    };
    let mut middleware = cli.middleware.clone();
    apply_global_flags(&mut middleware, &flags, command_timeout);
    install_debug_transport_logger(&flags.debug, &cli.config.redacted_debug_headers);
    if let Err(err) = apply_config_flags(cli, &matches, &mut middleware) {
        return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
    }
    // Validate and apply `--env` for built-in paths (help/tree/guide/group
    // help) so they reflect the selected environment and reject unknowns.
    if let Err(err) = apply_env_flag(&matches, &mut middleware) {
        return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
    }

    let command_path = command_path_from_matches(&cli.config.name, &matches);
    if command_path == "help" {
        if let Err(err) = run_pre_run(cli, &mut middleware, &command_path, &help_args(&matches)) {
            return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
        }
        return finish_run(cli, render_help_command(cli, &matches));
    }
    if command_path == "tree" {
        if let Err(err) = run_pre_run(
            cli,
            &mut middleware,
            &command_path,
            &crate::middleware::ValueMap::new(),
        ) {
            return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
        }
        return finish_run(
            cli,
            tree_render::render_tree(&cli.root, &cli.config.app_id, &middleware),
        );
    }
    if command_path == "guide" {
        if let Err(err) = run_pre_run(cli, &mut middleware, &command_path, &guide_args(&matches)) {
            return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
        }
        return finish_run(cli, render_guide(cli, &matches, &flags.output_format));
    }
    if command_path == "search" {
        let args = search_args(&matches);
        if let Err(err) = run_pre_run(cli, &mut middleware, &command_path, &args) {
            return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
        }
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let scope_path = args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let scope = super::lookup::resolve_search_scope(cli, scope_path);
        return finish_run(cli, render_search(cli, query, &scope, &flags.output_format));
    }
    if command_path == "completion" {
        let args = completion_args(&matches);
        if let Err(err) = run_pre_run(cli, &mut middleware, &command_path, &args) {
            return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
        }
        let install = args
            .get("install")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let shell_opt = args
            .get("shell")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if install {
            use crate::cli::completion::{detect_shell, parse_shell};
            let shell = match shell_opt {
                Some(ref s) => match parse_shell(s) {
                    Ok(s) => s,
                    Err(e) => {
                        return finish_run(
                            cli,
                            render_cli_error(&middleware, &e, &cli.config.app_id),
                        );
                    }
                },
                None => match detect_shell() {
                    Ok(s) => s,
                    Err(e) => {
                        return finish_run(
                            cli,
                            render_cli_error(&middleware, &e, &cli.config.app_id),
                        );
                    }
                },
            };
            return finish_run(
                cli,
                crate::cli::completion::install(&cli.root, &cli.config.name, shell)
                    .await
                    .unwrap_or_else(|e| render_cli_error(&middleware, &e, &cli.config.app_id)),
            );
        }
        return finish_run(cli, render_completion_print(cli, shell_opt, &middleware));
    }
    let Some(command) = cli.commands.get(&command_path) else {
        if !command_path.is_empty()
            && let Some(group) = find_command_by_colon_path(&cli.root, &command_path)
            && group.get_subcommands().next().is_some()
        {
            if let Err(err) = run_pre_run(
                cli,
                &mut middleware,
                &command_path,
                &crate::middleware::ValueMap::new(),
            ) {
                return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
            }
            if middleware.interactive
                && let Some(subcommand) = single_leaf_subcommand(group)
            {
                let augmented = inject_subcommand_after_command_path(
                    &text_args,
                    &cli.config.name,
                    &command_path,
                    &subcommand,
                    &bool_flags,
                    &value_flags,
                );
                return Box::pin(run_with_depth(cli, augmented, depth + 1)).await;
            }
            return finish_run(
                cli,
                render_bare_group_discovery(cli, group, &command_path, &middleware),
            );
        }
        if command_path.is_empty()
            && let Some(root_next_actions) = &cli.root_next_actions
        {
            // Bare-root discovery is static (help text / metadata + action
            // pointers) and must always be available as a cold-start entry
            // point, so we skip `pre_run` here — matching the no-hook
            // bare-root path below, which also renders help without it.
            let actions = root_next_actions();
            return finish_run(cli, render_root(cli, &middleware, actions));
        }
        return finish_run(
            cli,
            CliRunOutput {
                exit_code: if command_path.is_empty() { 0 } else { 1 },
                rendered: if command_path.is_empty() {
                    cli.root.clone().render_long_help().to_string()
                } else {
                    format!("unknown command {command_path:?}")
                },
            },
        );
    };

    let mut middleware = match initialized_middleware(cli) {
        Ok(middleware) => middleware,
        Err(err) => {
            return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
        }
    };
    apply_global_flags(&mut middleware, &flags, command_timeout);
    install_debug_transport_logger(&flags.debug, &cli.config.redacted_debug_headers);
    if let Err(err) = apply_config_flags(cli, &matches, &mut middleware) {
        return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
    }
    // The global `--env` flag overrides the seeded active environment for
    // this invocation; an unknown name surfaces as an error envelope.
    if let Err(err) = apply_env_flag(&matches, &mut middleware) {
        return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
    }

    let leaf = leaf_matches(&matches);
    apply_pagination_flags(&mut middleware, &command.spec, leaf);
    let args = command_args_from_matches(leaf, &command.spec, false);
    let user_args = command_args_from_matches(leaf, &command.spec, true);
    let pagination_command = command.spec.pagination.is_some().then(|| {
        pagination_command_base(
            &cli.config.name,
            &command_path,
            &command.spec,
            &user_args,
            &flags,
        )
    });
    if let Err(err) = run_pre_run(cli, &mut middleware, &command_path, &args) {
        return finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id));
    }
    let meta = resolve_meta(cli, &command_path, command.spec.metadata());
    let default_fields = command.spec.default_fields.clone().unwrap_or_default();
    let system = command.spec.system.clone().unwrap_or_default();
    // The human view this command declared: an explicit shared id wins;
    // otherwise an inline `with_view` was registered under the command path
    // at build time, so reference it by that path. `None` renders generic
    // human output.
    let view_id = command
        .spec
        .view_id
        .clone()
        .or_else(|| (!command.spec.view_columns.is_empty()).then(|| command_path.clone()));

    if let Some(streaming_handler) = command.streaming_handler.clone() {
        let result = run_with_timeout(
            command_timeout,
            &flags.timeout,
            run_streaming_command(
                &middleware,
                MiddlewareRequest {
                    meta,
                    command_path: &command_path,
                    system: &system,
                    user_args,
                    args,
                    default_fields: &default_fields,
                    view_id: view_id.as_deref(),
                    auth: command.spec.auth,
                    raw_output: command.spec.raw_output,
                    pagination_command,
                },
                Arc::new(leaf.clone()),
                streaming_handler,
            ),
        )
        .await;
        return finish_run(
            cli,
            match result {
                Ok(output) => output,
                Err(err) => render_cli_error(&middleware, &err, &cli.config.app_id),
            },
        );
    }

    let handler = command.handler.clone();
    let args_for_handler = args.clone();
    let user_args_for_handler = user_args.clone();
    let handler_path = command_path.clone();
    let middleware_for_handler = middleware.clone();
    let raw_matches_for_handler = Arc::new(leaf.clone());
    let result = run_with_timeout(
        command_timeout,
        &flags.timeout,
        middleware.run(
            MiddlewareRequest {
                meta,
                command_path: &command_path,
                system: &system,
                user_args,
                args,
                default_fields: &default_fields,
                view_id: view_id.as_deref(),
                auth: command.spec.auth,
                raw_output: command.spec.raw_output,
                pagination_command,
            },
            async move |credential| {
                handler(CommandContext {
                    credential,
                    args: args_for_handler,
                    user_args: user_args_for_handler,
                    command_path: handler_path,
                    middleware: middleware_for_handler,
                    raw_matches: raw_matches_for_handler,
                })
                .await
            },
        ),
    )
    .await;

    match result {
        Ok(output) => finish_run(cli, output.into()),
        Err(err) => finish_run(cli, render_cli_error(&middleware, &err, &cli.config.app_id)),
    }
}

pub(super) fn try_run_schema_bypass(cli: &Cli, args: &[String]) -> Option<CliRunOutput> {
    if !has_true_schema_flag(args) {
        return None;
    }
    let bool_flags = derive_bool_flags(&cli.root);
    let value_flags = derive_value_flags(&cli.root);
    let command_path = super::lookup::canonical_command_path(
        cli,
        &extract_command_path(args, &bool_flags, &value_flags),
    );
    // `--schema` is an inspection flag and must not require the command's own
    // arguments, so it short-circuits before clap validates them. Only fire
    // for a real leaf command, though: unknown paths and groups fall through
    // so clap and `detect_unknown_group_command` can report them as usual.
    let command = find_command_by_colon_path(&cli.root, &command_path)?;
    if command.get_subcommands().next().is_some() {
        return None;
    }
    let output_format = extract_output_format(args, &resolve_run_output_format(cli));
    // When no schema is registered, report that rather than running the
    // command — matching the middleware's no-schema response so the public
    // path and the lower layer agree even when required args are missing.
    match cli.middleware.schema_registry.get_by_path(&command_path) {
        Some(schema) => Some(render_schema(cli, schema, &output_format)),
        None => Some(render_schema(
            cli,
            crate::output::no_schema_response(&command_path),
            &output_format,
        )),
    }
}

/// Computes the default output format for this run — the fallback used
/// when no explicit `--output`/`--json`/`--human`/`--toon` is given.
pub(super) fn resolve_run_output_format(cli: &Cli) -> String {
    use std::io::IsTerminal;

    let env = std::env::var(output_env_var(&cli.config.app_id)).ok();
    let engine_config = cli.middleware.config.engine();
    resolve_default_output_format(
        env.as_deref(),
        engine_config.output.format.as_deref(),
        std::io::stdout().is_terminal(),
    )
}

fn initialized_middleware(cli: &Cli) -> Result<crate::Middleware> {
    let Some(init_deps) = &cli.init_deps else {
        return Ok(cli.middleware.clone());
    };
    let mut guard = cli
        .init_state
        .lock()
        .map_err(|_| CliCoreError::message("init deps lock poisoned"))?;
    if let Some(result) = guard.as_ref() {
        return result.clone().map_err(InitFailure::into_error);
    }
    let mut middleware = cli.middleware.clone();
    let result = init_deps(&mut middleware)
        .map(|()| middleware)
        .map_err(|err| InitFailure::capture(&err));
    *guard = Some(result.clone());
    result.map_err(InitFailure::into_error)
}

fn apply_config_flags(
    cli: &Cli,
    matches: &ArgMatches,
    middleware: &mut crate::Middleware,
) -> Result<()> {
    if let Some(apply_flags) = &cli.apply_flags {
        apply_flags(matches, middleware)?;
    }
    Ok(())
}

/// Applies the global `--env` override to a per-run middleware snapshot.
///
/// The flag is only registered when environments are configured, so when it
/// is present `middleware.environments` is set too. Validates the requested
/// name against the registered environments and updates `middleware.env`,
/// returning an error for an unknown environment.
fn apply_env_flag(matches: &ArgMatches, middleware: &mut crate::Middleware) -> Result<()> {
    // Guard on the environment system FIRST. The `--env` arg is only
    // registered when environments are configured (the same condition that
    // sets `middleware.environments`); calling `matches.get_one("env")` for
    // an arg that was never registered panics in clap, which would break
    // every CLI that does not use environments.
    let Some(environments) = middleware.environments.as_ref() else {
        return Ok(());
    };
    if let Some(env) = matches.get_one::<String>("env") {
        environments.source(env)?;
        middleware.env = env.clone();
    }
    Ok(())
}

fn run_pre_run(
    cli: &Cli,
    middleware: &mut crate::Middleware,
    command_path: &str,
    args: &crate::middleware::ValueMap,
) -> Result<()> {
    if let Some(pre_run) = &cli.pre_run {
        pre_run(middleware, command_path, args)?;
    }
    Ok(())
}

fn resolve_meta(cli: &Cli, command_path: &str, meta: crate::CommandMeta) -> crate::CommandMeta {
    if let Some(resolver) = &cli.meta_resolver {
        resolver(command_path, meta)
    } else {
        meta
    }
}

pub(super) fn finish_run(cli: &Cli, output: CliRunOutput) -> CliRunOutput {
    // Clear the per-thread credential-store flag so it does not leak into
    // subsequent sequential runs on the same thread.
    crate::config::clear_credential_store_flag();
    if let Some(on_shutdown) = &cli.on_shutdown {
        on_shutdown();
    }
    output
}

async fn run_with_timeout<F, T>(
    timeout: Option<Duration>,
    timeout_label: &str,
    future: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let Some(timeout) = timeout else {
        return future.await;
    };
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => Err(CliCoreError::message(format!(
            "command timed out after {timeout_label}"
        ))),
    }
}

async fn run_until_signal<Run, Shutdown>(run: Run, shutdown: Shutdown) -> CliRunOutput
where
    Run: Future<Output = CliRunOutput>,
    Shutdown: Future<Output = ()>,
{
    tokio::pin!(run);
    tokio::pin!(shutdown);
    tokio::select! {
        output = &mut run => output,
        () = &mut shutdown => CliRunOutput {
            exit_code: 130,
            rendered: "command interrupted\n".to_owned(),
        },
    }
}

#[cfg(unix)]
pub(super) async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut sigterm) => {
            tokio::select! {
                _ = ctrl_c => {},
                _ = sigterm.recv() => {},
            }
        }
        Err(_) => {
            drop(ctrl_c.await);
        }
    }
}

#[cfg(not(unix))]
pub(super) async fn shutdown_signal() {
    drop(tokio::signal::ctrl_c().await);
}

fn process_exit_code(code: i32) -> ExitCode {
    if code == 0 {
        return ExitCode::SUCCESS;
    }
    match u8::try_from(code) {
        Ok(code) if code != 0 => ExitCode::from(code),
        Ok(_) | Err(_) => ExitCode::from(1),
    }
}

async fn run_streaming_command(
    middleware: &crate::Middleware,
    request: MiddlewareRequest<'_>,
    raw_matches: Arc<ArgMatches>,
    streaming_handler: crate::command::StreamingCommandHandler,
) -> Result<CliRunOutput> {
    use tokio::{io::AsyncWriteExt, sync::mpsc};

    let args_for_handler = request.args.clone();
    let user_args_for_handler = request.user_args.clone();
    let handler_path = request.command_path.to_owned();
    let middleware_for_handler = middleware.clone();
    let raw_matches_for_handler = raw_matches;

    let (tx, mut rx) = mpsc::channel::<serde_json::Value>(64);
    let sender = StreamSender(tx);

    // Drain the channel concurrently so the handler's sends don't stall
    // while the writer flushes to stdout. If stdout is under backpressure
    // the bounded channel can still fill and the handler will await send.
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(event) = rx.recv().await {
            let Ok(line) = serde_json::to_string(&event) else {
                continue;
            };
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });

    let output = middleware
        .run(request, async move |credential| {
            streaming_handler(
                CommandContext {
                    credential,
                    args: args_for_handler,
                    user_args: user_args_for_handler,
                    command_path: handler_path,
                    middleware: middleware_for_handler,
                    raw_matches: raw_matches_for_handler,
                },
                sender,
            )
            .await?;
            Ok(crate::CommandResult::new(serde_json::Value::Null))
        })
        .await;

    // Handler has completed; its sender is dropped, which closes the channel.
    // Wait for the writer task to flush all remaining events.
    let _write_result = writer.await;

    match output {
        Ok(out) if out.exit_code == 0 => Ok(CliRunOutput {
            exit_code: 0,
            rendered: String::new(),
        }),
        Ok(out) => Ok(out.into()),
        Err(err) => Ok(CliRunOutput {
            exit_code: exit_code_for_error(&err),
            rendered: render_cli_error(middleware, &err, middleware.app_id.as_str()).rendered,
        }),
    }
}
