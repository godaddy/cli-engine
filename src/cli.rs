use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    time::Duration,
};

mod builtins;
mod completion;
mod help;
mod tree_render;

use clap::{ArgMatches, Command};

use crate::{
    ActivityEmitter, Auditor, AuthProvider, Authorizer, CliCoreError, CommandMeta, CommandSpec,
    FeatureFlag, GroupSpec, GuideEntry, Middleware, MiddlewareRequest, Result, RuntimeCommandSpec,
    RuntimeGroupSpec,
    auth::commands::auth_command_group,
    command::{
        CommandContext, StreamSender, command_args_from_matches, command_path_from_matches,
        leaf_matches,
    },
    error::exit_code_for_error,
    feature_flags::{FlagEntry, FlagPolicy, FlagRegistry, Stage},
    flags::{
        GlobalFlags, derive_bool_flags, derive_value_flags, extract_command_path,
        extract_output_format, extract_search_query, global_flags_from_matches,
        has_true_schema_flag, output_env_var, register_global_flags, register_reason_flag,
        resolve_default_output_format,
    },
    guide::{guide_content, render_guide_human},
    module::{Module, ModuleContext},
    output::{
        HumanViewDef, HumanViewRegistry, NextAction, SchemaRegistry, format_help_section,
        global_human_view_registry_snapshot, global_schema_registry_snapshot,
    },
    search::{SearchDocument, SearchIndex},
};

use builtins::{
    completion_args, completion_command, guide_args, guide_command, help_args, help_command,
};
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

/// Maximum number of chained `argv0` dispatch hand-offs before the engine
/// refuses to recurse further. Real multi-call nesting is zero or one level;
/// this bounds a pathologically long explicit `argv0 … argv0 …` chain so it
/// errors cleanly instead of overflowing the stack.
const MAX_ARGV0_DEPTH: usize = 16;

/// How the engine behaves when invoked under a registered alternative `argv[0]`
/// name (busybox/git-style multi-call dispatch).
///
/// A route is selected when the binary's `argv[0]` basename — or the name given
/// to the hidden `argv0` command — matches a key registered via
/// [`CliConfig::with_argv0_alias`] or [`CliConfig::with_argv0_personality`]. An
/// `argv[0]` that matches no route falls through to the default CLI, so existing
/// applications that register no routes are unaffected.
///
/// Non-exhaustive: more route kinds may be added in future releases. Register
/// routes through the [`CliConfig`] builders rather than matching on variants.
#[derive(Clone)]
#[non_exhaustive]
pub enum Argv0Route {
    /// Rewrite the invocation into these canonical subcommand tokens and run it
    /// through the normal command tree, with the real argument tail appended.
    ///
    /// For example, an `Alias(vec!["project".into(), "list".into()])` registered
    /// under `pl` makes `pl --team x` behave exactly like `project list --team x`.
    Alias(Vec<String>),
    /// Run an entirely separate CLI application built from the returned
    /// [`CliConfig`] (its own root name, commands, flags, and auth). The
    /// configuration is built lazily, only when the route is actually dispatched,
    /// so registering a personality costs nothing for invocations that never hit it.
    Personality(Arc<dyn Fn() -> CliConfig + Send + Sync>),
}

impl std::fmt::Debug for Argv0Route {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alias(tokens) => formatter.debug_tuple("Alias").field(tokens).finish(),
            Self::Personality(_) => formatter.write_str("Personality(..)"),
        }
    }
}

/// On-disk mechanism used by [`Cli::create_link`] to materialize an alternative
/// `argv[0]` name so the binary can be invoked under it.
///
/// Installers pick the mechanism that suits the platform and environment;
/// self-healing code can re-run [`Cli::create_link`] to restore a deleted link.
///
/// Non-exhaustive: more link mechanisms may be added in future releases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Argv0LinkMethod {
    /// A symbolic link to the target executable (`<name>` on Unix, `<name>.exe`
    /// on Windows). On Windows this may require Developer Mode or elevation.
    SoftLink,
    /// A hard link to the target executable (`<name>` on Unix, `<name>.exe` on
    /// Windows). The link must live on the same volume as the target.
    HardLink,
    /// A small shim script that forwards to the target via the `argv0` command:
    /// a `<name>.cmd` batch file on Windows, or an executable `<name>` shell
    /// script on Unix. Useful when links are unavailable or inconvenient.
    Script,
}

