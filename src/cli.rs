use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io::Write,
    process::ExitCode,
    sync::{Arc, Mutex},
    time::Duration,
};

mod builtins;
mod help;
mod tree_render;

use clap::{ArgMatches, Command};

use crate::{
    ActivityEmitter, Auditor, AuthProvider, Authorizer, CliCoreError, CommandMeta, CommandSpec,
    GroupSpec, GuideEntry, Middleware, MiddlewareRequest, Result, RuntimeCommandSpec,
    RuntimeGroupSpec,
    auth::commands::auth_command_group,
    command::{
        CommandContext, StreamSender, command_args_from_matches, command_path_from_matches,
        leaf_matches,
    },
    error::exit_code_for_error,
    flags::{
        GlobalFlags, default_output_format, derive_bool_flags, derive_value_flags,
        extract_command_path, extract_output_format, extract_search_query,
        global_flags_from_matches, has_true_schema_flag, register_global_flags,
    },
    guide::guide_content,
    module::{Module, ModuleContext},
    output::{
        HumanViewDef, NextAction, SchemaRegistry, format_help_section,
        global_human_view_registry_snapshot, global_schema_registry_snapshot,
    },
    search::{SearchDocument, SearchIndex},
};

use builtins::{guide_args, guide_command, help_args, help_command};
use help::{GROUP_HELP_TEMPLATE, ROOT_HELP_TEMPLATE};
pub use help::{ModuleHelpEntry, build_root_long, render_next_actions_human};

/// Build metadata shown by the root `--version` flag.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BuildInfo {
    /// Semantic version or other release label.
    pub version: String,
    /// Optional source control commit identifier.
    pub commit: Option<String>,
    /// Optional build date string.
    pub date: Option<String>,
}

impl BuildInfo {
    /// Creates build metadata with only a version string.
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            commit: None,
            date: None,
        }
    }

    /// Adds a commit identifier to the version string shown by `--version`.
    #[must_use]
    pub fn with_commit(mut self, commit: impl Into<String>) -> Self {
        self.commit = Some(commit.into());
        self
    }

    /// Adds a build date to the version string shown by `--version`.
    #[must_use]
    pub fn with_date(mut self, date: impl Into<String>) -> Self {
        self.date = Some(date.into());
        self
    }

    /// Returns the rendered version string used by the root `--version` flag.
    #[must_use]
    pub fn version_string(&self) -> String {
        let commit = self.commit.as_deref().unwrap_or_default();
        let date = self.date.as_deref().unwrap_or_default();

        if commit.is_empty() && date.is_empty() {
            self.version.clone()
        } else {
            format!("{} (commit {commit}, built {date})", self.version)
        }
    }
}

/// Late dependency initializer run once before real command execution.
pub type InitDeps = Arc<dyn Fn(&mut Middleware) -> Result<()> + Send + Sync>;
/// Hook used to add application-specific global flags to the root `clap` command.
pub type RegisterFlags = Arc<dyn Fn(Command) -> Command + Send + Sync>;
/// Hook used to copy parsed application-specific flags into middleware.
pub type ApplyFlags = Arc<dyn Fn(&ArgMatches, &mut Middleware) -> Result<()> + Send + Sync>;
/// Hook run immediately before executable commands and built-ins.
pub type PreRun =
    Arc<dyn Fn(&mut Middleware, &str, &crate::middleware::ValueMap) -> Result<()> + Send + Sync>;
/// Hook used to adjust command metadata globally before middleware executes.
pub type ResolveMeta = Arc<dyn Fn(&str, CommandMeta) -> CommandMeta + Send + Sync>;
/// Hook called after a CLI run completes.
pub type OnShutdown = Arc<dyn Fn() + Send + Sync>;
/// Hook that contributes extra root-scope `--search` documents.
pub type ExtraSearchDocs = Arc<dyn Fn() -> Vec<SearchDocument> + Send + Sync>;
/// Hook that supplies the suggested next actions shown when the CLI is invoked
/// with no subcommand (bare root). The same actions drive a human "Next actions"
/// section and the JSON discovery envelope.
pub type RootNextActions = Arc<dyn Fn() -> Vec<NextAction> + Send + Sync>;

/// Default name for the admin help category, under which the engine files the
/// built-in `auth` command when a consumer does not override it via
/// [`CliConfig::with_admin_category`].
const DEFAULT_ADMIN_CATEGORY: &str = "Admin";

/// Declarative configuration for a CLI application.
///
/// Use [`CliConfig::new`] for the common path and chain `with_*` methods for
/// modules, auth providers, guides, views, and lifecycle hooks. Direct struct
/// literals remain available for advanced setup and tests.
#[derive(Clone, Default)]
pub struct CliConfig {
    /// Root command name shown in usage output.
    pub name: String,
    /// One-line root command description.
    pub short: String,
    /// Optional longer root command description. Defaults to `short`.
    pub long: Option<String>,
    /// Version/build metadata for `--version`.
    pub build: BuildInfo,
    /// Application id stored in middleware and output metadata.
    pub app_id: String,
    /// Fallback auth provider when a command does not select one explicitly.
    pub default_auth_provider: Option<String>,
    /// Domain modules mounted under the root command.
    pub modules: Vec<Module>,
    /// Additional top-level runtime commands.
    pub commands: Vec<RuntimeCommandSpec>,
    /// Global guide entries mounted under `guide`.
    pub guides: Vec<GuideEntry>,
    /// Global human output views.
    pub views: Vec<HumanViewDef>,
    /// Providers registered before command execution starts.
    pub auth_providers: Vec<Arc<dyn AuthProvider>>,
    /// Optional authorization gatekeeper injected into middleware.
    pub authz: Option<Arc<dyn Authorizer>>,
    /// Optional audit recorder injected into middleware.
    pub auditor: Option<Arc<dyn Auditor>>,
    /// Optional activity event sink injected into middleware.
    pub activity: Option<Arc<dyn ActivityEmitter>>,
    /// Optional late initializer for runtime dependencies.
    pub init_deps: Option<InitDeps>,
    /// Optional hook for adding application-specific global flags.
    pub register_flags: Option<RegisterFlags>,
    /// Optional hook for applying parsed application-specific flags.
    pub apply_flags: Option<ApplyFlags>,
    /// Optional hook run before executable commands and built-ins.
    pub pre_run: Option<PreRun>,
    /// Optional hook for global command metadata adjustments.
    pub meta_resolver: Option<ResolveMeta>,
    /// Optional hook called after each run.
    pub on_shutdown: Option<OnShutdown>,
    /// Optional root-scope search document provider.
    pub extra_search_docs: Option<ExtraSearchDocs>,
    /// Optional provider for the bare-root suggested next actions.
    pub root_next_actions: Option<RootNextActions>,
    /// Name of the admin help category. The engine files its built-in `auth`
    /// command under this heading; apps should use the same name for their own
    /// admin modules (e.g. godaddy's `env`). When unset, defaults to `"Admin"`;
    /// set it to match a consumer's own taxonomy (e.g. gdx's "Administration").
    pub admin_category: Option<String>,
}

