//! Declarative CLI configuration: [`CliConfig`], [`BuildInfo`], and the
//! lifecycle-hook type aliases used to customize a [`super::Cli`].

use std::{collections::BTreeMap, sync::Arc};

use clap::{ArgMatches, Command};

use super::argv0::{Argv0Route, is_valid_argv0_name};
use crate::{
    ActivityEmitter, Auditor, AuthProvider, Authorizer, CommandMeta, GuideEntry, Middleware,
    Module, Result, RuntimeCommandSpec,
    feature_flags::{FlagPolicy, Stage},
    output::{HumanViewDef, NextAction},
    search::SearchDocument,
};

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
/// Hook that contributes extra root-scope `search` documents.
pub type ExtraSearchDocs = Arc<dyn Fn() -> Vec<SearchDocument> + Send + Sync>;
/// Hook that supplies the suggested next actions shown when the CLI is invoked
/// with no subcommand (bare root). The same actions drive a human "Next actions"
/// section and the JSON discovery envelope.
pub type RootNextActions = Arc<dyn Fn() -> Vec<NextAction> + Send + Sync>;

/// Default name for the admin help category, under which the engine files the
/// built-in `auth` command when a consumer does not override it via
/// [`CliConfig::with_admin_category`].
pub(super) const DEFAULT_ADMIN_CATEGORY: &str = "Admin";