/// Top-level subcommand names that are reserved by the engine and must not be
/// used as module group names.  [`Cli::add_module_group`] rejects a group whose
/// name matches a reserved name so the engine's built-in command always wins.
pub(crate) const BUILTIN_COMMAND_NAMES: [&str; 4] = ["help", "guide", "tree", "completion"];

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
    /// When set, [`Cli::new`] registers a global `--env` flag, seeds the active
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
    fn flag_policy(&self) -> FlagPolicy {
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
            .subcommand(Command::new("tree").about("Display full command tree"))
            .subcommand(completion_command());
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
                clap::Arg::new("env")
                    .long("env")
                    .global(true)
                    .value_name("ENV")
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
            // Seed the sticky/default active environment now; the global `--env`
            // flag overrides it per invocation in `run_with_depth`. The same
            // `Arc` the consumer shared with any `PkceAuthProvider` is reused, so
            // the file layer and active-env persistence resolve consistently.
            middleware.env = environments.effective_active(None, &middleware.config);
            middleware.environments = Some(Arc::clone(environments));
        }
        // Seed the merged flag policy before any module/group is registered so
        // pruning during `add_module`/`add_module_group` below sees it. The active
        // environment's fully-resolved (compiled + file + env-var) min_stage/
        // feature_overrides, when present, take precedence over the consumer-level
        // CliConfig policy — an environment can loosen or tighten visibility beyond
        // what the binary set in code. Environment resolution failure (e.g. an
        // unknown active environment name) is tolerated here and falls back to the
        // consumer-level policy only: this is just flag-policy computation, not full
        // environment validation, and must not make `Cli::new` fail in a new way.
        // The normal lazy paths (`env get`/`env info`, `ctx.environment()?`) still
        // surface a real resolution error to the user when a command needs it.
        let mut flag_policy = config.flag_policy();
        if let Some(environments) = &middleware.environments
            && let Ok(env) = environments.resolve(&middleware.env)
        {
            if let Some(min_stage) = env.min_stage {
                flag_policy.min_stage = min_stage;
            }
            flag_policy.overrides.extend(env.feature_overrides);
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
        if cli.config.config_commands {
            cli.ensure_config_command();
        }
        if cli.config.environments.is_some() {
            cli.ensure_env_command();
        }
        cli.ensure_flags_command();
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
        self.install_default_user_agent();
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
        self.add_module_group_inner(category, group, None)
    }

    /// Shared implementation behind [`add_module_group`](Self::add_module_group)
    /// and [`add_module`](Self::add_module). `inherited` is the effective
    /// feature flag the group's enclosing module declared (if any), so a
    /// module-level flag cascades down to the group even though
    /// `add_module_group` itself has no concept of a module.
    fn add_module_group_inner(
        &mut self,
        category: impl Into<String>,
        group: RuntimeGroupSpec,
        inherited: Option<FeatureFlag>,
    ) -> &mut Self {
        // Prevent consumer modules from shadowing engine built-ins in the clap
        // command tree.  A reserved group name would override the engine's own
        // subcommand (last-writer-wins in clap) and corrupt the dispatch path.
        if BUILTIN_COMMAND_NAMES.contains(&group.group.name.as_str()) {
            tracing::warn!(
                name = %group.group.name,
                "module group name is reserved by cli-engine built-ins; the group will not be registered"
            );
            return self;
        }

        let mut prefix = Vec::new();
        let Some(group) = prune_feature_flag_tree(
            group,
            inherited.as_ref(),
            &self.middleware.flag_policy,
            &mut prefix,
            &mut self.middleware.flag_registry,
        ) else {
            return self;
        };

        let category = category.into();
        if !group.group.hidden {
            self.module_entries.push(ModuleHelpEntry {
                category,
                name: group.group.name.clone(),
                short: group.group.short.clone(),
            });
        }

        let mut prefix = Vec::new();
        register_runtime_group_metadata(
            &group,
            &mut prefix,
            &mut self.middleware.schema_registry,
            &mut self.middleware.human_views,
        );
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
        self.add_module_group_inner(module.category, group, module.feature_flag.clone())
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

    /// Resolves busybox/git-style `argv[0]` dispatch before the normal pipeline.
    ///
    /// Returns [`Argv0Outcome::Proceed`] with the (possibly rewritten) argument
    /// vector to feed the normal command pipeline, or [`Argv0Outcome::Handled`]
    /// with a fully rendered result when a personality ran or an explicit `argv0`
    /// invocation was rejected. When no routes are registered this is inert and
    /// returns the arguments unchanged. `depth` counts chained hand-offs and
    /// bounds recursion via [`MAX_ARGV0_DEPTH`].
    async fn resolve_argv0(&self, text_args: Vec<String>, depth: usize) -> Argv0Outcome {
        if self.config.argv0_routes.is_empty() {
            return Argv0Outcome::Proceed(text_args);
        }

        if depth > MAX_ARGV0_DEPTH {
            return Argv0Outcome::Handled(
                self.render_argv0_error(&text_args, "argv0 dispatch recursion limit exceeded"),
            );
        }

        // The hidden `argv0` meta-command (`<bin> argv0 <name> [args...]`) forces
        // a route without an actual symlink. It is recognized positionally as the
        // first argument after the program name and is never registered with clap,
        // so it stays absent from `--help`, `tree`, and `--search`.
        let explicit = text_args.get(1).map(String::as_str) == Some("argv0");
        let (name, rest) = if explicit {
            match text_args.get(2) {
                None => {
                    return Argv0Outcome::Handled(self.render_argv0_error(
                        &text_args,
                        "the argv0 command requires a name to dispatch as",
                    ));
                }
                // Normalize the explicit name the same way as a symlink basename
                // so a route registered as `whatever` matches whether the caller
                // passed `whatever`, `whatever.exe`, or a `.cmd` shim's `whatever.cmd`.
                Some(name) => (
                    program_basename(name),
                    text_args
                        .get(3..)
                        .map(<[String]>::to_vec)
                        .unwrap_or_default(),
                ),
            }
        } else {
            let name = text_args
                .first()
                .map(|arg| program_basename(arg))
                .unwrap_or_default();
            let rest = text_args
                .get(1..)
                .map(<[String]>::to_vec)
                .unwrap_or_default();
            (name, rest)
        };

        match self.config.argv0_routes.get(&name) {
            Some(Argv0Route::Alias(tokens)) => {
                // Rewrite as `<canonical-name> <tokens...> <rest...>`. Element 0 is
                // the canonical name so the downstream program-name skip applies.
                let mut rewritten = Vec::with_capacity(1 + tokens.len() + rest.len());
                rewritten.push(self.config.name.clone());
                rewritten.extend(tokens.iter().cloned());
                rewritten.extend(rest);
                Argv0Outcome::Proceed(rewritten)
            }
            Some(Argv0Route::Personality(build)) => {
                // Hand off to an independent CLI built lazily from the route. Its
                // own config name leads so its help/usage and program-name skip
                // render correctly. `Box::pin` breaks the recursive `async fn`;
                // `depth + 1` bounds a pathological chain of hand-offs.
                let config = build();
                let bin = config.name.clone();
                let alt = Self::new(config);
                let mut alt_args = Vec::with_capacity(1 + rest.len());
                alt_args.push(bin);
                alt_args.extend(rest);
                Argv0Outcome::Handled(Box::pin(alt.run_with_depth(alt_args, depth + 1)).await)
            }
            None if explicit => Argv0Outcome::Handled(self.render_argv0_error(
                &text_args,
                format!(
                    "{name:?} is not a registered argv0 name; known names: {}",
                    self.known_argv0_names()
                ),
            )),
            None => {
                // Unregistered name (e.g. the binary renamed to something we do not
                // recognize): fall through to the default CLI. Normalizing element 0
                // to the canonical name lets a renamed binary parse as the default
                // application instead of treating its name as a command token.
                let mut rewritten = Vec::with_capacity(1 + rest.len());
                rewritten.push(self.config.name.clone());
                rewritten.extend(rest);
                Argv0Outcome::Proceed(rewritten)
            }
        }
    }

    /// Computes the default output format for this run — the fallback used
    /// when no explicit `--output`/`--json`/`--human`/`--toon` is given.
    fn resolve_run_output_format(&self) -> String {
        use std::io::IsTerminal;

        let env = std::env::var(output_env_var(&self.config.app_id)).ok();
        let engine_config = self.middleware.config.engine();
        resolve_default_output_format(
            env.as_deref(),
            engine_config.output.format.as_deref(),
            std::io::stdout().is_terminal(),
        )
    }

    /// Comma-separated, sorted list of registered alternative `argv[0]` names,
    /// used in the error shown for an unknown explicit `argv0` invocation.
    fn known_argv0_names(&self) -> String {
        self.config
            .argv0_routes
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Renders an `argv0`-dispatch error through the engine's structured error
    /// envelope so it honors `--output` (parsed from the raw args, since dispatch
    /// runs before clap) and the shared exit-code mapping, matching every other
    /// CLI error rather than emitting bare text.
    fn render_argv0_error(&self, text_args: &[String], message: impl Into<String>) -> CliRunOutput {
        let mut middleware = self.middleware.clone();
        middleware.output_format =
            extract_output_format(text_args, &self.resolve_run_output_format());
        let err = CliCoreError::message(message);
        self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id))
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
        if !self.config.argv0_routes.contains_key(name) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{name:?} is not a registered argv0 name"),
            ));
        }

        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let link = dir.join(argv0_link_file_name(name, method));

        // Resolve the target up front so an existing entry can be compared against it.
        let resolved_target;
        let target = match target {
            Some(target) => target,
            None => {
                resolved_target = std::env::current_exe()?;
                resolved_target.as_path()
            }
        };

        // Ensure-desired-state. `symlink_metadata` does not follow links, so a
        // present-but-dangling link still counts as existing. A matching entry is
        // left untouched (idempotent); a differing one is removed and recreated.
        if std::fs::symlink_metadata(&link).is_ok() {
            if argv0_link_matches(&link, target, name, method)? {
                return Ok(link);
            }
            std::fs::remove_file(&link)?;
        }

        match method {
            Argv0LinkMethod::SoftLink => create_symlink(target, &link)?,
            Argv0LinkMethod::HardLink => std::fs::hard_link(target, &link)?,
            Argv0LinkMethod::Script => {
                std::fs::write(&link, argv0_script_contents(target, name))?;
                make_executable(&link)?;
            }
        }
        Ok(link)
    }

    /// Runs the CLI with provided args and captures the rendered result.
    pub async fn run<I, S>(&self, args: I) -> CliRunOutput
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString> + Clone,
    {
        self.run_with_depth(args, 0).await
    }

    /// Runs the CLI like [`Cli::run`], threading the `argv0` dispatch recursion
    /// `depth` so a chain of personality hand-offs is bounded by [`MAX_ARGV0_DEPTH`].
    async fn run_with_depth<I, S>(&self, args: I, depth: usize) -> CliRunOutput
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
        let text_args = match self.resolve_argv0(text_args, depth).await {
            Argv0Outcome::Handled(output) => return output,
            Argv0Outcome::Proceed(args) => args,
        };
        let mut clap_args = normalize_optional_global_flags_before_command(&self.root, &text_args);
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
        // Resolve the positional command path once and share it between the
        // group-help rewrite and the unknown-command check below.
        let bool_flags = derive_bool_flags(&self.root);
        let value_flags = derive_value_flags(&self.root);
        let positionals =
            positional_command_tokens(&text_args, &self.config.name, &bool_flags, &value_flags);
        // Positional tokens after a `--` separator are literal operands, not
        // command keywords, so the group-help shim must not treat a `help`
        // among them as a help request. Count the positionals that precede any
        // `--` to mark where genuine command keywords end.
        let command_keyword_count = match text_args.iter().position(|arg| arg == "--") {
            Some(end) => positional_command_tokens(
                &text_args[..end],
                &self.config.name,
                &bool_flags,
                &value_flags,
            )
            .len(),
            None => positionals.len(),
        };
        if let Some(parts) =
            group_help_target_parts(&self.root, &positionals, command_keyword_count)
        {
            // Rewrite `<group> help [sub...]` into the canonical
            // `help <group> [sub...]` so it flows through the curated root
            // `help` command, which also runs global-flag parsing and the
            // `pre_run` hook (matching `help <group>` and bare-group help).
            // Only the positional command tokens are reordered; every flag and
            // its value is preserved in place so e.g. `--output json` survives.
            clap_args = rewrite_group_help_args(
                &clap_args,
                &self.config.name,
                &bool_flags,
                &value_flags,
                &parts,
            );
        } else if let Some(message) = unknown_group_command_message(&self.root, &positionals) {
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

        let default_format = self.resolve_run_output_format();
        let flags = global_flags_from_matches(&matches, &default_format);
        // Publish the --credential-store override so auth providers resolving
        // their storage backend see it at the top of the precedence chain.
        crate::config::set_credential_store_flag(flags.credential_store);
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
        install_debug_transport_logger(&flags.debug, &self.config.redacted_debug_headers);
        if let Err(err) = self.apply_config_flags(&matches, &mut middleware) {
            return self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id));
        }
        // Validate and apply `--env` for built-in paths (help/tree/guide/group
        // help) so they reflect the selected environment and reject unknowns.
        if let Err(err) = self.apply_env_flag(&matches, &mut middleware) {
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
            return self.finish_run(self.render_guide(&matches, &flags.output_format));
        }
        if command_path == "completion" {
            let args = completion_args(&matches);
            if let Err(err) = self.run_pre_run(&mut middleware, &command_path, &args) {
                return self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id));
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
                            return self.finish_run(render_cli_error(
                                &middleware,
                                &e,
                                &self.config.app_id,
                            ));
                        }
                    },
                    None => match detect_shell() {
                        Ok(s) => s,
                        Err(e) => {
                            return self.finish_run(render_cli_error(
                                &middleware,
                                &e,
                                &self.config.app_id,
                            ));
                        }
                    },
                };
                return self.finish_run(
                    completion::install(&self.root, &self.config.name, shell)
                        .await
                        .unwrap_or_else(|e| render_cli_error(&middleware, &e, &self.config.app_id)),
                );
            }
            return self.finish_run(self.render_completion_print(shell_opt, &middleware));
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
                // Bare-root discovery is static (help text / metadata + action
                // pointers) and must always be available as a cold-start entry
                // point, so we skip `pre_run` here — matching the no-hook
                // bare-root path below, which also renders help without it.
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
        install_debug_transport_logger(&flags.debug, &self.config.redacted_debug_headers);
        if let Err(err) = self.apply_config_flags(&matches, &mut middleware) {
            return self.finish_run(render_cli_error(&middleware, &err, &self.config.app_id));
        }
        // The global `--env` flag overrides the seeded active environment for
        // this invocation; an unknown name surfaces as an error envelope.
        if let Err(err) = self.apply_env_flag(&matches, &mut middleware) {
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
                    view_id: view_id.as_deref(),
                    auth: command.spec.auth,
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
        let output_format = extract_output_format(args, &self.resolve_run_output_format());
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
        // `--schema` is an inspection flag and must not require the command's own
        // arguments, so it short-circuits before clap validates them. Only fire
        // for a real leaf command, though: unknown paths and groups fall through
        // so clap and `unknown_group_command_message` can report them as usual.
        let command = find_command_by_colon_path(&self.root, &command_path)?;
        if command.get_subcommands().next().is_some() {
            return None;
        }
        let output_format = extract_output_format(args, &self.resolve_run_output_format());
        // When no schema is registered, report that rather than running the
        // command — matching the middleware's no-schema response so the public
        // path and the lower layer agree even when required args are missing.
        match self.middleware.schema_registry.get_by_path(&command_path) {
            Some(schema) => Some(self.render_schema(schema, &output_format)),
            None => Some(self.render_schema(
                crate::output::no_schema_response(&command_path),
                &output_format,
            )),
        }
    }

    fn render_schema(&self, data: impl serde::Serialize, output_format: &str) -> CliRunOutput {
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
            crate::Envelope::success(data, self.config.app_id.clone()).prepare_for_render("");
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
        // Reject an invalid explicit `--output` here too, matching the normal
        // command path (`Middleware::render_envelope`). `OutputFormat::from_str`
        // is infallible and would otherwise silently coerce an unrecognized
        // value (e.g. `--output yaml`) to JSON instead of reporting the error.
        if !crate::output::is_valid_output_format(&middleware.output_format) {
            let err = CliCoreError::InvalidOutputFormat(middleware.output_format.clone());
            return CliRunOutput {
                exit_code: exit_code_for_error(&err),
                rendered: err.to_string(),
            };
        }
        let format = middleware
            .output_format
            .parse()
            .unwrap_or(crate::output::OutputFormat::Json);
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

    fn render_guide(&self, matches: &ArgMatches, output_format: &str) -> CliRunOutput {
        use std::io::IsTerminal;

        // Reject an invalid explicit `--output` here too, matching the normal
        // command path and `render_root`; otherwise an unrecognized value (e.g.
        // `--output yaml`) would silently fall through and emit raw content.
        if !crate::output::is_valid_output_format(output_format) {
            let err = CliCoreError::InvalidOutputFormat(output_format.to_owned());
            return CliRunOutput {
                exit_code: exit_code_for_error(&err),
                rendered: err.to_string(),
            };
        }

        let leaf = leaf_matches(matches);
        let topic = leaf.get_one::<String>("topic").map(String::as_str);
        match guide_content(&self.guide_entries, topic) {
            Ok(rendered) => {
                // Only reflow an actual guide topic body, and only for human output.
                // The topic list is plain text (not markdown) and json/toon keep the
                // raw markdown so their output stays deterministic.
                let rendered = if topic.is_some() && output_format == "human" {
                    let is_tty = std::io::stdout().is_terminal();
                    render_guide_human(&rendered, crate::output::terminal_width(), is_tty)
                } else {
                    rendered
                };
                CliRunOutput {
                    exit_code: 0,
                    rendered,
                }
            }
            Err(err) => CliRunOutput {
                exit_code: 1,
                rendered: err,
            },
        }
    }

    fn render_completion_print(
        &self,
        shell_opt: Option<String>,
        middleware: &Middleware,
    ) -> CliRunOutput {
        use crate::cli::completion::{detect_shell, generate_script, parse_shell};
        let shell = match shell_opt {
            Some(s) => match parse_shell(&s) {
                Ok(s) => s,
                Err(e) => return render_cli_error(middleware, &e, &self.config.app_id),
            },
            None => match detect_shell() {
                Ok(s) => s,
                Err(e) => return render_cli_error(middleware, &e, &self.config.app_id),
            },
        };
        match generate_script(&self.root, &self.config.name, shell) {
            Ok(script) => CliRunOutput {
                exit_code: 0,
                rendered: script,
            },
            Err(e) => render_cli_error(middleware, &e, &self.config.app_id),
        }
    }

    fn render_help_command(&self, matches: &ArgMatches) -> CliRunOutput {
        let leaf = leaf_matches(matches);
        let parts = leaf
            .get_many::<String>("command")
            .map(|values| values.map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        self.render_help_for_parts(&parts)
    }

    /// Renders the curated help text for a resolved command path.
    ///
    /// Empty `parts` render the root help. A path that resolves to a group or
    /// command renders that command's long help; an unresolved path returns the
    /// standard "unknown command" guidance with a non-zero exit code. Shared by
    /// the root `help <path>` command and the `<group> help` subcommand form.
    fn render_help_for_parts(&self, parts: &[&str]) -> CliRunOutput {
        if parts.is_empty() {
            return CliRunOutput {
                exit_code: 0,
                rendered: self.root.clone().render_long_help().to_string(),
            };
        }
        let Some(command) = find_help_target(&self.root, parts) else {
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
        let builtins = BUILTIN_COMMAND_NAMES;
        let categorized: BTreeSet<&str> = self
            .module_entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        let mut generic: Vec<ModuleHelpEntry> = self
            .root
            .get_subcommands()
            .filter(|command| !command.is_hide_set())
            .filter(|command| !builtins.contains(&command.get_name()))
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
        let mut group = auth_command_group(&default_provider, &registered_names);
        let mut seen_names: std::collections::HashSet<String> =
            group.commands.iter().map(|c| c.spec.name.clone()).collect();
        for extra in self.config.auth_extra_commands.clone() {
            if !seen_names.insert(extra.spec.name.clone()) {
                tracing::warn!(
                    command = %extra.spec.name,
                    "auth_extra_commands entry collides with a built-in auth subcommand or an \
                     earlier auth_extra_commands entry; ignoring"
                );
                continue;
            }
            group = group.with_command(extra);
        }
        let mut prefix = Vec::new();
        register_runtime_group_metadata(
            &group,
            &mut prefix,
            &mut self.middleware.schema_registry,
            &mut self.middleware.human_views,
        );
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
        // Categorize `auth` wherever it is ensured (construction or a later
        // `register_auth_provider`), so it never falls into the generic
        // "Commands" bucket. Idempotent via the `already_listed` guard.
        self.register_auth_help_entry();
    }

    /// Mounts the built-in `config` command group and files it under the admin
    /// help category. Idempotent and yields to a consumer-defined `config`
    /// subcommand if one already exists.
    fn ensure_config_command(&mut self) {
        if has_subcommand(&self.root, "config") {
            return;
        }
        let group = crate::config_commands::config_command_group();
        let mut prefix = Vec::new();
        group.register_commands(&mut prefix, &mut self.commands);
        let mut prefix = Vec::new();
        let clap_group = runtime_group_clap_command_with_schema_help(
            &group,
            &mut prefix,
            &self.middleware.schema_registry,
        );
        self.root = self.root.clone().subcommand(clap_group);
        let category = self
            .config
            .admin_category
            .clone()
            .unwrap_or_else(|| DEFAULT_ADMIN_CATEGORY.to_owned());
        if !self
            .module_entries
            .iter()
            .any(|entry| entry.name == "config")
        {
            self.module_entries.push(ModuleHelpEntry {
                category,
                name: "config".to_owned(),
                short: "Read and write the CLI config file".to_owned(),
            });
        }
        self.refresh_root_long();
    }

    /// Mounts the built-in `env` command group and files it under the admin
    /// help category. Idempotent and yields to a consumer-defined `env`
    /// subcommand if one already exists.
    fn ensure_env_command(&mut self) {
        if has_subcommand(&self.root, "env") {
            return;
        }
        let group = crate::env_commands::env_command_group();
        let mut prefix = Vec::new();
        group.register_commands(&mut prefix, &mut self.commands);
        let mut prefix = Vec::new();
        let clap_group = runtime_group_clap_command_with_schema_help(
            &group,
            &mut prefix,
            &self.middleware.schema_registry,
        );
        self.root = self.root.clone().subcommand(clap_group);
        let category = self
            .config
            .admin_category
            .clone()
            .unwrap_or_else(|| DEFAULT_ADMIN_CATEGORY.to_owned());
        if !self.module_entries.iter().any(|e| e.name == "env") {
            self.module_entries.push(ModuleHelpEntry {
                category,
                name: "env".to_owned(),
                short: "Manage the active environment".to_owned(),
            });
        }
        self.refresh_root_long();
    }

    /// Mounts the built-in `flags` command group and files it under the admin
    /// help category. Idempotent and yields to a consumer-defined `flags`
    /// subcommand if one already exists. Unlike [`Self::ensure_env_command`],
    /// this is mounted unconditionally: feature-flag introspection does not
    /// depend on any opt-in system, so it is always available.
    fn ensure_flags_command(&mut self) {
        if has_subcommand(&self.root, "flags") {
            return;
        }
        let group = crate::flag_commands::flags_command_group();
        let mut prefix = Vec::new();
        group.register_commands(&mut prefix, &mut self.commands);
        let mut prefix = Vec::new();
        let clap_group = runtime_group_clap_command_with_schema_help(
            &group,
            &mut prefix,
            &self.middleware.schema_registry,
        );
        self.root = self.root.clone().subcommand(clap_group);
        let category = self
            .config
            .admin_category
            .clone()
            .unwrap_or_else(|| DEFAULT_ADMIN_CATEGORY.to_owned());
        if !self.module_entries.iter().any(|e| e.name == "flags") {
            self.module_entries.push(ModuleHelpEntry {
                category,
                name: "flags".to_owned(),
                short: "Inspect declared feature flags".to_owned(),
            });
        }
        self.refresh_root_long();
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

    /// Applies the global `--env` override to a per-run middleware snapshot.
    ///
    /// The flag is only registered when environments are configured, so when it
    /// is present `middleware.environments` is set too. Validates the requested
    /// name against the registered environments and updates `middleware.env`,
    /// returning an error for an unknown environment.
    fn apply_env_flag(&self, matches: &ArgMatches, middleware: &mut Middleware) -> Result<()> {
        // Guard on the environment system FIRST. The `--env` arg is only
        // registered when environments are configured (the same condition that
        // sets `middleware.environments`); calling `matches.get_one("env")` for
        // an arg that was never registered panics in clap, which would break
        // every CLI that does not use environments.
        let Some(environments) = middleware.environments.as_ref() else {
            return Ok(());
        };
        if let Some(env) = matches.get_one::<String>("env") {
            environments.resolve(env)?;
            middleware.env = env.clone();
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
        // Clear the per-thread credential-store flag so it does not leak into
        // subsequent sequential runs on the same thread.
        crate::config::clear_credential_store_flag();
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

/// Builds the transport debug logger implied by a parsed `--debug` pattern,
/// without publishing it anywhere.
///
/// Pure so tests can assert on the decision (`--debug` pattern -> enabled or
/// not) without touching the process-wide default logger, which every
/// [`Cli::run`] call republishes — including the many unrelated tests that
/// exercise `cli.run(...)` with no `--debug` flag and would otherwise race
/// with an assertion on the shared global.
fn debug_transport_logger_for(
    debug: &str,
    extra_redacted: &[String],
) -> Arc<dyn crate::transport::TransportLogger> {
    if crate::debug_component_enabled(debug, "transport") {
        Arc::new(
            crate::transport::StderrTransportLogger::new()
                .with_redacted_headers(extra_redacted.iter().cloned()),
        )
    } else {
        Arc::new(crate::transport::NoopTransportLogger)
    }
}

/// Installs (or clears) the process-wide transport debug logger from the parsed
/// `--debug` pattern.
///
/// When `--debug` selects the `transport` component the engine publishes a
/// [`StderrTransportLogger`](crate::transport::StderrTransportLogger) — extended
/// with any [`CliConfig::with_redacted_debug_headers`] entries — which every
/// [`HttpClient`](crate::transport::HttpClient) built afterward picks up
/// automatically, with no per-command wiring. The logger is reset to a noop when
/// `transport` is not selected so the explicit setting always reflects the
/// current invocation rather than a stale process-global from an earlier one.
fn install_debug_transport_logger(debug: &str, extra_redacted: &[String]) {
    crate::transport::set_default_transport_logger(debug_transport_logger_for(
        debug,
        extra_redacted,
    ));
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
    if command.is_hide_set() || BUILTIN_COMMAND_NAMES.contains(&command.get_name()) {
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

fn unknown_group_command_message(root: &Command, positionals: &[String]) -> Option<String> {
    if positionals.is_empty() {
        return None;
    }

    let mut current = root;
    let mut path = vec![root.get_name().to_owned()];
    for token in positionals {
        if let Some(next) = current.find_subcommand(token) {
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

/// Detects the `<group> help [sub...]` form and returns the command path whose
/// help should be rendered.
///
/// The engine ships a curated root `help` command, so it disables clap's
/// auto-generated help subcommand on the root. That setting propagates to every
/// subcommand and cannot be re-enabled per child, so `<group> help` would
/// otherwise hit clap's "unrecognized subcommand" error even though the group's
/// help listing advertises a `help` entry. We recognize the form here so the
/// caller can route it through the curated help renderer, matching clap's
/// documented equivalence between `cmd group help sub` and `cmd help group sub`.
///
/// Only groups (commands that have subcommands) are matched: a group is pure
/// subcommand dispatch, so a `help` token in that position is unambiguously a
/// help request. Leaf commands may accept a literal `help` positional argument,
/// so they are left for clap to parse (`<leaf> --help` still works). A group
/// that registers its own real `help` subcommand is likewise deferred to clap,
/// which dispatches the user-defined command (only auto-generated help is
/// suppressed).
///
/// `command_keyword_count` is the number of leading positionals that are
/// genuine command keywords (those before any `--`). A `help` at or beyond that
/// index is a literal operand after `--`, not a help request, so it is ignored.
fn group_help_target_parts(
    root: &Command,
    positionals: &[String],
    command_keyword_count: usize,
) -> Option<Vec<String>> {
    let help_index = positionals.iter().position(|token| token == "help")?;
    // A leading `help` is the curated root help command; let it flow through.
    if help_index == 0 {
        return None;
    }
    // A `help` after a `--` separator is a literal operand; leave it for clap.
    if help_index >= command_keyword_count {
        return None;
    }
    let prefix = &positionals[..help_index];
    let mut current = root;
    for token in prefix {
        current = current.find_subcommand(token)?;
    }
    // The token before `help` must resolve to a group; leaves are left to clap.
    current.get_subcommands().next()?;
    // Defer to clap when the group defines a real `help` subcommand of its own.
    if current.find_subcommand("help").is_some() {
        return None;
    }
    // `<group> help <sub...>` shows help for `<group> <sub...>`.
    let suffix = &positionals[help_index + 1..];
    Some(prefix.iter().chain(suffix).cloned().collect())
}

/// Rewrites a `<group> help [sub...]` invocation into the canonical
/// `help <group> [sub...]` argument vector.
///
/// Only the positional command tokens are reordered (from `[group..., help,
/// sub...]` to `[help, group..., sub...]`); every flag — including `key=value`
/// forms, value-consuming flags, unknown flags that consume a value, and
/// anything after `--` — is preserved in its original place. Reordering keeps
/// the positional count unchanged, so the rewritten stream is filled slot for
/// slot. `parts` is the resolved command path (group + subcommand) from
/// [`group_help_target_parts`].
fn rewrite_group_help_args(
    clap_args: &[String],
    root_name: &str,
    bool_flags: &BTreeSet<String>,
    value_flags: &BTreeSet<String>,
    parts: &[String],
) -> Vec<String> {
    // New positional order: the curated `help` command, then the command path.
    let mut next_positional = std::iter::once("help".to_owned())
        .chain(parts.iter().cloned())
        .peekable();
    let mut out = Vec::with_capacity(clap_args.len());
    let mut iter = clap_args.iter().peekable();
    if iter
        .peek()
        .is_some_and(|arg| arg_matches_root_name(arg, root_name))
        && let Some(program) = iter.next()
    {
        out.push(program.clone());
    }

    let mut take_positional =
        |fallback: &String| next_positional.next().unwrap_or(fallback.clone());

    while let Some(arg) = iter.next() {
        if arg == "--" {
            out.push(arg.clone());
            // Everything after `--` is positional.
            for rest in iter.by_ref() {
                out.push(take_positional(rest));
            }
            break;
        }
        if arg.contains('=') || bool_flags.contains(arg) {
            out.push(arg.clone());
            continue;
        }
        if value_flags.contains(arg) || unknown_flag_consumes_value(arg, iter.peek()) {
            out.push(arg.clone());
            if let Some(value) = iter.next() {
                out.push(value.clone());
            }
            continue;
        }
        if arg.starts_with('-') {
            out.push(arg.clone());
            continue;
        }
        out.push(take_positional(arg));
    }
    // Defensive: emit any positionals not yet placed (counts normally match).
    out.extend(next_positional);
    out
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
        || Path::new(arg)
            .file_stem()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == root_name)
}

/// Outcome of [`Cli::resolve_argv0`]: either rewritten arguments to feed the
/// normal pipeline, or a fully rendered result to return immediately.
enum Argv0Outcome {
    /// Continue the normal run pipeline with these arguments.
    Proceed(Vec<String>),
    /// Return this already-rendered result without further processing.
    Handled(CliRunOutput),
}

/// Extracts the bare program name from an `argv[0]` value, dropping any directory
/// path and file extension (e.g. `/usr/bin/pl` or `pl.exe` both yield `pl`).
/// Falls back to the raw value when no file stem can be derived.
fn program_basename(arg: &str) -> String {
    Path::new(arg)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map_or_else(|| arg.to_owned(), ToOwned::to_owned)
}

/// Returns `true` when `name` is a valid alternative `argv[0]` route name: a
/// non-empty token of ASCII letters, digits, `-`, or `_`. This keeps the name
/// safe as a link/shim filename and as an `argv[0]` basename (which is matched
/// with its extension stripped, so an embedded dot would break matching).
fn is_valid_argv0_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

/// Returns `true` when the entry at `link` already matches what [`Cli::create_link`]
/// would produce for `method`/`target`/`name`, so it can be left untouched. A
/// mismatch (wrong kind, stale symlink target, or differing contents) returns
/// `false` so the caller replaces it.
fn argv0_link_matches(
    link: &Path,
    target: &Path,
    name: &str,
    method: Argv0LinkMethod,
) -> std::io::Result<bool> {
    let metadata = std::fs::symlink_metadata(link)?;
    match method {
        Argv0LinkMethod::SoftLink => {
            Ok(metadata.file_type().is_symlink() && std::fs::read_link(link)? == target)
        }
        Argv0LinkMethod::HardLink => {
            if metadata.file_type().is_symlink() {
                return Ok(false);
            }
            // A correct hard link is indistinguishable from the target by content;
            // comparing bytes also accepts an identical copy, which is harmless.
            Ok(std::fs::read(link)? == std::fs::read(target)?)
        }
        Argv0LinkMethod::Script => {
            if metadata.file_type().is_symlink() {
                return Ok(false);
            }
            Ok(std::fs::read_to_string(link).ok() == Some(argv0_script_contents(target, name)))
        }
    }
}

/// File name for an alternative `argv[0]` link, per method and host platform.
fn argv0_link_file_name(name: &str, method: Argv0LinkMethod) -> String {
    let extension = match method {
        Argv0LinkMethod::Script if cfg!(windows) => ".cmd",
        // Unix scripts are extension-less executables; links carry `.exe` on Windows.
        Argv0LinkMethod::Script => "",
        _ if cfg!(windows) => ".exe",
        _ => "",
    };
    format!("{name}{extension}")
}

/// Contents of an alternative `argv[0]` shim script that forwards to `target`
/// via the explicit `argv0` command. A `.cmd` batch file on Windows, an
/// executable POSIX shell script elsewhere.
fn argv0_script_contents(target: &Path, name: &str) -> String {
    let target = target.display();
    if cfg!(windows) {
        format!("@\"{target}\" argv0 {name} %*\r\n")
    } else {
        format!("#!/bin/sh\nexec \"{target}\" argv0 {name} \"$@\"\n")
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlink creation is not supported on this platform",
    ))
}

/// Marks a freshly written shim script executable on Unix; a no-op elsewhere.
#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Walks a runtime group tree, resolving each node's effective feature flag by
/// cascading from `inherited` — a node's own [`GroupSpec::feature_flag`] or
/// [`CommandSpec::feature_flag`] wins if set, otherwise it inherits the
/// nearest ancestor's effective flag, otherwise (nothing in the ancestor
/// chain declared a flag) it implicitly resolves to [`Stage::Ga`] with no key.
/// Every node that resolves to a *named* flag (own or inherited) is recorded
/// into `registry` under its colon-separated path, together with whether
/// `policy` judged it visible. Nodes that resolve to the implicit no-flag
/// default are not recorded (there is nothing to introspect) and are always
/// visible.
///
/// Returns `None` when this group itself should be dropped from the tree —
/// either because its effective flag is not visible under `policy`, or
/// because every one of its commands and subgroups was pruned away, leaving
/// an empty group with nothing to mount. An emptied-out group is dropped
/// unconditionally, even if its own flag was visible: a `clap` subcommand
/// group with zero children is useless either way, so this simplifies the
/// pruning logic rather than threading through a "was this group itself
/// visible but empty" distinction that no caller needs.
///
/// Note that an invisible ancestor short-circuits before its children are
/// even visited: a more permissive flag on a descendant cannot resurrect a
/// subtree whose enclosing group already failed the visibility check.
fn prune_feature_flag_tree(
    mut group: RuntimeGroupSpec,
    inherited: Option<&FeatureFlag>,
    policy: &FlagPolicy,
    prefix: &mut Vec<String>,
    registry: &mut FlagRegistry,
) -> Option<RuntimeGroupSpec> {
    prefix.push(group.group.name.clone());

    let effective = group
        .group
        .feature_flag
        .clone()
        .or_else(|| inherited.cloned());
    if !record_and_check_visibility(effective.as_ref(), policy, prefix, registry) {
        prefix.pop();
        return None;
    }

    let mut kept_groups = Vec::with_capacity(group.groups.len());
    for child in std::mem::take(&mut group.groups) {
        if let Some(pruned) =
            prune_feature_flag_tree(child, effective.as_ref(), policy, prefix, registry)
        {
            kept_groups.push(pruned);
        }
    }
    group.groups = kept_groups;

    let mut kept_commands = Vec::with_capacity(group.commands.len());
    for command in std::mem::take(&mut group.commands) {
        prefix.push(command.spec.name.clone());
        let command_effective = command
            .spec
            .feature_flag
            .clone()
            .or_else(|| effective.clone());
        let visible =
            record_and_check_visibility(command_effective.as_ref(), policy, prefix, registry);
        prefix.pop();
        if visible {
            kept_commands.push(command);
        }
    }
    group.commands = kept_commands;

    prefix.pop();

    if group.commands.is_empty() && group.groups.is_empty() {
        None
    } else {
        Some(group)
    }
}

/// Records `effective` at the current `prefix` path into `registry` (only
/// when it names a flag key — the implicit Ga default is not recorded) and
/// returns whether the node is visible under `policy`.
fn record_and_check_visibility(
    effective: Option<&FeatureFlag>,
    policy: &FlagPolicy,
    prefix: &[String],
    registry: &mut FlagRegistry,
) -> bool {
    let Some(flag) = effective else {
        return true;
    };
    let visible = policy.visible(Some(flag.key.as_str()), flag.stage);
    registry.record(FlagEntry {
        path: prefix.join(":"),
        key: flag.key.clone(),
        stage: flag.stage,
        visible,
    });
    visible
}

fn register_runtime_group_metadata(
    group: &RuntimeGroupSpec,
    prefix: &mut Vec<String>,
    schemas: &mut SchemaRegistry,
    views: &mut HumanViewRegistry,
) {
    prefix.push(group.group.name.clone());
    for child_group in &group.groups {
        register_runtime_group_metadata(child_group, prefix, schemas, views);
    }
    for child in &group.commands {
        prefix.push(child.spec.name.clone());
        let command_path = prefix.join(":");
        register_command_schema(&child.spec, &command_path, schemas);
        // An inline `with_view` is registered under the command's own path; the
        // dispatch references it by that path. A `with_view_id` takes precedence
        // (dispatch uses it instead), so skip the inline registration when one is
        // set — registering it would leave an unused entry. Shared views are
        // registered separately by the module/CLI.
        if child.spec.view_id.is_none() && !child.spec.view_columns.is_empty() {
            views.register(HumanViewDef::new(
                command_path,
                child.spec.view_columns.clone(),
            ));
        }
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

#[cfg(test)]
mod user_agent_tests {
    use super::*;

    #[test]
    fn user_agent_string_derives_name_and_version_by_default() {
        let config =
            CliConfig::new("gdx", "GoDaddy CLI", "gdx").with_build(BuildInfo::new("1.2.3"));
        assert_eq!(config.user_agent_string(), "gdx/1.2.3");
    }

    #[test]
    fn user_agent_string_prefers_explicit_override() {
        let config = CliConfig::new("gdx", "GoDaddy CLI", "gdx")
            .with_build(BuildInfo::new("1.2.3"))
            .with_user_agent("gdx-cli/9.9 (custom)");
        assert_eq!(config.user_agent_string(), "gdx-cli/9.9 (custom)");
    }

    #[test]
    fn user_agent_string_omits_version_when_absent() {
        let config = CliConfig::new("gdx", "GoDaddy CLI", "gdx");
        assert_eq!(config.user_agent_string(), "gdx");
    }

    #[test]
    fn install_default_user_agent_publishes_config_value() {
        let _guard = crate::transport::client::UA_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restore = crate::transport::client::RestoreDefaultUserAgent;
        crate::transport::set_default_user_agent("cli/dev");
        let cli = Cli::new(
            CliConfig::new("uatest", "UA test", "uatest").with_build(BuildInfo::new("4.5.6")),
        );
        cli.install_default_user_agent();
        assert_eq!(
            crate::transport::client::default_user_agent(),
            "uatest/4.5.6"
        );
    }

    #[test]
    fn install_debug_transport_logger_tracks_the_debug_pattern() {
        // Asserts on `debug_transport_logger_for`'s decision directly rather
        // than publishing to and reading back the process-wide default
        // logger, which `Cli::run` republishes on every call — including the
        // many unrelated tests that call `cli.run(...)` with no `--debug`
        // flag and would otherwise race with this assertion.

        // `transport` selected -> an active (enabled) logger is built.
        assert!(debug_transport_logger_for("transport", &[]).enabled());

        // Wildcard with transport excluded -> a disabled (noop) logger.
        assert!(!debug_transport_logger_for("*,-transport", &[]).enabled());

        // Empty pattern -> disabled (noop).
        assert!(!debug_transport_logger_for("", &[]).enabled());
    }
}

#[cfg(test)]
mod env_config_tests {
    use super::*;

    #[test]
    fn with_environments_stores_shared_arc_with_consumer_app_id() {
        // The consumer sets app_id on the Environments before sharing the Arc;
        // CliConfig stores it as-is, so the file path resolves only because the
        // consumer stamped the matching app_id (not because the engine did).
        let cfg = CliConfig::new("gddy", "GoDaddy CLI", "gddy").with_environments(Arc::new(
            crate::environments::Environments::new("prod")
                .with_app_id("gddy")
                .with_config_file(true),
        ));
        let envs = cfg.environments.as_ref().expect("environments set");
        assert!(envs.config_file_path().is_some());
    }

    #[tokio::test]
    async fn env_flag_overrides_default_and_reaches_middleware_env() {
        use crate::{CommandResult, CommandSpec, RuntimeCommandSpec};
        use serde_json::json;
        let mut cli = Cli::new(
            CliConfig::new("envtest", "Env test", "envtest").with_environments(Arc::new(
                crate::environments::Environments::new("prod")
                    .with_environment("prod", crate::environments::EnvironmentDef::new())
                    .with_environment("ote", crate::environments::EnvironmentDef::new()),
            )),
        );
        cli.add_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("whichenv", "echo env").no_auth(true),
            async |ctx| {
                Ok(CommandResult::new(
                    json!({ "env": ctx.environment()?.name }),
                ))
            },
        ));
        let out = cli
            .run(["envtest", "whichenv", "--env", "ote", "--output", "json"])
            .await;
        assert_eq!(out.exit_code, 0, "rendered: {}", out.rendered);
        assert!(out.rendered.contains("\"env\""));
        assert!(out.rendered.contains("ote"));
    }

    #[tokio::test]
    async fn unknown_env_flag_produces_error_envelope() {
        let cli = Cli::new(
            CliConfig::new("envtest2", "Env test", "envtest2").with_environments(Arc::new(
                crate::environments::Environments::new("prod")
                    .with_environment("prod", crate::environments::EnvironmentDef::new()),
            )),
        );
        let out = cli.run(["envtest2", "tree", "--env", "nope"]).await;
        assert_ne!(out.exit_code, 0);
        assert!(out.rendered.contains("nope"));
    }
}

#[cfg(test)]
mod feature_flag_pruning_tests {
    use super::*;
    use crate::CommandResult;

    fn trivial_command(name: &str) -> RuntimeCommandSpec {
        RuntimeCommandSpec::new(
            CommandSpec::new(name, "short").no_auth(true),
            async |_, _| Ok(CommandResult::new(serde_json::Value::Null)),
        )
    }

    fn flagged_command(name: &str, key: &str, stage: Stage) -> RuntimeCommandSpec {
        let mut command = trivial_command(name);
        command.spec = command.spec.with_feature_flag(key, stage);
        command
    }

    fn empty_policy() -> FlagPolicy {
        FlagPolicy::default()
    }

    #[test]
    fn no_flags_anywhere_keeps_everything() {
        let group = RuntimeGroupSpec::new(GroupSpec::new("root", "short"))
            .with_command(trivial_command("a"))
            .with_command(trivial_command("b"))
            .with_group(
                RuntimeGroupSpec::new(GroupSpec::new("child", "short"))
                    .with_command(trivial_command("c")),
            );

        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned =
            prune_feature_flag_tree(group, None, &empty_policy(), &mut prefix, &mut registry);

        let pruned = pruned.expect("unflagged tree should never be dropped");
        assert_eq!(pruned.commands.len(), 2);
        assert_eq!(pruned.groups.len(), 1);
        assert_eq!(pruned.groups[0].commands.len(), 1);
        assert!(registry.entries().is_empty());
    }

    #[test]
    fn experimental_command_is_pruned_sibling_is_not() {
        let group = RuntimeGroupSpec::new(GroupSpec::new("root", "short"))
            .with_command(flagged_command("gated", "gated-flag", Stage::Experimental))
            .with_command(trivial_command("sibling"));

        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned =
            prune_feature_flag_tree(group, None, &empty_policy(), &mut prefix, &mut registry)
                .expect("group still has a visible command left");

        assert_eq!(pruned.commands.len(), 1);
        assert_eq!(pruned.commands[0].spec.name, "sibling");

        let entries = registry.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "root:gated");
        assert_eq!(entries[0].key, "gated-flag");
        assert!(!entries[0].visible);
    }

    #[test]
    fn beta_group_pruned_under_ga_min_stage_kept_under_beta_min_stage() {
        let build_tree = || {
            RuntimeGroupSpec::new(GroupSpec::new("root", "short"))
                .with_command(trivial_command("keep-me"))
                .with_group(
                    RuntimeGroupSpec::new(
                        GroupSpec::new("flagged-group", "short")
                            .with_feature_flag("group-flag", Stage::Beta),
                    )
                    .with_command(trivial_command("cmd-default"))
                    .with_command(flagged_command(
                        "cmd-ga",
                        "cmd-ga-flag",
                        Stage::Ga,
                    )),
                )
        };

        // Default policy (min_stage: Ga) drops the whole Beta subtree, including
        // both its undeclared and explicitly-Ga-declared children, because the
        // ancestor group itself already fails visibility before children are
        // even visited.
        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned = prune_feature_flag_tree(
            build_tree(),
            None,
            &empty_policy(),
            &mut prefix,
            &mut registry,
        )
        .expect("root keeps its unflagged sibling command");
        assert!(pruned.groups.is_empty());
        assert_eq!(pruned.commands.len(), 1);
        assert_eq!(pruned.commands[0].spec.name, "keep-me");
        // Only the group itself was recorded; its children were never visited.
        assert_eq!(registry.entries().len(), 1);
        assert_eq!(registry.entries()[0].path, "root:flagged-group");
        assert!(!registry.entries()[0].visible);

        // A Beta-permissive policy keeps the group and both of its children.
        let policy = FlagPolicy::default().with_min_stage(Stage::Beta);
        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned =
            prune_feature_flag_tree(build_tree(), None, &policy, &mut prefix, &mut registry)
                .expect("root is kept");
        assert_eq!(pruned.groups.len(), 1);
        assert_eq!(pruned.groups[0].commands.len(), 2);
        assert!(registry.entries().iter().all(|entry| entry.visible));
    }

    #[test]
    fn ancestor_invisibility_short_circuits_before_children_are_visited() {
        // The child declares its own, more permissive Ga flag under a distinct
        // key. Per the documented pruning semantics, an invisible ancestor drops
        // its whole subtree unconditionally: the child's own flag is never even
        // considered, because `prune_feature_flag_tree` returns `None` for the
        // ancestor as soon as its own effective flag fails visibility, before
        // recursing into commands or subgroups at all.
        let group = RuntimeGroupSpec::new(
            GroupSpec::new("ancestor", "short").with_feature_flag("ancestor-flag", Stage::Beta),
        )
        .with_command(flagged_command("child", "child-flag", Stage::Ga));

        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned =
            prune_feature_flag_tree(group, None, &empty_policy(), &mut prefix, &mut registry);

        assert!(
            pruned.is_none(),
            "invisible ancestor drops its whole subtree"
        );
        // The child was never visited, so nothing about it was recorded.
        assert_eq!(registry.entries().len(), 1);
        assert_eq!(registry.entries()[0].path, "ancestor");
        assert!(registry.by_key("child-flag").is_empty());
    }

    #[test]
    fn cascading_inherited_flag_key_and_stage_reach_unflagged_descendants() {
        // Simulates a module-level flag with no per-group/per-command
        // declaration anywhere below it: `inherited` here stands in for
        // `Module::feature_flag`, exactly as `add_module_group_inner` passes it.
        let module_flag = FeatureFlag::new("module-flag", Stage::Beta);
        let group = RuntimeGroupSpec::new(GroupSpec::new("root", "short"))
            .with_command(trivial_command("unflagged-child"));

        let policy = FlagPolicy::default().with_min_stage(Stage::Beta);
        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned = prune_feature_flag_tree(
            group,
            Some(&module_flag),
            &policy,
            &mut prefix,
            &mut registry,
        )
        .expect("Beta-permissive policy keeps a Beta-inherited tree");
        assert_eq!(pruned.commands.len(), 1);

        // Both the group and the descendant command recorded the *same*
        // inherited key/stage, proving real cascading rather than an implicit
        // Ga default at either level.
        let entries = registry.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "root");
        assert_eq!(entries[0].key, "module-flag");
        assert_eq!(entries[0].stage, Stage::Beta);
        assert_eq!(entries[1].path, "root:unflagged-child");
        assert_eq!(entries[1].key, "module-flag");
        assert_eq!(entries[1].stage, Stage::Beta);

        // Under the default (Ga) policy the same inherited Beta flag makes the
        // whole tree invisible together, since the group and its unflagged
        // child resolve to the identical effective flag.
        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned = prune_feature_flag_tree(
            RuntimeGroupSpec::new(GroupSpec::new("root", "short"))
                .with_command(trivial_command("unflagged-child")),
            Some(&module_flag),
            &empty_policy(),
            &mut prefix,
            &mut registry,
        );
        assert!(pruned.is_none());
    }

    #[test]
    fn registry_records_only_named_flags_not_unflagged_nodes() {
        let group = RuntimeGroupSpec::new(GroupSpec::new("root", "short")).with_group(
            RuntimeGroupSpec::new(
                GroupSpec::new("g", "short").with_feature_flag("g-flag", Stage::Beta),
            )
            .with_command(trivial_command("c1"))
            .with_command(flagged_command("c2", "c2-flag", Stage::Ga)),
        );

        // Permissive enough that nothing is pruned, so every node is visited.
        let policy = FlagPolicy::default().with_min_stage(Stage::Experimental);
        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned = prune_feature_flag_tree(group, None, &policy, &mut prefix, &mut registry)
            .expect("permissive policy keeps everything");
        assert_eq!(pruned.groups[0].commands.len(), 2);

        let entries = registry.entries();
        assert_eq!(entries.len(), 3, "root has no flag and is not recorded");
        assert_eq!(entries[0].path, "root:g");
        assert_eq!(entries[0].key, "g-flag");
        assert_eq!(entries[1].path, "root:g:c1");
        assert_eq!(entries[1].key, "g-flag");
        assert_eq!(entries[1].stage, Stage::Beta);
        assert_eq!(entries[2].path, "root:g:c2");
        assert_eq!(entries[2].key, "c2-flag");
        assert_eq!(entries[2].stage, Stage::Ga);
        assert!(entries.iter().all(|entry| entry.visible));
    }

    #[test]
    fn module_feature_flag_cascades_into_its_group_via_add_module() {
        // Regression test for the bug this task fixes: `add_module` used to
        // discard `module.feature_flag` entirely, so a module-level flag could
        // never reach its group/commands. `Module::new` returns a group with an
        // unflagged command; the module itself declares Experimental, and the
        // default (Ga) policy must prune the whole group away.
        let module = Module::new("Test Category", |_ctx| {
            RuntimeGroupSpec::new(GroupSpec::new("gated-mod", "short"))
                .with_command(trivial_command("list"))
        })
        .with_feature_flag("module-flag", Stage::Experimental);

        let mut cli = Cli::new(CliConfig::new("modtest", "Module test", "modtest"));
        cli.add_module(module);

        assert!(
            !cli.commands.contains_key("gated-mod:list"),
            "module-level Experimental flag should have pruned the whole group under the default Ga policy"
        );
        assert!(
            !has_subcommand(&cli.root, "gated-mod"),
            "the pruned group must not be mounted in the clap tree either"
        );
    }

    #[test]
    fn module_feature_flag_keeps_group_when_policy_allows_it() {
        let module = Module::new("Test Category", |_ctx| {
            RuntimeGroupSpec::new(GroupSpec::new("gated-mod-2", "short"))
                .with_command(trivial_command("list"))
        })
        .with_feature_flag("module-flag-2", Stage::Experimental);

        let mut cli = Cli::new(
            CliConfig::new("modtest2", "Module test", "modtest2")
                .with_min_stage(Stage::Experimental),
        );
        cli.add_module(module);

        assert!(cli.commands.contains_key("gated-mod-2:list"));
        assert!(has_subcommand(&cli.root, "gated-mod-2"));
    }

    #[test]
    fn active_environment_min_stage_loosens_consumer_level_policy() {
        // The CliConfig itself leaves min_stage at its Ga default, which would
        // normally prune this Experimental-flagged group. The active ("prod")
        // environment's compiled min_stage override should reach
        // `middleware.flag_policy` before pruning runs and keep it instead.
        let module = Module::new("Test Category", |_ctx| {
            RuntimeGroupSpec::new(GroupSpec::new("gated-mod-3", "short"))
                .with_command(trivial_command("list"))
        })
        .with_feature_flag("module-flag-3", Stage::Experimental);

        let mut cli = Cli::new(
            CliConfig::new("modtest3", "Module test", "modtest3").with_environments(Arc::new(
                crate::environments::Environments::new("prod").with_environment(
                    "prod",
                    crate::environments::EnvironmentDef::new().with_min_stage(Stage::Experimental),
                ),
            )),
        );
        cli.add_module(module);

        assert!(cli.commands.contains_key("gated-mod-3:list"));
        assert!(has_subcommand(&cli.root, "gated-mod-3"));
    }
}

#[cfg(test)]
mod flags_command_tests {
    use super::*;
    use crate::CommandResult;

    /// Builds a module with one flagged group containing one flagged (via
    /// inheritance) `list` command, so `flag_registry` has something to
    /// introspect once the module is mounted.
    fn flagged_module(group_name: &'static str, key: &'static str, stage: Stage) -> Module {
        Module::new("Test Category", move |_ctx| {
            RuntimeGroupSpec::new(GroupSpec::new(group_name, "short")).with_command(
                RuntimeCommandSpec::new(
                    CommandSpec::new("list", "short").no_auth(true),
                    async |_, _| Ok(CommandResult::new(serde_json::Value::Null)),
                ),
            )
        })
        .with_feature_flag(key, stage)
    }

    #[tokio::test]
    async fn flags_list_reports_flagged_entries() {
        let mut cli = Cli::new(
            CliConfig::new("flagtest", "Flag test", "flagtest").with_min_stage(Stage::Beta),
        );
        cli.add_module(flagged_module("flagged-mod", "list-flag", Stage::Beta));

        let out = cli
            .run(["flagtest", "flags", "list", "--output", "json"])
            .await;
        assert_eq!(out.exit_code, 0, "rendered: {}", out.rendered);
        let rendered: serde_json::Value =
            serde_json::from_str(&out.rendered).expect("stdout should contain json");
        let entries = rendered["data"].as_array().expect("data should be array");
        let command_entry = entries
            .iter()
            .find(|entry| entry["path"] == "flagged-mod:list")
            .expect("flagged command entry should be present");
        assert_eq!(command_entry["key"], "list-flag");
        assert_eq!(command_entry["stage"], "beta");
        assert_eq!(command_entry["visible"], true);
    }

    #[tokio::test]
    async fn flags_info_returns_policy_and_entries_for_known_key() {
        let mut cli = Cli::new(
            CliConfig::new("flagtest2", "Flag test", "flagtest2").with_min_stage(Stage::Beta),
        );
        cli.add_module(flagged_module("flagged-mod-2", "info-flag", Stage::Beta));

        let out = cli
            .run([
                "flagtest2",
                "flags",
                "info",
                "info-flag",
                "--output",
                "json",
            ])
            .await;
        assert_eq!(out.exit_code, 0, "rendered: {}", out.rendered);
        let rendered: serde_json::Value =
            serde_json::from_str(&out.rendered).expect("stdout should contain json");
        let data = &rendered["data"];
        assert_eq!(data["key"], "info-flag");
        assert_eq!(data["policy"]["min_stage"], "beta");
        assert!(data["policy"]["override"].is_null());
        let entries = data["entries"].as_array().expect("entries should be array");
        assert!(!entries.is_empty());
        assert!(entries.iter().any(|entry| {
            entry["path"] == "flagged-mod-2:list" && entry["decided_by"] == "min_stage"
        }));
    }

    #[tokio::test]
    async fn flags_info_reports_override_decided_by() {
        // The module declares Experimental, which the default Ga policy would
        // normally hide; the override forces Ga instead, so the entries stay
        // visible even though `entry.stage` still reports the node's own
        // (Experimental) declaration, not the override.
        let mut cli = Cli::new(
            CliConfig::new("flagtest3", "Flag test", "flagtest3")
                .with_feature_override("override-flag", Stage::Ga),
        );
        cli.add_module(flagged_module(
            "flagged-mod-3",
            "override-flag",
            Stage::Experimental,
        ));

        let out = cli
            .run([
                "flagtest3",
                "flags",
                "info",
                "override-flag",
                "--output",
                "json",
            ])
            .await;
        assert_eq!(out.exit_code, 0, "rendered: {}", out.rendered);
        let rendered: serde_json::Value =
            serde_json::from_str(&out.rendered).expect("stdout should contain json");
        let data = &rendered["data"];
        assert_eq!(data["policy"]["min_stage"], "ga");
        assert_eq!(data["policy"]["override"], "ga");
        let entries = data["entries"].as_array().expect("entries should be array");
        assert!(!entries.is_empty());
        assert!(
            entries
                .iter()
                .all(|entry| entry["decided_by"] == "override")
        );
        assert!(entries.iter().all(|entry| entry["visible"] == true));
        assert!(entries.iter().all(|entry| entry["stage"] == "experimental"));
    }

    #[tokio::test]
    async fn flags_info_unknown_key_errors() {
        let cli = Cli::new(CliConfig::new("flagtest4", "Flag test", "flagtest4"));

        let out = cli
            .run(["flagtest4", "flags", "info", "no-such-flag"])
            .await;
        assert_ne!(out.exit_code, 0);
        assert!(out.rendered.contains("no such flag"));
    }
}