impl CliConfig {
    /// Creates the minimum useful CLI configuration.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        short: impl Into<String>,
        app_id: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            short: short.into(),
            app_id: app_id.into(),
            ..Self::default()
        }
    }

    /// Sets root long help text.
    #[must_use]
    pub fn with_long(mut self, long: impl Into<String>) -> Self {
        self.long = Some(long.into());
        self
    }

    /// Sets build metadata used by `--version`.
    #[must_use]
    pub fn with_build(mut self, build: BuildInfo) -> Self {
        self.build = build;
        self
    }

    /// Sets the fallback auth provider for commands that do not name one.
    #[must_use]
    pub fn with_default_auth_provider(mut self, provider: impl Into<String>) -> Self {
        self.default_auth_provider = Some(provider.into());
        self
    }

    /// Adds one domain module.
    #[must_use]
    pub fn with_module(mut self, module: Module) -> Self {
        self.modules.push(module);
        self
    }

    /// Adds several domain modules.
    #[must_use]
    pub fn with_modules(mut self, modules: impl IntoIterator<Item = Module>) -> Self {
        self.modules.extend(modules);
        self
    }

    /// Adds a top-level runtime command outside a module.
    #[must_use]
    pub fn with_command(mut self, command: RuntimeCommandSpec) -> Self {
        self.commands.push(command);
        self
    }

    /// Adds one global guide.
    #[must_use]
    pub fn with_guide(mut self, guide: GuideEntry) -> Self {
        self.guides.push(guide);
        self
    }

    /// Adds several global guides.
    #[must_use]
    pub fn with_guides(mut self, guides: impl IntoIterator<Item = GuideEntry>) -> Self {
        self.guides.extend(guides);
        self
    }

    /// Adds one global human view.
    #[must_use]
    pub fn with_view(mut self, view: HumanViewDef) -> Self {
        self.views.push(view);
        self
    }

    /// Registers one auth provider.
    #[must_use]
    pub fn with_auth_provider(mut self, provider: Arc<dyn AuthProvider>) -> Self {
        self.auth_providers.push(provider);
        self
    }

    /// Sets the authorization gatekeeper.
    #[must_use]
    pub fn with_authz(mut self, authz: Arc<dyn Authorizer>) -> Self {
        self.authz = Some(authz);
        self
    }

    /// Sets the audit recorder.
    #[must_use]
    pub fn with_auditor(mut self, auditor: Arc<dyn Auditor>) -> Self {
        self.auditor = Some(auditor);
        self
    }

    /// Sets the activity event sink.
    #[must_use]
    pub fn with_activity(mut self, activity: Arc<dyn ActivityEmitter>) -> Self {
        self.activity = Some(activity);
        self
    }

    /// Sets the late dependency initializer.
    #[must_use]
    pub fn with_init_deps(mut self, init_deps: InitDeps) -> Self {
        self.init_deps = Some(init_deps);
        self
    }

    /// Sets the application-specific global flag registration hook.
    #[must_use]
    pub fn with_register_flags(mut self, register_flags: RegisterFlags) -> Self {
        self.register_flags = Some(register_flags);
        self
    }

    /// Sets the application-specific parsed flag application hook.
    #[must_use]
    pub fn with_apply_flags(mut self, apply_flags: ApplyFlags) -> Self {
        self.apply_flags = Some(apply_flags);
        self
    }

    /// Sets the pre-run hook.
    #[must_use]
    pub fn with_pre_run(mut self, pre_run: PreRun) -> Self {
        self.pre_run = Some(pre_run);
        self
    }

    /// Sets the command metadata resolver hook.
    #[must_use]
    pub fn with_meta_resolver(mut self, meta_resolver: ResolveMeta) -> Self {
        self.meta_resolver = Some(meta_resolver);
        self
    }

    /// Sets the shutdown hook.
    #[must_use]
    pub fn with_on_shutdown(mut self, on_shutdown: OnShutdown) -> Self {
        self.on_shutdown = Some(on_shutdown);
        self
    }

    /// Sets the provider for additional root-scope search documents.
    #[must_use]
    pub fn with_extra_search_docs(mut self, extra_search_docs: ExtraSearchDocs) -> Self {
        self.extra_search_docs = Some(extra_search_docs);
        self
    }

    /// Sets the provider for the bare-root suggested next actions.
    #[must_use]
    pub fn with_root_next_actions(mut self, root_next_actions: RootNextActions) -> Self {
        self.root_next_actions = Some(root_next_actions);
        self
    }

    /// Sets the name of the admin help category. The engine files the built-in
    /// `auth` command there; apps should use the same name for their own admin
    /// modules (e.g. godaddy's `env`). Optional: defaults to `"Admin"`.
    #[must_use]
    pub fn with_admin_category(mut self, category: impl Into<String>) -> Self {
        self.admin_category = Some(category.into());
        self
    }
}

