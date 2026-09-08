use std::{
    collections::BTreeMap,
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
};

use clap::{Arg, Command};

mod argv0;
mod builtins;
mod completion;
mod config;
mod correction;
mod flags_apply;
mod help;
mod lookup;
mod registration;
mod render;
mod run;
mod schema_tree;
mod tree_render;

use crate::{
    AuthProvider, CliCoreError, GuideEntry, Middleware, Module, RuntimeCommandSpec,
    RuntimeGroupSpec,
    error::exit_code_for_error,
    feature_flags::Stage,
    flags::{register_global_flags, register_reason_flag},
    module::ModuleContext,
    output::{global_human_view_registry_snapshot, global_schema_registry_snapshot},
};

pub use argv0::{Argv0LinkMethod, Argv0Route};
use builtins::{completion_command, guide_command, help_command, search_command};
pub use config::{
    ApplyFlags, BuildInfo, CliConfig, ExtraSearchDocs, InitDeps, OnShutdown, PreRun, RegisterFlags,
    ResolveMeta, RootNextActions,
};
use help::ROOT_HELP_TEMPLATE;
pub use help::{ModuleHelpEntry, build_root_long, render_next_actions_human};

/// Captured result of running a CLI in tests or embedding contexts.
#[derive(Clone, Debug, PartialEq)]
pub struct CliRunOutput {
    /// Process-style exit code.
    pub exit_code: i32,
    /// Rendered stdout or stderr payload.
    pub rendered: String,
}

impl From<crate::middleware::MiddlewareOutput> for CliRunOutput {
    fn from(o: crate::middleware::MiddlewareOutput) -> Self {
        Self {
            exit_code: o.exit_code,
            rendered: o.rendered,
        }
    }
}

/// Configured CLI application.
///
/// A `Cli` owns the `clap` command tree, middleware, registered runtime
/// commands, guides, schemas, and built-ins. Consumer binaries normally create
/// one `Cli` and call [`Cli::execute`].
#[derive(Clone)]
pub struct Cli {
    config: CliConfig,
    middleware: Middleware,
    root: Command,
    commands: BTreeMap<String, RuntimeCommandSpec>,
    module_entries: Vec<ModuleHelpEntry>,
    guide_entries: Vec<GuideEntry>,
    init_deps: Option<InitDeps>,
    apply_flags: Option<ApplyFlags>,
    pre_run: Option<PreRun>,
    meta_resolver: Option<ResolveMeta>,
    on_shutdown: Option<OnShutdown>,
    extra_search_docs: Option<ExtraSearchDocs>,
    root_next_actions: Option<RootNextActions>,
    init_state: Arc<Mutex<Option<Result<Middleware, InitFailure>>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InitFailure {
    message: String,
    code: String,
    system: String,
    request_id: String,
    fix: Option<String>,
    exit_code: i32,
}

impl InitFailure {
    fn capture(err: &CliCoreError) -> Self {
        let envelope = crate::output::build_error_envelope(err, "");
        let (code, system, request_id) = envelope.error.map_or_else(
            || ("ERROR".to_owned(), String::new(), String::new()),
            |error| (error.code, error.system, error.request_id),
        );
        Self {
            message: err.to_string(),
            code,
            system,
            request_id,
            fix: envelope.fix,
            exit_code: exit_code_for_error(err),
        }
    }

    fn into_error(self) -> CliCoreError {
        let message = CliCoreError::SystemMessage {
            message: self.message,
            system: self.system,
            code: self.code,
            request_id: self.request_id,
        };
        CliCoreError::with_exit_code(
            self.exit_code,
            CliCoreError::with_fix(self.fix.unwrap_or_default(), message),
        )
    }
}

impl std::fmt::Debug for Cli {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Cli")
            .field("config", &self.config)
            .field("middleware", &self.middleware)
            .field("root", &self.root)
            .field("commands", &self.commands)
            .field("module_entries", &self.module_entries)
            .field("guide_entries", &self.guide_entries)
            .field("has_init_deps", &self.init_deps.is_some())
            .field("has_apply_flags", &self.apply_flags.is_some())
            .field("has_pre_run", &self.pre_run.is_some())
            .field("has_meta_resolver", &self.meta_resolver.is_some())
            .field("has_on_shutdown", &self.on_shutdown.is_some())
            .field("has_extra_search_docs", &self.extra_search_docs.is_some())
            .field("has_root_next_actions", &self.root_next_actions.is_some())
            .finish()
    }
}