/// Top-level subcommand names that are reserved by the engine and must not be
/// used as module group names.  [`super::Cli::add_module_group`] rejects a group whose
/// name matches a reserved name so the engine's built-in command always wins.
pub(crate) const BUILTIN_COMMAND_NAMES: [&str; 5] =
    ["help", "guide", "tree", "completion", "search"];

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
    /// Additional commands mounted as siblings of the built-in `auth`
    /// group's `login`/`status`/`logout` (e.g. `auth scopes`). Populate via
    /// [`CliConfig::with_auth_extra_commands`]; folded in internally after
    /// the built-in group is built, so the built-ins are never lost or
    /// overwritten.
    pub auth_extra_commands: Vec<RuntimeCommandSpec>,
    /// Global guide entries mounted under `guide`.
    pub guides: Vec<GuideEntry>,
    /// Global human output views.
    pub views: Vec<HumanViewDef>,
    /// Providers registered before command execution starts.
    pub auth_providers: Vec<Arc<dyn AuthProvider>>,
    /// Optional override for the process-wide outbound User-Agent. When unset,
    /// the engine derives `name/version` from this config. See
    /// [`CliConfig::user_agent_string`].
    pub user_agent: Option<String>,
    /// Extra HTTP header names to redact in `--debug transport` output, on top
    /// of the built-in sensitive set (`authorization`, `proxy-authorization`,
    /// `cookie`, `set-cookie`, `x-api-key`). Set CLI-specific secret-bearing
    /// headers here — e.g. a custom API-key header an auth injector adds.
    /// Populate via [`CliConfig::with_redacted_debug_headers`].
    pub redacted_debug_headers: Vec<String>,
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
    /// Whether to mount the built-in `config` command group (`config
    /// get`/`set`/`path`/`list`). Off by default to avoid colliding with a
    /// consumer's own `config` noun. Enable via
    /// [`CliConfig::with_config_commands`].
    pub config_commands: bool,
    /// Alternative `argv[0]` names this binary may be invoked as, mapped to the
    /// behavior the engine should take (busybox/git-style multi-call dispatch).
    ///
    /// Keyed by the bare alternative name (no path, no extension). Empty by
    /// default, in which case argv0 dispatch is inert and behavior is identical
    /// to a binary that never opted in. Populate via [`CliConfig::with_argv0_alias`]
    /// and [`CliConfig::with_argv0_personality`].
    pub argv0_routes: BTreeMap<String, Argv0Route>,
    /// Optional first-class environment system.
    ///
    /// Registered via [`CliConfig::with_environments`]. When set, the engine
    /// registers a global `--env` flag, seeds the active environment into
    /// middleware, and exposes it to handlers through
    /// [`CommandContext::environment`](crate::command::CommandContext::environment).
    pub environments: Option<Arc<crate::environments::Environments>>,
    /// Explicit argv override for [`super::Cli::new`]'s startup `--env` prescan,
    /// mainly used to make tests hermetic.
    pub startup_args: Option<Vec<std::ffi::OsString>>,
    /// Minimum feature stage required for a flagged command, group, or module
    /// to remain mounted.
    ///
    /// Defaults to [`Stage::Ga`] via [`Stage`]'s own `Default`, which combined
    /// with an empty [`feature_overrides`](Self::feature_overrides) is the
    /// zero-config behavior: nothing is gated unless a command/group/module
    /// opts in with `.with_feature_flag(...)`, and even then it stays visible
    /// until this is lowered. Lower it (e.g. to [`Stage::Beta`] or
    /// [`Stage::Experimental`]) to opt a build or environment into
    /// pre-release commands. Set via [`CliConfig::with_min_stage`].
    pub min_stage: Stage,
    /// Per-key stage overrides that substitute a forced stage for a flag
    /// key's own declared stage before comparing against
    /// [`min_stage`](Self::min_stage).
    ///
    /// Empty by default. Populate via [`CliConfig::with_feature_override`] to
    /// force one named flag to a specific effective stage — e.g. forcing a
    /// single flag to [`Stage::Ga`] to turn it on for internal testing without
    /// lowering [`min_stage`](Self::min_stage) for every other flagged
    /// command, or forcing it to [`Stage::Experimental`] to disable it even
    /// under a permissive `min_stage`. See [`FlagPolicy::visible`] for the
    /// exact comparison.
    pub feature_overrides: BTreeMap<String, Stage>,
    /// Whether to auto-enable interactive mode when a TTY is detected.
    ///
    /// When `false` (the default), commands only run interactively if the user
    /// passes `--interactive` explicitly. When `true`, the engine auto-detects
    /// a TTY (stdin + stderr) and defaults to interactive mode — meaning
    /// missing required arguments will be prompted for instead of erroring.
    ///
    /// Set via [`CliConfig::with_auto_interactive`]. Start with `false` for
    /// backwards compatibility; flip to `true` once the CLI's commands have
    /// been tested under interactive prompting.
    pub auto_interactive: bool,
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

    /// Registers a first-class environment system.
    ///
    /// When set, [`super::Cli::new`] registers a global `--env` flag, seeds the active
    /// environment into middleware (explicit `--env` > persisted active >
    /// configured default), and exposes the resolved environment to handlers via
    /// [`CommandContext::environment`](crate::command::CommandContext::environment).
    ///
    /// The [`Environments`](crate::environments::Environments) is stored as-is, so
    /// the consumer is responsible for configuring it before wrapping it in an
    /// `Arc`:
    ///
    /// - Call
    ///   [`Environments::with_app_id`](crate::environments::Environments::with_app_id)
    ///   with the **same** `app_id` passed to [`CliConfig::new`], so the config
    ///   file and active-environment persistence resolve to the application's
    ///   config directory. (An empty `app_id` makes
    ///   [`Environments::config_file_path`](crate::environments::Environments::config_file_path)
    ///   return `None`, silently disabling the `environments.toml` file layer.)
    /// - Call
    ///   [`Environments::with_config_file(true)`](crate::environments::Environments::with_config_file)
    ///   if the application loads a user-editable `environments.toml`.
    /// - **Share the same `Arc`** with any `PkceAuthProvider::with_environments`
    ///   (available with the `pkce-auth` feature):
    ///   the provider's OAuth file layer and active-environment persistence must
    ///   resolve against the identical, `app_id`-stamped instance the engine sees,
    ///   or a file-defined environment (or a file override of a compiled
    ///   environment's `client_id`) will be visible to `env info` yet invisible to
    ///   the actual OAuth login.
    #[must_use]
    pub fn with_environments(
        mut self,
        environments: Arc<crate::environments::Environments>,
    ) -> Self {
        self.environments = Some(environments);
        self
    }

    /// Overrides the argv [`super::Cli::new`] prescans for `--env` before pruning the
    /// command tree, instead of the real process argv.
    ///
    /// Only meaningful alongside [`with_environments`](Self::with_environments)
    /// — otherwise `Cli::new` never registers `--env` or does the prescan at
    /// all, so this is silently unused. Element `0` is treated as the program
    /// name and skipped, the same convention [`Cli::run`](super::Cli::run)/[`Cli::execute_from`](super::Cli::execute_from)
    /// use for their own `args` parameter.
    ///
    /// This matters beyond tests: tree pruning is decided once, at `Cli::new`
    /// time, from either this override or real process argv — never from the
    /// `args` a later [`Cli::run`](super::Cli::run)/[`Cli::execute_from`](super::Cli::execute_from) call receives. Any
    /// caller that builds the `Cli` once and later runs it with a synthetic
    /// argv (e.g. a wrapper binary invoking it programmatically, or a fixed
    /// argument list unrelated to `std::env::args_os()`) should pass the same
    /// `--env` here too, or an environment named only in the later call's
    /// argv won't have been consulted for pruning, and a flagged command that
    /// environment would reveal (or hide) can disagree with what actually
    /// dispatches. A test that configures `with_environments` should call
    /// this (even with an empty iterator) to keep construction hermetic;
    /// without it, `Cli::new` reads whatever real argv the test binary itself
    /// was invoked with.
    #[must_use]
    pub fn with_startup_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString>,
    {
        self.startup_args = Some(args.into_iter().map(Into::into).collect());
        self
    }

    /// Sets the minimum feature stage required for a flagged command, group,
    /// or module to remain mounted.
    ///
    /// See [`min_stage`](Self::min_stage) for the default and [`FlagPolicy`]
    /// for how it combines with [`feature_overrides`](Self::feature_overrides)
    /// during command-tree pruning.
    #[must_use]
    pub fn with_min_stage(mut self, stage: Stage) -> Self {
        self.min_stage = stage;
        self
    }

    /// Enables auto-interactive mode: when a TTY is detected, the CLI
    /// defaults to interactive prompting for missing required arguments.
    ///
    /// Off by default for backwards compatibility. Enable once commands have
    /// been tested under interactive prompting. `--interactive` still works as
    /// an explicit override regardless of this setting.
    #[must_use]
    pub fn with_auto_interactive(mut self, enabled: bool) -> Self {
        self.auto_interactive = enabled;
        self
    }

    /// Adds (or replaces) a per-key feature-flag stage override.
    ///
    /// See [`feature_overrides`](Self::feature_overrides) for how the
    /// override participates in the [`FlagPolicy::visible`] comparison.
    #[must_use]
    pub fn with_feature_override(mut self, key: impl Into<String>, stage: Stage) -> Self {
        self.feature_overrides.insert(key.into(), stage);
        self
    }

    /// Builds the merged [`FlagPolicy`] used for command-tree pruning from
    /// this config's `min_stage` and `feature_overrides`.
    pub(super) fn flag_policy(&self) -> FlagPolicy {
        FlagPolicy {
            min_stage: self.min_stage,
            overrides: self.feature_overrides.clone(),
        }
    }

    /// Overrides the outbound User-Agent string for all HTTP traffic.
    ///
    /// When unset, the engine derives `name/version` from this config (see
    /// [`CliConfig::user_agent_string`]). Set this when the upstream APIs expect
    /// a specific product token. The resolved value is applied process-wide on
    /// execution via [`crate::transport::set_default_user_agent`], so it reaches
    /// both command [`HttpClient`](crate::transport::HttpClient)s and the
    /// engine's own OAuth token requests.
    #[must_use]
    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = Some(user_agent.into());
        self
    }

    /// Adds HTTP header names to redact in `--debug transport` output, on top of
    /// the built-in sensitive set.
    ///
    /// Use this for CLI-specific secret-bearing headers that are not standard
    /// auth headers — for example a custom API-key header that an
    /// [`AuthInjector`](crate::transport::AuthInjector) sets. Matching is
    /// case-insensitive and additive: the built-in set is always redacted.
    /// Calls accumulate. Names are trimmed and empty entries are dropped, so a
    /// mistyped value with stray whitespace cannot silently disable redaction.
    #[must_use]
    pub fn with_redacted_debug_headers(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.redacted_debug_headers
            .extend(names.into_iter().filter_map(|name| {
                let name = name.into().trim().to_owned();
                (!name.is_empty()).then_some(name)
            }));
        self
    }

    /// Returns the outbound User-Agent string the CLI presents on HTTP requests.
    ///
    /// Resolution order:
    /// 1. an explicit [`with_user_agent`](Self::with_user_agent) override;
    /// 2. otherwise `name/version` (for example `gdx/1.2.3`);
    /// 3. otherwise just `name` when no build version is set.
    #[must_use]
    pub fn user_agent_string(&self) -> String {
        if let Some(user_agent) = &self.user_agent {
            return user_agent.clone();
        }
        if self.build.version.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.name, self.build.version)
        }
    }

    /// Adds one domain module.
    ///
    /// # Reserved group names
    ///
    /// The top-level group names `help`, `guide`, `tree`, and `completion` are
    /// reserved by the engine.  A module whose root group uses one of these
    /// names will be rejected at registration time (logged as a warning) so
    /// the engine's own built-in always takes precedence in the command tree.
    #[must_use]
    pub fn with_module(mut self, module: Module) -> Self {
        self.modules.push(module);
        self
    }

    /// Adds several domain modules.
    ///
    /// See [`with_module`](Self::with_module) for the list of reserved group names.
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

    /// Adds commands mounted as siblings of the built-in `auth` group's
    /// `login`/`status`/`logout`.
    ///
    /// Use this to extend `auth` with consumer-specific subcommands (e.g.
    /// `auth scopes`) without losing or duplicating the built-ins — unlike
    /// pre-registering an `auth` [`Module`], which either drops the built-ins
    /// entirely or has them silently overwrite any extra command added this
    /// way, these are folded in additively after building the built-in group.
    #[must_use]
    pub fn with_auth_extra_commands(
        mut self,
        commands: impl IntoIterator<Item = RuntimeCommandSpec>,
    ) -> Self {
        self.auth_extra_commands.extend(commands);
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

    /// Mounts the built-in `config` command group (`config get`/`set`/`path`/
    /// `list`) for reading and writing the per-application config file.
    ///
    /// Off by default so it never collides with a consumer's own `config` noun;
    /// the group is filed under the admin help category when enabled.
    #[must_use]
    pub fn with_config_commands(mut self) -> Self {
        self.config_commands = true;
        self
    }

    /// Registers an alternative `argv[0]` name that acts as a shortcut to a
    /// command path on this same CLI.
    ///
    /// When the binary is invoked under `name` (via symlink, hardlink, copy, or
    /// the hidden `argv0` command), the engine behaves as if the user had typed
    /// `command_path` followed by the real argument tail, routed through the
    /// normal command tree. For example:
    ///
    /// ```
    /// use cli_engine::CliConfig;
    ///
    /// // Invoking the binary as `pl --team platform` runs `project list --team platform`.
    /// let config = CliConfig::new("my-cli", "Team CLI", "my-cli")
    ///     .with_argv0_alias("pl", ["project", "list"]);
    /// ```
    ///
    /// `name` must be a simple token: non-empty and composed only of ASCII
    /// letters, digits, `-`, or `_` (no dots, spaces, path separators, or shell
    /// metacharacters), and it must differ from the CLI's own name. These are
    /// debug-asserted. The restriction keeps the name usable as a link/shim
    /// filename and an `argv[0]` basename (which is matched with its extension
    /// stripped, so a dot would break matching).
    #[must_use]
    pub fn with_argv0_alias(
        mut self,
        name: impl Into<String>,
        command_path: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let name = name.into();
        debug_assert!(
            is_valid_argv0_name(&name),
            "argv0 route name {name:?} must be non-empty and contain only ASCII letters, digits, '-', or '_'"
        );
        debug_assert!(
            name != self.name,
            "argv0 route name {name:?} must differ from the CLI's own name {:?}",
            self.name
        );
        let tokens = command_path.into_iter().map(Into::into).collect();
        self.argv0_routes.insert(name, Argv0Route::Alias(tokens));
        self
    }

    /// Registers an alternative `argv[0]` name that runs an entirely separate CLI
    /// application.
    ///
    /// When the binary is invoked under `name`, the engine builds a fresh
    /// [`CliConfig`] from `build` and runs that application instead — its own root
    /// name, commands, flags, and auth. The closure runs lazily, only when the
    /// route is dispatched, so unused personalities cost nothing. The personality
    /// presents the name from its own [`CliConfig`] in help and usage output.
    ///
    /// ```
    /// use cli_engine::CliConfig;
    ///
    /// let config = CliConfig::new("my-cli", "Team CLI", "my-cli")
    ///     .with_argv0_personality("legacy-tool", || {
    ///         CliConfig::new("legacy-tool", "Legacy compatibility shim", "legacy-tool")
    ///     });
    /// ```
    ///
    /// `name` follows the same contract as [`CliConfig::with_argv0_alias`]: a
    /// simple `[A-Za-z0-9_-]` token, differing from the CLI's own name
    /// (debug-asserted).
    #[must_use]
    pub fn with_argv0_personality(
        mut self,
        name: impl Into<String>,
        build: impl Fn() -> CliConfig + Send + Sync + 'static,
    ) -> Self {
        let name = name.into();
        debug_assert!(
            is_valid_argv0_name(&name),
            "argv0 route name {name:?} must be non-empty and contain only ASCII letters, digits, '-', or '_'"
        );
        debug_assert!(
            name != self.name,
            "argv0 route name {name:?} must differ from the CLI's own name {:?}",
            self.name
        );
        self.argv0_routes
            .insert(name, Argv0Route::Personality(Arc::new(build)));
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
            .field(
                "argv0_routes",
                &self.argv0_routes.keys().collect::<Vec<_>>(),
            )
            .field("min_stage", &self.min_stage)
            .field("feature_overrides", &self.feature_overrides)
            .finish()
    }
}