impl std::fmt::Debug for CliConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CliConfig")
            .field("name", &self.name)
            .field("short", &self.short)
            .field("long", &self.long)
            .field("build", &self.build)
            .field("app_id", &self.app_id)
            .field("default_auth_provider", &self.default_auth_provider)
            .field("modules", &self.modules)
            .field("commands", &self.commands)
            .field("guides", &self.guides)
            .field("views", &self.views)
            .field("auth_providers_len", &self.auth_providers.len())
            .field("has_authz", &self.authz.is_some())
            .field("has_auditor", &self.auditor.is_some())
            .field("has_activity", &self.activity.is_some())
            .field("has_init_deps", &self.init_deps.is_some())
            .field("has_register_flags", &self.register_flags.is_some())
            .field("has_apply_flags", &self.apply_flags.is_some())
            .field("has_pre_run", &self.pre_run.is_some())
            .field("has_meta_resolver", &self.meta_resolver.is_some())
            .field("has_on_shutdown", &self.on_shutdown.is_some())
            .field("has_extra_search_docs", &self.extra_search_docs.is_some())
            .field("has_root_next_actions", &self.root_next_actions.is_some())
            .field("admin_category", &self.admin_category)
            .finish()
    }
}

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
    init_state: Arc<Mutex<Option<std::result::Result<Middleware, InitFailure>>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InitFailure {
    message: String,
    code: String,
    system: String,
    request_id: String,
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
            exit_code: exit_code_for_error(err),
        }
    }

    fn into_error(self) -> CliCoreError {
        CliCoreError::with_exit_code(
            self.exit_code,
            CliCoreError::SystemMessage {
                message: self.message,
                system: self.system,
                code: self.code,
                request_id: self.request_id,
            },
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
            .subcommand(Command::new("tree").about("Display full command tree"));
        if let Some(register_flags) = &config.register_flags {
            root = register_flags(root);
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
            cli.ensure_auth_command();
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
        // Registered last so `auth` appends to its category after module-defined
        // entries, preserving the consumer's category ordering.
        cli.register_auth_help_entry();
        cli
    }

    /// Lists the auto-registered `auth` command under the admin help category so
    /// it is never uncategorized once clap's auto subcommand list is suppressed.
    /// Defaults to [`DEFAULT_ADMIN_CATEGORY`]; `admin_category` overrides it to
    /// align with a consumer's own taxonomy.
    fn register_auth_help_entry(&mut self) {
        let category = self
            .config
            .admin_category
            .clone()
            .unwrap_or_else(|| DEFAULT_ADMIN_CATEGORY.to_owned());
        let already_listed = self.module_entries.iter().any(|entry| entry.name == "auth");
        let short = self
            .root
            .find_subcommand("auth")
            .filter(|auth| !auth.is_hide_set())
            .map(|auth| {
                auth.get_about()
                    .map(ToString::to_string)
                    .unwrap_or_default()
            });
        if !already_listed && let Some(short) = short {
            self.module_entries.push(ModuleHelpEntry {
                category,
                name: "auth".to_owned(),
                short,
            });
        }
        self.refresh_root_long();
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
        let mut stdout = std::io::stdout().lock();
        let mut stderr = std::io::stderr().lock();
        match self
            .execute_from(std::env::args_os(), &mut stdout, &mut stderr)
            .await
        {
            Ok(code) => code,
            Err(err) => {
                drop(writeln!(stderr, "{err}"));
                ExitCode::from(1)
            }
        }
    }

    /// Executes the CLI with caller-provided args and output writers.
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
        self.execute_from_until_signal(args, stdout, stderr, shutdown_signal())
            .await
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
        let output = run_until_signal(self.run(args), shutdown).await;
        if output.exit_code == 130
            && output.rendered == "command interrupted\n"
            && let Some(on_shutdown) = &self.on_shutdown
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

    /// Registers an auth provider after construction.
    pub fn register_auth_provider(&mut self, provider: Arc<dyn AuthProvider>) -> &mut Self {
        self.middleware.auth.register(provider);
        self.ensure_auth_command();
        self.refresh_root_long();
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
        let category = category.into();
        if !group.group.hidden {
            self.module_entries.push(ModuleHelpEntry {
                category,
                name: group.group.name.clone(),
                short: group.group.short.clone(),
            });
        }

        let mut prefix = Vec::new();
        register_runtime_group_schemas(&group, &mut prefix, &mut self.middleware.schema_registry);
        let mut prefix = Vec::new();
        group.register_commands(&mut prefix, &mut self.commands);
        let mut prefix = Vec::new();
        let clap_group = runtime_group_clap_command_with_schema_help(
            &group,
            &mut prefix,
            &self.middleware.schema_registry,
        );
        self.root = self.root.clone().subcommand(clap_group);
        self.refresh_root_long();
        self
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
        self.add_module_group(module.category, group)
    }

    /// Adds one top-level runtime command after construction.
    pub fn add_command(&mut self, command: RuntimeCommandSpec) -> &mut Self {
        let name = command.spec.name.clone();
        register_command_schema(&command.spec, &name, &mut self.middleware.schema_registry);
        self.commands.insert(name, command.clone());
        self.root = self
            .root
            .clone()
            .subcommand(command_clap_command_with_schema_help(
                &command.spec,
                &command.spec.name,
                &self.middleware.schema_registry,
            ));
        self
    }

    /// Controls whether the built-in `guide` command is advertised.
    pub fn set_has_guide(&mut self, has_guide: bool) -> &mut Self {
        if has_guide && self.guide_entries.is_empty() && !has_subcommand(&self.root, "guide") {
            self.root = self.root.clone().subcommand(guide_command());
        }
        self.refresh_root_long();
        self
    }

    /// Adds guide entries after construction.
    pub fn add_guides(&mut self, entries: impl IntoIterator<Item = GuideEntry>) -> &mut Self {
        let mut seen = self
            .guide_entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<BTreeSet<_>>();
        for entry in entries {
            if seen.insert(entry.name.clone()) {
                self.guide_entries.push(entry);
            }
        }
        if !self.guide_entries.is_empty() && !has_subcommand(&self.root, "guide") {
            self.root = self.root.clone().subcommand(guide_command());
        }
        self.refresh_root_long();
        self
    }

    /// Runs the CLI with provided args and captures the rendered result.
    pub async fn run<I, S>(&self, args: I) -> CliRunOutput
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
        let clap_args = normalize_optional_global_flags_before_command(&self.root, &text_args);
        if has_root_version_flag(&text_args, &self.root, &self.config.name) {
            return self.finish_run(CliRunOutput {
                exit_code: 0,
                rendered: format!(
                    "{} version {}\n",
                    self.config.name,
                    self.config.build.version_string()
                ),
            });
        }
        if let Some(output) = self.try_run_schema_bypass(&text_args) {
            return output;
        }
        if let Some(output) = self.try_run_search_bypass(&text_args) {
            return output;
        }
        if let Some(message) =
            unknown_group_command_message(&self.root, &text_args, &self.config.name)
        {
            return self.finish_run(CliRunOutput {
                exit_code: 1,
                rendered: message,
            });
        }

        let matches = match self.root.clone().try_get_matches_from(clap_args) {
            Ok(matches) => matches,
            Err(err) => {
                return self.finish_run(CliRunOutput {
                    exit_code: err.exit_code(),
                    rendered: err.to_string(),
                });
            }
        };

        let default_format = default_output_format(&self.config.app_id);
        let flags = global_flags_from_matches(&matches, &default_format);
        let command_timeout = match parse_command_timeout(&flags.timeout) {
            Ok(timeout) => timeout,
            Err(err) => {
                return self.finish_run(render_cli_error(
                    &self.middleware,
                    &err,
                    &self.config.app_id,
                ));
            }
        };
        let mut middleware = self.middleware.clone();
        apply_global_flags(&mut middleware, &flags, command_timeout);
        if let Err(err) = self.apply_config_flags(&matches, &mut middleware) {
            return self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id));
        }

        let command_path = command_path_from_matches(&self.config.name, &matches);
        if command_path == "help" {
            if let Err(err) = self.run_pre_run(&mut middleware, &command_path, &help_args(&matches))
            {
                return self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id));
            }
            return self.finish_run(self.render_help_command(&matches));
        }
        if command_path == "tree" {
            if let Err(err) = self.run_pre_run(
                &mut middleware,
                &command_path,
                &crate::middleware::ValueMap::new(),
            ) {
                return self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id));
            }
            return self.finish_run(tree_render::render_tree(
                &self.root,
                &self.config.app_id,
                &middleware,
            ));
        }
        if command_path == "guide" {
            if let Err(err) =
                self.run_pre_run(&mut middleware, &command_path, &guide_args(&matches))
            {
                return self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id));
            }
            return self.finish_run(self.render_guide(&matches));
        }
        let Some(command) = self.commands.get(&command_path) else {
            if !command_path.is_empty()
                && let Some(group) = find_command_by_colon_path(&self.root, &command_path)
                && group.get_subcommands().next().is_some()
            {
                if let Err(err) = self.run_pre_run(
                    &mut middleware,
                    &command_path,
                    &crate::middleware::ValueMap::new(),
                ) {
                    return self.finish_run(render_cli_error(
                        &middleware,
                        &err,
                        &self.config.app_id,
                    ));
                }
                return self.finish_run(CliRunOutput {
                    exit_code: 0,
                    rendered: group.clone().render_long_help().to_string(),
                });
            }
            if command_path.is_empty()
                && let Some(root_next_actions) = &self.root_next_actions
            {
                if let Err(err) = self.run_pre_run(
                    &mut middleware,
                    &command_path,
                    &crate::middleware::ValueMap::new(),
                ) {
                    return self.finish_run(render_cli_error(
                        &middleware,
                        &err,
                        &self.config.app_id,
                    ));
                }
                let actions = root_next_actions();
                return self.finish_run(self.render_root(&middleware, actions));
            }
            return self.finish_run(CliRunOutput {
                exit_code: if command_path.is_empty() { 0 } else { 1 },
                rendered: if command_path.is_empty() {
                    self.root.clone().render_long_help().to_string()
                } else {
                    format!("unknown command {command_path:?}")
                },
            });
        };

        let mut middleware = match self.initialized_middleware() {
            Ok(middleware) => middleware,
            Err(err) => {
                return self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id));
            }
        };
        apply_global_flags(&mut middleware, &flags, command_timeout);
        if let Err(err) = self.apply_config_flags(&matches, &mut middleware) {
            return self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id));
        }

        let leaf = leaf_matches(&matches);
        let args = command_args_from_matches(leaf, &command.spec, false);
        let user_args = command_args_from_matches(leaf, &command.spec, true);
        if let Err(err) = self.run_pre_run(&mut middleware, &command_path, &args) {
            return self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id));
        }
        let meta = self.resolve_meta(&command_path, command.spec.metadata());
        let default_fields = command.spec.default_fields.clone().unwrap_or_default();
        let system = command.spec.system.clone().unwrap_or_default();

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
                        no_auth: command.spec.no_auth,
                    },
                    Arc::new(leaf.clone()),
                    streaming_handler,
                ),
            )
            .await;
            return self.finish_run(match result {
                Ok(output) => output,
                Err(err) => render_cli_error(&middleware, &err, &self.config.app_id),
            });
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
                    no_auth: command.spec.no_auth,
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
            Ok(output) => self.finish_run(output.into()),
            Err(err) => self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id)),
        }
    }

    fn try_run_search_bypass(&self, args: &[String]) -> Option<CliRunOutput> {
        let query = extract_search_query(args);
        if query.is_empty() {
            return None;
        }
        let scope = self.search_scope(args);
        let output_format =
            extract_output_format(args, &default_output_format(&self.config.app_id));
        Some(self.render_search(&query, &scope, &output_format))
    }

    fn try_run_schema_bypass(&self, args: &[String]) -> Option<CliRunOutput> {
        if !has_true_schema_flag(args) {
            return None;
        }
        let bool_flags = derive_bool_flags(&self.root);
        let value_flags = derive_value_flags(&self.root);
        let command_path =
            self.canonical_command_path(&extract_command_path(args, &bool_flags, &value_flags));
        let schema = self.middleware.schema_registry.get_by_path(&command_path)?;
        let output_format =
            extract_output_format(args, &default_output_format(&self.config.app_id));
        Some(self.render_schema(schema, &output_format))
    }

    fn render_schema(
        &self,
        schema: crate::output::SchemaInfo,
        output_format: &str,
    ) -> CliRunOutput {
        let format: crate::output::OutputFormat = match output_format.parse() {
            Ok(format) => format,
            Err(err) => {
                return CliRunOutput {
                    exit_code: exit_code_for_error(&err),
                    rendered: err.to_string(),
                };
            }
        };
        let envelope =
            crate::Envelope::success(schema, self.config.app_id.clone()).prepare_for_render("");
        match crate::output::render(format, &envelope) {
            Ok(rendered) => CliRunOutput {
                exit_code: 0,
                rendered,
            },
            Err(err) => CliRunOutput {
                exit_code: exit_code_for_error(&err),
                rendered: err.to_string(),
            },
        }
    }

    fn render_search(&self, query: &str, scope: &str, output_format: &str) -> CliRunOutput {
        let format: crate::output::OutputFormat = match output_format.parse() {
            Ok(format) => format,
            Err(err) => {
                return CliRunOutput {
                    exit_code: exit_code_for_error(&err),
                    rendered: err.to_string(),
                };
            }
        };
        let docs = self.search_documents(scope);
        let results = SearchIndex::new(docs).search(query, 10);
        let envelope =
            crate::Envelope::success(results, self.config.app_id.clone()).prepare_for_render("");
        match crate::output::render(format, &envelope) {
            Ok(rendered) => CliRunOutput {
                exit_code: 0,
                rendered,
            },
            Err(err) => CliRunOutput {
                exit_code: exit_code_for_error(&err),
                rendered: err.to_string(),
            },
        }
    }

    /// Renders the bare-root response. For human output, renders long help plus
    /// a "Next actions" section so a human invoking the CLI with no arguments
    /// gets readable guidance; for machine-readable output, emits a discovery
    /// envelope (light metadata + next actions). The output format has already
    /// resolved the TTY/env/flag policy, so this just branches on it.
    fn render_root(&self, middleware: &Middleware, actions: Vec<NextAction>) -> CliRunOutput {
        let format: crate::output::OutputFormat = match middleware.output_format.parse() {
            Ok(format) => format,
            Err(err) => {
                return CliRunOutput {
                    exit_code: exit_code_for_error(&err),
                    rendered: err.to_string(),
                };
            }
        };
        if format == crate::output::OutputFormat::Human {
            // Fold the suggested actions into the root long-about so they render
            // alongside the other curated sections (before Usage) instead of
            // dangling beneath clap's options dump.
            let base_long = self
                .root
                .get_long_about()
                .map(ToString::to_string)
                .unwrap_or_default();
            let long = format!("{base_long}{}", render_next_actions_human(&actions));
            let rendered = self
                .root
                .clone()
                .long_about(long)
                .render_long_help()
                .to_string();
            return CliRunOutput {
                exit_code: 0,
                rendered,
            };
        }
        let description = self
            .config
            .long
            .as_deref()
            .filter(|long| !long.is_empty())
            .unwrap_or(self.config.short.as_str());
        let data = serde_json::json!({
            "description": description,
            "version": self.config.build.version,
        });
        let envelope = crate::Envelope::success(data, self.config.app_id.clone())
            .with_next_actions(actions)
            .prepare_for_render(&middleware.verbose);
        match crate::output::render(format, &envelope) {
            Ok(rendered) => CliRunOutput {
                exit_code: 0,
                rendered,
            },
            Err(err) => CliRunOutput {
                exit_code: exit_code_for_error(&err),
                rendered: err.to_string(),
            },
        }
    }

    fn search_documents(&self, scope: &str) -> Vec<SearchDocument> {
        let (scoped, mut prefix) = find_command_and_canonical_path_by_colon_path(&self.root, scope)
            .unwrap_or((&self.root, Vec::new()));
        let mut docs = Vec::new();
        let mut aliases = Vec::new();
        append_command_alias_terms(scoped, &mut aliases);
        collect_command_search_documents(scoped, &mut prefix, &mut aliases, &mut docs);
        if scope.is_empty() {
            for entry in &self.guide_entries {
                docs.push(SearchDocument {
                    id: format!("guide:{}", entry.name),
                    kind: "guide".to_owned(),
                    title: format!("guide {}", entry.name),
                    summary: entry.summary.clone(),
                    content: format!("{} {}", entry.summary, entry.content),
                });
            }
            if let Some(extra_search_docs) = &self.extra_search_docs {
                docs.extend(extra_search_docs());
            }
        }
        docs
    }

    fn search_scope(&self, args: &[String]) -> String {
        let parts = extract_search_scope_parts(args);
        canonical_path_from_parts(&self.root, &parts).unwrap_or_default()
    }

    fn canonical_command_path(&self, command_path: &str) -> String {
        find_command_and_canonical_path_by_colon_path(&self.root, command_path).map_or_else(
            || command_path.to_owned(),
            |(_, canonical)| canonical.join(":"),
        )
    }

    fn render_guide(&self, matches: &ArgMatches) -> CliRunOutput {
        let leaf = leaf_matches(matches);
        let topic = leaf.get_one::<String>("topic").map(String::as_str);
        match guide_content(&self.guide_entries, topic) {
            Ok(rendered) => CliRunOutput {
                exit_code: 0,
                rendered,
            },
            Err(err) => CliRunOutput {
                exit_code: 1,
                rendered: err,
            },
        }
    }

    fn render_help_command(&self, matches: &ArgMatches) -> CliRunOutput {
        let leaf = leaf_matches(matches);
        let parts = leaf
            .get_many::<String>("command")
            .map(|values| values.map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        if parts.is_empty() {
            return CliRunOutput {
                exit_code: 0,
                rendered: self.root.clone().render_long_help().to_string(),
            };
        }
        let Some(command) = find_help_target(&self.root, &parts) else {
            return CliRunOutput {
                exit_code: 1,
                rendered: format!(
                    "unknown command {:?} — run '{} help' for available commands",
                    parts.join(" "),
                    self.config.name
                ),
            };
        };
        CliRunOutput {
            exit_code: 0,
            rendered: command.clone().render_long_help().to_string(),
        }
    }

    fn refresh_root_long(&mut self) {
        // Module-categorized entries, plus any visible top-level command that is
        // neither categorized nor an engine built-in, listed under a generic
        // "Commands" section. This keeps every command discoverable once clap's
        // auto subcommand list is suppressed by the root help template.
        const BUILTINS: [&str; 4] = ["help", "guide", "tree", "completion"];
        let categorized: BTreeSet<&str> = self
            .module_entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        let mut generic: Vec<ModuleHelpEntry> = self
            .root
            .get_subcommands()
            .filter(|command| !command.is_hide_set())
            .filter(|command| !BUILTINS.contains(&command.get_name()))
            .filter(|command| !categorized.contains(command.get_name()))
            .map(|command| ModuleHelpEntry {
                category: "Commands".to_owned(),
                name: command.get_name().to_owned(),
                short: command
                    .get_about()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            })
            .collect();
        generic.sort_by(|left, right| left.name.cmp(&right.name));

        let mut entries = self.module_entries.clone();
        entries.extend(generic);
        let has_guide = !self.guide_entries.is_empty() || has_subcommand(&self.root, "guide");
        let intro = self
            .config
            .long
            .as_deref()
            .filter(|long| !long.is_empty())
            .unwrap_or(self.config.short.as_str());
        self.root = self
            .root
            .clone()
            .long_about(build_root_long(intro, &entries, has_guide));
    }

    fn ensure_auth_command(&mut self) {
        let default_provider = self.default_auth_provider();
        let registered_names = self.middleware.auth.registered_names();
        if default_provider.is_empty() && registered_names.is_empty() {
            return;
        }
        let replacing_builtin = self.commands.contains_key("auth:login");
        if has_subcommand(&self.root, "auth") && !replacing_builtin {
            return;
        }
        let group = auth_command_group(&default_provider, &registered_names);
        let mut prefix = Vec::new();
        group.register_commands(&mut prefix, &mut self.commands);
        let mut prefix = Vec::new();
        let clap_group = runtime_group_clap_command_with_schema_help(
            &group,
            &mut prefix,
            &self.middleware.schema_registry,
        );
        self.root = if replacing_builtin {
            self.root.clone().mut_subcommand("auth", |_| clap_group)
        } else {
            self.root.clone().subcommand(clap_group)
        };
    }

    fn default_auth_provider(&self) -> String {
        if !self.middleware.default_auth_provider.is_empty() {
            return self.middleware.default_auth_provider.clone();
        }
        self.middleware
            .auth
            .registered_names()
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    fn initialized_middleware(&self) -> Result<Middleware> {
        let Some(init_deps) = &self.init_deps else {
            return Ok(self.middleware.clone());
        };
        let mut guard = self
            .init_state
            .lock()
            .map_err(|_| CliCoreError::message("init deps lock poisoned"))?;
        if let Some(result) = guard.as_ref() {
            return result.clone().map_err(InitFailure::into_error);
        }
        let mut middleware = self.middleware.clone();
        let result = init_deps(&mut middleware)
            .map(|()| middleware)
            .map_err(|err| InitFailure::capture(&err));
        *guard = Some(result.clone());
        result.map_err(InitFailure::into_error)
    }

    fn apply_config_flags(&self, matches: &ArgMatches, middleware: &mut Middleware) -> Result<()> {
        if let Some(apply_flags) = &self.apply_flags {
            apply_flags(matches, middleware)?;
        }
        Ok(())
    }

    fn run_pre_run(
        &self,
        middleware: &mut Middleware,
        command_path: &str,
        args: &crate::middleware::ValueMap,
    ) -> Result<()> {
        if let Some(pre_run) = &self.pre_run {
            pre_run(middleware, command_path, args)?;
        }
        Ok(())
    }

    fn resolve_meta(&self, command_path: &str, meta: CommandMeta) -> CommandMeta {
        if let Some(resolver) = &self.meta_resolver {
            resolver(command_path, meta)
        } else {
            meta
        }
    }

    fn finish_run(&self, output: CliRunOutput) -> CliRunOutput {
        if let Some(on_shutdown) = &self.on_shutdown {
            on_shutdown();
        }
        output
    }
}

fn apply_global_flags(middleware: &mut Middleware, flags: &GlobalFlags, timeout: Option<Duration>) {
    middleware.output_format = flags.output_format.clone();
    middleware.verbose = flags.verbose.clone();
    middleware.dry_run = flags.dry_run;
    middleware.fields = flags.fields.clone();
    middleware.filter = flags.filter.clone();
    middleware.expr = flags.expr.clone();
    middleware.limit = flags.limit;
    middleware.offset = flags.offset;
    middleware.reason = flags.reason.clone();
    middleware.schema = flags.schema;
    middleware.timeout = timeout;
    middleware.debug = flags.debug.clone();
    middleware.search = flags.search.clone();
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
async fn shutdown_signal() {
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
async fn shutdown_signal() {
    drop(tokio::signal::ctrl_c().await);
}

fn parse_command_timeout(raw: &str) -> Result<Option<Duration>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Some(Duration::from_secs(60)));
    }
    let Some(seconds) = parse_duration_seconds(raw) else {
        return Err(CliCoreError::message(format!(
            "invalid timeout {raw:?}: expected duration like 60s, 5m, or 0s"
        )));
    };
    if seconds <= 0.0 {
        Ok(None)
    } else {
        Ok(Some(Duration::from_secs_f64(seconds)))
    }
}