impl Cli {
    /// Builds a CLI application from declarative configuration.
    #[must_use]
    pub fn new(config: CliConfig) -> Self {
        let auth_providers = config.auth_providers.clone();
        let guides = config.guides.clone();
        let views = config.views.clone();
        let modules = config.modules.clone();
        let commands = config.commands.clone();
        let init_deps = config.init_deps.clone();
        let apply_flags = config.apply_flags.clone();
        let pre_run = config.pre_run.clone();
        let meta_resolver = config.meta_resolver.clone();
        let on_shutdown = config.on_shutdown.clone();
        let extra_search_docs = config.extra_search_docs.clone();
        let root_next_actions = config.root_next_actions.clone();
        let mut root = Command::new(config.name.clone())
            .about(config.short.clone())
            .disable_help_subcommand(true)
            .version(config.build.version_string());
        if let Some(long) = &config.long
            && !long.is_empty()
        {
            root = root.long_about(long.clone());
        }
        root = register_global_flags(root)
            .subcommand(help_command())
            .subcommand(guide_command())
            .subcommand(Command::new("tree").about("Display full command tree"))
            .subcommand(completion_command())
            .subcommand(search_command());
        if let Some(register_flags) = &config.register_flags {
            root = register_flags(root);
        }
        // `--reason` is only meaningful when something actually consumes it —
        // an authorizer, auditor, or activity emitter. Apps with none of those
        // registered never see the flag at all, rather than a flag whose value
        // is captured and silently discarded. This checks the eager `CliConfig`
        // fields only: an authorizer/auditor/activity emitter installed later via
        // `init_deps` runs per-request, after flag registration, so it can't be
        // observed here. Apps that want `--reason` must set `authz`/`auditor`/
        // `activity` directly on `CliConfig`, not exclusively through `init_deps`.
        if config.authz.is_some() || config.auditor.is_some() || config.activity.is_some() {
            root = register_reason_flag(root);
        }
        if config.environments.is_some() {
            root = root.arg(
                Arg::new("env")
                    .long("env")
                    .global(true)
                    .value_name("ENV")
                    .display_order(crate::flags::global_flag_order::ENV)
                    .help("Override the active environment (see: env list)"),
            );
        }
        let intro = config
            .long
            .as_deref()
            .filter(|long| !long.is_empty())
            .unwrap_or(config.short.as_str());
        root = root
            .long_about(build_root_long(intro, &[], false))
            .help_template(ROOT_HELP_TEMPLATE);

        let mut middleware = Middleware::new();
        middleware.app_id = config.app_id.clone();
        // One-time, macOS-only: move any pre-existing $HOME/.config/<app_id>
        // contents to $HOME/Library/Application Support/<app_id> before the
        // config file below is loaded from its (possibly new) location.
        crate::fs::migrate_macos_config_dir(&config.app_id);
        // Load the per-application config file once at startup; cloned into each
        // per-run middleware so handlers and module registration share it.
        middleware.config = Arc::new(crate::config::ConfigFile::load(&config.app_id));
        middleware.default_auth_provider = config.default_auth_provider.clone().unwrap_or_default();
        middleware.authz = config.authz.clone();
        middleware.auditor = config.auditor.clone();
        middleware.activity = config.activity.clone();
        middleware
            .schema_registry
            .merge(&global_schema_registry_snapshot());
        middleware
            .human_views
            .merge(&global_human_view_registry_snapshot());
        if let Some(environments) = &config.environments {
            // Seed the sticky/default active environment now, but let a
            // startup `--env` win over it if one is present: `prescan_env_flag`
            // scans `startup_args` (or, when unset, the real process argv) the
            // same way `apply_env_flag` will parse it for real per invocation
            // — this is what lets a same-invocation `--env <name>` affect the
            // `flag_policy` computed below (and therefore which flagged
            // commands get pruned), not just `middleware.env`. The real,
            // per-invocation value used for dispatch still comes from
            // `apply_env_flag`'s clap parse in `run_with_depth`; this prescan
            // only decides tree shape earlier than clap otherwise could,
            // since that decision can't be revisited once the tree is built.
            let startup_args = config
                .startup_args
                .clone()
                .unwrap_or_else(|| std::env::args_os().collect());
            let startup_env_flag = flags_apply::prescan_env_flag(
                startup_args
                    .iter()
                    .skip(1) // argv[0] is the program name, same convention `run`/`execute_from` use
                    .map(|arg| arg.to_string_lossy().into_owned()),
            );
            // The same `Arc` the consumer shared with any `PkceAuthProvider` is
            // reused, so the file layer and active-env persistence resolve
            // consistently.
            middleware.env =
                environments.effective_active(startup_env_flag.as_deref(), &middleware.config);
            middleware.environments = Some(Arc::clone(environments));
        }
        let mut flag_policy = config.flag_policy();
        if let Some(min_stage) = flags_apply::global_min_stage_override(&config.app_id) {
            flag_policy.min_stage = min_stage;
        }
        if let Some(environments) = &middleware.environments
            && let Ok(source) = environments.source(&middleware.env)
        {
            let chain = crate::env_config::SourceChain::new().push(&source);
            match crate::env_config::resolve_field::<Stage>(
                &chain,
                "min_stage",
                "min_stage",
                None,
                false,
                crate::env_config::default_from_toml::<Stage>,
                |_raw: &str| -> Result<Stage, String> { Err(String::new()) },
            ) {
                Ok(Some(min_stage)) => flag_policy.min_stage = min_stage,
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(env = %middleware.env, error = %err, "ignoring invalid environment min_stage");
                }
            }
            match crate::env_config::resolve_field::<BTreeMap<String, Stage>>(
                &chain,
                "feature_overrides",
                "feature_overrides",
                None,
                false,
                crate::env_config::default_from_toml::<BTreeMap<String, Stage>>,
                |_raw: &str| -> Result<BTreeMap<String, Stage>, String> { Err(String::new()) },
            ) {
                Ok(Some(overrides)) => flag_policy.overrides.extend(overrides),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(env = %middleware.env, error = %err, "ignoring invalid environment feature_overrides");
                }
            }
        }
        middleware.flag_policy = flag_policy;

        let mut cli = Self {
            config,
            middleware,
            root,
            commands: BTreeMap::new(),
            module_entries: Vec::new(),
            guide_entries: Vec::new(),
            init_deps,
            apply_flags,
            pre_run,
            meta_resolver,
            on_shutdown,
            extra_search_docs,
            root_next_actions,
            init_state: Arc::new(Mutex::new(None)),
        };
        for provider in auth_providers {
            cli.register_auth_provider(provider);
        }
        if cli.middleware.default_auth_provider.is_empty()
            && let Some(provider) = cli.middleware.auth.registered_names().first()
        {
            cli.middleware.default_auth_provider = provider.clone();
        }
        if !cli.middleware.default_auth_provider.is_empty() {
            registration::ensure_auth_command(&mut cli);
        }
        for view in views {
            cli.middleware.human_views.register(view);
        }
        cli.add_guides(guides);
        for module in modules {
            cli.add_module(module);
        }
        for command in commands {
            cli.add_command(command);
        }
        if cli.config.config_commands {
            registration::ensure_config_command(&mut cli);
        }
        if cli.config.environments.is_some() {
            registration::ensure_env_command(&mut cli);
        }
        registration::ensure_flags_command(&mut cli);
        cli
    }

    /// Returns the shared middleware template.
    #[must_use]
    pub fn middleware(&self) -> &Middleware {
        &self.middleware
    }

    /// Returns mutable middleware for advanced application setup.
    pub fn middleware_mut(&mut self) -> &mut Middleware {
        &mut self.middleware
    }

    /// Executes the CLI with process arguments and process stdout/stderr.
    pub async fn execute(&self) -> ExitCode {
        run::execute(self).await
    }

    /// Executes the CLI with caller-provided args and output writers.
    ///
    /// If `args` carries a synthetic `--env` unrelated to real process argv
    /// (or to whatever [`CliConfig::with_startup_args`] this `Cli` was built
    /// with), command-tree pruning — decided once, at construction time —
    /// won't reflect it; see `with_startup_args`'s doc for why.
    pub async fn execute_from<I, S, O, E>(
        &self,
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
        run::execute_from(self, args, stdout, stderr).await
    }

    /// Executes the CLI until either command completion or a shutdown signal future resolves.
    pub async fn execute_from_until_signal<I, S, O, E, Shutdown>(
        &self,
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
        run::execute_from_until_signal(self, args, stdout, stderr, shutdown).await
    }

    /// Publishes the configured outbound User-Agent process-wide so that
    /// command [`HttpClient`](crate::transport::HttpClient)s and the engine's
    /// own OAuth token requests share it.
    ///
    /// Called from the execution entrypoints rather than [`Cli::new`] so that
    /// merely constructing a `Cli` (as tests do in bulk) does not mutate global
    /// state. See [`CliConfig::user_agent_string`] for resolution order.
    fn install_default_user_agent(&self) {
        crate::transport::set_default_user_agent(self.config.user_agent_string());
    }

    /// Registers an auth provider after construction.
    pub fn register_auth_provider(&mut self, provider: Arc<dyn AuthProvider>) -> &mut Self {
        self.middleware.auth.register(provider);
        registration::ensure_auth_command(self);
        registration::refresh_root_long(self);
        self
    }

    /// Returns the built `clap` root command.
    #[must_use]
    pub fn root_command(&self) -> &Command {
        &self.root
    }

    /// Adds one runtime module group after construction.
    pub fn add_module_group(
        &mut self,
        category: impl Into<String>,
        group: RuntimeGroupSpec,
    ) -> &mut Self {
        registration::add_module_group_inner(self, category, group, None)
    }

    /// Adds one module after construction.
    pub fn add_module(&mut self, module: Module) -> &mut Self {
        for view in module.views.clone() {
            self.middleware.human_views.register(view);
        }
        self.add_guides(module.guides.clone());
        let mut context = ModuleContext::new(&mut self.middleware);
        let group = (module.register)(&mut context);
        let (guides, views) = context.into_parts();
        for view in views {
            self.middleware.human_views.register(view);
        }
        self.add_guides(guides);
        registration::add_module_group_inner(
            self,
            module.category,
            group,
            module.feature_flag.clone(),
        )
    }

    /// Adds one top-level runtime command after construction.
    pub fn add_command(&mut self, command: RuntimeCommandSpec) -> &mut Self {
        let name = command.spec.name.clone();
        schema_tree::register_command_schema(
            &command.spec,
            &name,
            &mut self.middleware.schema_registry,
        );
        self.commands.insert(name, command.clone());
        self.root =
            self.root
                .clone()
                .subcommand(schema_tree::command_clap_command_with_schema_help(
                    &command.spec,
                    &command.spec.name,
                    &self.middleware.schema_registry,
                ));
        self
    }

    /// Controls whether the built-in `guide` command is advertised.
    pub fn set_has_guide(&mut self, has_guide: bool) -> &mut Self {
        if has_guide
            && self.guide_entries.is_empty()
            && !lookup::has_subcommand(&self.root, "guide")
        {
            self.root = self.root.clone().subcommand(guide_command());
        }
        registration::sync_guide_topic_values(self);
        registration::refresh_root_long(self);
        self
    }

    /// Adds guide entries after construction.
    pub fn add_guides(&mut self, entries: impl IntoIterator<Item = GuideEntry>) -> &mut Self {
        let mut seen = self
            .guide_entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for entry in entries {
            if seen.insert(entry.name.clone()) {
                self.guide_entries.push(entry);
            }
        }
        if !self.guide_entries.is_empty() && !lookup::has_subcommand(&self.root, "guide") {
            self.root = self.root.clone().subcommand(guide_command());
        }
        registration::sync_guide_topic_values(self);
        registration::refresh_root_long(self);
        self
    }

    /// Returns the registered alternative `argv[0]` names, sorted.
    ///
    /// Useful for install or self-healing code that iterates the names and calls
    /// [`Cli::create_link`] for each.
    #[must_use]
    pub fn argv0_names(&self) -> Vec<&str> {
        self.config
            .argv0_routes
            .keys()
            .map(String::as_str)
            .collect()
    }

    /// Creates an on-disk link in `dir` that lets the binary be invoked under the
    /// registered alternative `argv[0]` name `name`, using `method`.
    ///
    /// `target` is the executable the link points at; pass `None` to use the
    /// current executable ([`std::env::current_exe`]), which is the common choice
    /// for install and self-healing code. The file name follows the platform and
    /// method: a symlink or hard link is `<name>` on Unix and `<name>.exe` on
    /// Windows; a [`Argv0LinkMethod::Script`] shim is `<name>.cmd` on Windows and
    /// an executable `<name>` shell script on Unix.
    ///
    /// The call ensures the desired state idempotently: if the destination already
    /// matches what would be created (a symlink to `target`, a hard link with the
    /// same contents, or a shim with identical contents) it is left untouched and
    /// its path returned; if it exists but differs (wrong kind, stale target, or
    /// edited shim) it is replaced. This makes the call safe to re-run as install
    /// or self-healing code, restoring both deleted and corrupted links. The
    /// directory is created if necessary.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` is not a registered route, if the current
    /// executable cannot be resolved (when `target` is `None`), or if the
    /// directory or link cannot be created or replaced (e.g. insufficient
    /// privilege for a Windows symlink, or a hard link across volumes).
    pub fn create_link(
        &self,
        name: &str,
        dir: impl AsRef<Path>,
        target: Option<&Path>,
        method: Argv0LinkMethod,
    ) -> std::io::Result<PathBuf> {
        argv0::create_link(self, name, dir, target, method)
    }

    /// Runs the CLI with provided args and captures the rendered result.
    ///
    /// Same `--env`/tree-pruning caveat as [`Cli::execute_from`]: see
    /// [`CliConfig::with_startup_args`].
    pub async fn run<I, S>(&self, args: I) -> CliRunOutput
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString> + Clone,
    {
        run::run_with_depth(self, args, 0).await
    }
}