fn parse_duration_seconds(raw: &str) -> Option<f64> {
    for (suffix, seconds) in [
        ("ns", 0.000_000_001_f64),
        ("us", 0.000_001_f64),
        ("µs", 0.000_001_f64),
        ("ms", 0.001_f64),
        ("s", 1.0_f64),
        ("m", 60.0_f64),
        ("h", 3600.0_f64),
    ] {
        if let Some(number) = raw.strip_suffix(suffix) {
            let value = number.parse::<f64>().ok()?;
            if !value.is_finite() {
                return None;
            }
            return Some(value * seconds);
        }
    }
    None
}

fn render_cli_error(
    middleware: &Middleware,
    err: &(dyn std::error::Error + 'static),
    system: &str,
) -> CliRunOutput {
    let format = middleware
        .output_format
        .parse::<crate::output::OutputFormat>()
        .unwrap_or(crate::output::OutputFormat::Json);
    let envelope =
        crate::output::build_error_envelope(err, system).prepare_for_render(&middleware.verbose);
    match crate::output::render(format, &envelope) {
        Ok(rendered) => CliRunOutput {
            exit_code: exit_code_for_error(err),
            rendered,
        },
        Err(render_err) => CliRunOutput {
            exit_code: exit_code_for_error(err),
            rendered: render_err.to_string(),
        },
    }
}

fn find_command_by_colon_path<'command>(
    root: &'command Command,
    path: &str,
) -> Option<&'command Command> {
    find_command_and_canonical_path_by_colon_path(root, path).map(|(command, _)| command)
}

fn find_help_target<'command>(
    root: &'command Command,
    parts: &[&str],
) -> Option<&'command Command> {
    let mut current = root;
    let mut matched_any = false;
    for part in parts {
        let Some(next) = current.find_subcommand(part) else {
            break;
        };
        current = next;
        matched_any = true;
    }
    matched_any.then_some(current)
}

fn find_command_and_canonical_path_by_colon_path<'command>(
    root: &'command Command,
    path: &str,
) -> Option<(&'command Command, Vec<String>)> {
    if path.is_empty() {
        return Some((root, Vec::new()));
    }
    let mut current = root;
    let mut canonical = Vec::new();
    for part in path.split(':') {
        current = current.find_subcommand(part)?;
        canonical.push(current.get_name().to_owned());
    }
    Some((current, canonical))
}

fn canonical_path_from_parts(root: &Command, parts: &[String]) -> Option<String> {
    if parts.is_empty() {
        return Some(String::new());
    }
    let mut current = root;
    let mut canonical = Vec::new();
    for part in parts {
        current = current.find_subcommand(part)?;
        canonical.push(current.get_name().to_owned());
    }
    Some(canonical.join(":"))
}

fn extract_search_scope_parts(args: &[String]) -> Vec<String> {
    let mut parts = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--search" || arg.starts_with("--search=") {
            break;
        }
        if arg.starts_with('-') {
            if !arg.contains('=') && index + 1 < args.len() && !args[index + 1].starts_with('-') {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        parts.push(arg.clone());
        index += 1;
    }
    parts
}

fn collect_command_search_documents(
    command: &Command,
    prefix: &mut Vec<String>,
    aliases: &mut Vec<String>,
    docs: &mut Vec<SearchDocument>,
) {
    if command.is_hide_set() || command.get_name() == "completion" {
        return;
    }
    if command.get_subcommands().next().is_some() {
        for child in command.get_subcommands() {
            prefix.push(child.get_name().to_owned());
            let alias_len = aliases.len();
            append_command_alias_terms(child, aliases);
            collect_command_search_documents(child, prefix, aliases, docs);
            aliases.truncate(alias_len);
            prefix.pop();
        }
        return;
    }
    if prefix.is_empty() {
        prefix.push(command.get_name().to_owned());
        append_command_alias_terms(command, aliases);
    }
    let path = prefix.join(" ");
    let alias_text = aliases.join(" ");
    docs.push(SearchDocument {
        id: format!("cmd:{path}"),
        kind: "command".to_owned(),
        title: path,
        summary: command
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default(),
        content: format!(
            "{} {} {} {}",
            command
                .get_about()
                .map(ToString::to_string)
                .unwrap_or_default(),
            command
                .get_long_about()
                .map(ToString::to_string)
                .unwrap_or_default(),
            command_flag_text(command),
            alias_text
        ),
    });
    if prefix.len() == 1 && prefix[0] == command.get_name() {
        prefix.pop();
    }
}

fn append_command_alias_terms(command: &Command, aliases: &mut Vec<String>) {
    aliases.extend(command.get_all_aliases().map(str::to_owned));
    aliases.extend(
        command
            .get_all_short_flag_aliases()
            .map(|alias| alias.to_string()),
    );
    aliases.extend(command.get_all_long_flag_aliases().map(str::to_owned));
}

fn command_flag_text(command: &Command) -> String {
    command
        .get_arguments()
        .filter_map(|arg| {
            let mut names = Vec::new();
            if let Some(short) = arg.get_short() {
                names.push(format!("-{short}"));
            }
            if let Some(long) = arg.get_long() {
                names.push(format!("--{long}"));
            }
            if let Some(short_aliases) = arg.get_all_short_aliases() {
                names.extend(
                    short_aliases
                        .into_iter()
                        .map(|short_alias| format!("-{short_alias}")),
                );
            }
            if let Some(aliases) = arg.get_all_aliases() {
                names.extend(aliases.into_iter().map(|alias| format!("--{alias}")));
            }
            (!names.is_empty()).then(|| names.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn has_subcommand(command: &Command, name: &str) -> bool {
    command
        .get_subcommands()
        .any(|child| child.get_name() == name)
}

fn has_root_version_flag(args: &[String], root: &Command, root_name: &str) -> bool {
    let bool_flags = derive_bool_flags(root);
    let value_flags = derive_value_flags(root);
    let mut iter = args.iter().peekable();
    if iter
        .peek()
        .is_some_and(|arg| arg_matches_root_name(arg, root_name))
    {
        iter.next();
    }

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--version" | "-v" => return true,
            "--" => return false,
            value if value.contains('=') || bool_flags.contains(value) => continue,
            value
                if value_flags.contains(value)
                    || unknown_flag_consumes_value(value, iter.peek()) =>
            {
                iter.next();
            }
            value if value.starts_with('-') => {}
            _ => return false,
        }
    }
    false
}

fn normalize_optional_global_flags_before_command(root: &Command, args: &[String]) -> Vec<String> {
    let optional_string_defaults = BTreeMap::from([("--verbose", "all"), ("--debug", "*")]);
    let optional_bool_defaults = BTreeMap::from([("--dry-run", "true"), ("--schema", "true")]);
    let mut normalized = Vec::with_capacity(args.len());
    let mut index = 0;
    let mut current = root;
    while index < args.len() {
        let arg = &args[index];
        if index == 0 && arg_matches_root_name(arg, root.get_name()) {
            normalized.push(arg.clone());
            index += 1;
            continue;
        }

        if let Some(default) = optional_bool_defaults.get(arg.as_str()) {
            normalized.push(format!("{arg}={default}"));
            index += 1;
            continue;
        }

        if let Some(default) = optional_string_defaults.get(arg.as_str()) {
            match args.get(index + 1) {
                None => {
                    normalized.push(format!("{arg}={default}"));
                    index += 1;
                    continue;
                }
                Some(next)
                    if current.get_name() == root.get_name()
                        || next.starts_with('-')
                        || direct_subcommand(current, next).is_some() =>
                {
                    normalized.push(format!("{arg}={default}"));
                    index += 1;
                    continue;
                }
                Some(next) => {
                    normalized.push(arg.clone());
                    normalized.push(next.clone());
                    index += 2;
                    continue;
                }
            }
        }

        normalized.push(arg.clone());
        if !arg.starts_with('-')
            && let Some(next_command) = direct_subcommand(current, arg)
        {
            current = next_command;
        }
        index += 1;
    }
    normalized
}

fn direct_subcommand<'command>(
    command: &'command Command,
    token: &str,
) -> Option<&'command Command> {
    command.get_subcommands().find(|child| {
        child.get_name() == token || child.get_all_aliases().any(|alias| alias == token)
    })
}

fn unknown_group_command_message(
    root: &Command,
    args: &[String],
    root_name: &str,
) -> Option<String> {
    let bool_flags = derive_bool_flags(root);
    let value_flags = derive_value_flags(root);
    let positionals = positional_command_tokens(args, root_name, &bool_flags, &value_flags);
    if positionals.is_empty() {
        return None;
    }

    let mut current = root;
    let mut path = vec![root.get_name().to_owned()];
    for token in positionals {
        if let Some(next) = current.find_subcommand(&token) {
            current = next;
            path.push(next.get_name().to_owned());
            continue;
        }
        if current.get_subcommands().next().is_some() {
            return Some(format!(
                "unknown command {token:?} for {:?}",
                path.join(" ")
            ));
        }
        return None;
    }
    None
}

fn positional_command_tokens(
    args: &[String],
    root_name: &str,
    bool_flags: &BTreeSet<String>,
    value_flags: &BTreeSet<String>,
) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut iter = args.iter().peekable();
    if iter
        .peek()
        .is_some_and(|arg| arg_matches_root_name(arg, root_name))
    {
        iter.next();
    }

    while let Some(arg) = iter.next() {
        if arg == "--" {
            tokens.extend(iter.cloned());
            break;
        }
        if arg.contains('=') {
            continue;
        }
        if bool_flags.contains(arg) {
            continue;
        }
        if value_flags.contains(arg) || unknown_flag_consumes_value(arg, iter.peek()) {
            iter.next();
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        tokens.push(arg.clone());
    }
    tokens
}

fn unknown_flag_consumes_value(arg: &str, next: Option<&&String>) -> bool {
    arg.starts_with('-') && next.is_some_and(|value| !value.starts_with('-'))
}

fn arg_matches_root_name(arg: &str, root_name: &str) -> bool {
    arg == root_name
        || std::path::Path::new(arg)
            .file_stem()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == root_name)
}

fn register_runtime_group_schemas(
    group: &RuntimeGroupSpec,
    prefix: &mut Vec<String>,
    schemas: &mut SchemaRegistry,
) {
    prefix.push(group.group.name.clone());
    for child_group in &group.groups {
        register_runtime_group_schemas(child_group, prefix, schemas);
    }
    for child in &group.commands {
        prefix.push(child.spec.name.clone());
        register_command_schema(&child.spec, &prefix.join(":"), schemas);
        prefix.pop();
    }
    prefix.pop();
}

fn register_command_schema(spec: &CommandSpec, command_path: &str, schemas: &mut SchemaRegistry) {
    if let Some(schema) = &spec.output_schema {
        schemas.register_info(command_path.to_owned(), schema.clone());
    }
}

fn runtime_group_clap_command_with_schema_help(
    group: &RuntimeGroupSpec,
    prefix: &mut Vec<String>,
    schemas: &SchemaRegistry,
) -> Command {
    let mut command = group_clap_command_without_children(&group.group);
    prefix.push(group.group.name.clone());
    for child_group in &group.groups {
        command = command.subcommand(runtime_group_clap_command_with_schema_help(
            child_group,
            prefix,
            schemas,
        ));
    }
    for child in &group.commands {
        prefix.push(child.spec.name.clone());
        let command_path = prefix.join(":");
        command = command.subcommand(command_clap_command_with_schema_help(
            &child.spec,
            &command_path,
            schemas,
        ));
        prefix.pop();
    }
    prefix.pop();
    command
}

fn group_clap_command_without_children(group: &GroupSpec) -> Command {
    let mut command = Command::new(group.name.clone())
        .about(group.short.clone())
        .help_template(GROUP_HELP_TEMPLATE);
    if let Some(long) = &group.long
        && !long.is_empty()
    {
        command = command.long_about(long.clone());
    }
    for alias in &group.aliases {
        command = command.alias(alias.clone());
    }
    if group.hidden {
        command = command.hide(true);
    }
    command
}

fn command_clap_command_with_schema_help(
    spec: &CommandSpec,
    command_path: &str,
    schemas: &SchemaRegistry,
) -> Command {
    let mut command = spec.clap_command();
    let Some(schema) = schemas.get_by_path(command_path) else {
        return command;
    };
    let schema_help = format_help_section(&schema.fields);
    if schema_help.is_empty() {
        return command;
    }
    let base = spec
        .long
        .as_ref()
        .filter(|long| !long.is_empty())
        .cloned()
        .unwrap_or_else(|| spec.short.clone());
    let long = if base.is_empty() {
        schema_help
    } else {
        format!("{base}\n\n{schema_help}")
    };
    command = command.long_about(long);
    command
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
    middleware: &Middleware,
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
