use std::collections::BTreeMap;

use clap::{Arg, ArgGroup, Command};
use schemars::JsonSchema;

use crate::{
    AuthRequirement, CommandMeta, FeatureFlag, OutputSchema, SchemaInfo, Stage, Tier,
    output::TableColumn,
};

/// Declarative leaf command metadata and parser arguments.
///
/// `CommandSpec` intentionally keeps command metadata next to the command's
/// handler. This is the primary copy/paste surface for teams adding commands.
///
/// Construct with [`CommandSpec::new`] or [`CommandSpec::from_args`], then
/// configure with the `with_*` builder methods — never as a struct literal.
/// `#[non_exhaustive]` enforces this so the engine can add fields (as it did
/// for [`arg_groups`](CommandSpec::arg_groups)) without a breaking release.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct CommandSpec {
    /// Leaf command name.
    pub name: String,
    /// One-line command description.
    pub short: String,
    /// Optional long help text.
    pub long: Option<String>,
    /// Alternate command names accepted by the parser.
    pub aliases: Vec<String>,
    /// Whether the command runs but is hidden from help, tree, and search.
    pub hidden: bool,
    /// Backend/system id used in output metadata and generic error envelopes.
    pub system: Option<String>,
    /// Default comma-separated field projection.
    pub default_fields: Option<String>,
    /// Authentication requirement enforced by the engine for this command.
    ///
    /// Defaults to [`AuthRequirement::Required`] (fail-closed). Use
    /// [`auth_optional`](CommandSpec::auth_optional) for commands that should run
    /// logged out, or [`no_auth`](CommandSpec::no_auth) for commands that never
    /// authenticate.
    pub auth: AuthRequirement,
    /// Auth provider name for this command.
    pub auth_provider: Option<String>,
    /// Risk tier used by authentication, authorization, and dry-run.
    pub tier: Option<Tier>,
    /// Explicit dry-run prompt marker for commands without a tier.
    pub mutates: bool,
    /// Opts this command into handler-driven `--dry-run`.
    ///
    /// Set with [`handles_dry_run`](CommandSpec::handles_dry_run). When
    /// `true`, the engine skips its generic `--dry-run` short-circuit for
    /// this command and invokes the handler as normal (still respecting the
    /// command's [`AuthRequirement`]). The handler is responsible for
    /// running its real validation unconditionally, checking
    /// [`CommandContext::dry_run`](crate::CommandContext::dry_run) to skip
    /// only the mutating I/O, and tagging its preview result with
    /// [`CommandResult::with_dry_run`](crate::CommandResult::with_dry_run).
    ///
    /// **Requires a context-aware handler.** Only handlers built with
    /// [`RuntimeCommandSpec::new_with_context`](crate::RuntimeCommandSpec::new_with_context),
    /// [`new_streaming`](crate::RuntimeCommandSpec::new_streaming),
    /// [`new_typed_with_context`](crate::RuntimeCommandSpec::new_typed_with_context),
    /// or [`new_typed_streaming`](crate::RuntimeCommandSpec::new_typed_streaming)
    /// receive a [`CommandContext`](crate::CommandContext) and can call
    /// [`CommandContext::dry_run`](crate::CommandContext::dry_run). A handler
    /// built with [`RuntimeCommandSpec::new`](crate::RuntimeCommandSpec::new)/
    /// [`new_typed`](crate::RuntimeCommandSpec::new_typed) only receives
    /// `(CredentialResolver, args)` — it has no way to observe `--dry-run` at
    /// all, so opting it into `handles_dry_run` would silently execute the
    /// handler's real side effects under `--dry-run` instead of skipping
    /// them. `RuntimeCommandSpec::new`/`new_typed` debug-assert against this
    /// misuse; release builds do not, so treat the assert as a
    /// development-time safety net, not the actual guarantee — only pair
    /// this field with one of the four context-aware constructors above.
    pub handles_dry_run: bool,
    /// Forces this command's successful output to print verbatim to stdout.
    pub raw_output: bool,
    /// Provider-specific auth metadata.
    pub auth_metadata: BTreeMap<String, String>,
    /// Command-specific `clap` arguments.
    pub args: Vec<Arg>,
    /// Argument relations (mutually-exclusive or "at least one of" groups).
    ///
    /// Set with [`with_arg_group`](CommandSpec::with_arg_group), or captured
    /// automatically by [`from_args`](CommandSpec::from_args) from a
    /// `#[derive(clap::Args)]` struct's `#[group(...)]` attribute.
    pub arg_groups: Vec<ArgGroup>,
    /// Optional output schema published through `--schema` and help.
    pub output_schema: Option<SchemaInfo>,
    /// Inline human-output table columns assigned directly to this command.
    ///
    /// Set with [`with_view`](CommandSpec::with_view). When present (and
    /// [`view_id`](CommandSpec::view_id) is unset), the engine registers these
    /// columns under the command's own path so human output renders them.
    pub view_columns: Vec<TableColumn>,
    /// Id of a shared human view this command should use.
    ///
    /// Set with [`with_view_id`](CommandSpec::with_view_id). Names a
    /// [`HumanViewDef`](crate::HumanViewDef) registered with `with_view` on the
    /// module or CLI, so several commands can share one table. Takes precedence
    /// over inline [`view_columns`](CommandSpec::view_columns).
    pub view_id: Option<String>,
    /// This command's own feature-flag declaration, if any.
    ///
    /// `None` means the command has no explicit stage declaration of its own,
    /// in which case it inherits its effective stage from its nearest ancestor
    /// (nested group, then enclosing group, then module — nearest declaration
    /// wins), implicitly resolving to [`Stage::Ga`] if nothing in the ancestor
    /// chain declares a flag either; see [`Stage`]'s documentation for why
    /// that is its default. Set with
    /// [`with_feature_flag`](CommandSpec::with_feature_flag). This field only
    /// records the command's own declaration; cascading resolution against the
    /// ancestor chain happens when a [`Cli`](crate::Cli) mounts the enclosing
    /// module or group.
    pub feature_flag: Option<FeatureFlag>,
    /// This command's opt-in pagination policy, if any.
    ///
    /// `None` (the default) means the command does not paginate: `--limit`/
    /// `--offset` are not registered for it, so they neither show up in its
    /// `--help` nor parse on its command line. Set with
    /// [`with_pagination`](CommandSpec::with_pagination).
    pub pagination: Option<PaginationConfig>,
}

/// Opt-in pagination policy for a single command, set with
/// [`CommandSpec::with_pagination`].
///
/// Registering this is what makes `--limit`/`--offset` exist for a command at
/// all — without it, the engine does not register those flags, so they are
/// absent from `--help` and rejected as unknown arguments if passed. Construct
/// it with `..Default::default()`, as in the example below, so a future
/// engine release can add fields without breaking existing callers.
///
/// ```
/// use cli_engine::PaginationConfig;
///
/// let pagination = PaginationConfig {
///     default_limit: 20,
///     max_limit: 100,
///     ..Default::default()
/// };
/// assert_eq!(pagination.default_limit, 20);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaginationConfig {
    /// Page size applied when the user passes neither `--limit` nor
    /// `--offset`. `0` (the default) means unlimited — the same "no
    /// pagination" sentinel used everywhere else in the output pipeline.
    pub default_limit: i64,
    /// Upper bound a user can request with an explicit `--limit`. `0` (the
    /// default) means uncapped. Does not affect `default_limit` itself.
    pub max_limit: i64,
}

impl CommandSpec {
    /// Creates a command spec with the required name and one-line help.
    #[must_use]
    pub fn new(name: impl Into<String>, short: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            short: short.into(),
            ..Self::default()
        }
    }

    /// Creates a command spec from a `#[derive(clap::Args)]` struct.
    ///
    /// Extracts the argument definitions from the derive type and populates the
    /// spec's args list. The command name and help text are still required since
    /// `Args` types do not carry those. Also captures any `ArgGroup`s the derive
    /// macro registers (via a struct-level `#[group(...)]` attribute) into
    /// [`arg_groups`](CommandSpec::arg_groups).
    ///
    /// **Flatten caveat**: `clap_derive` empties a struct's own implicit group's
    /// member list when the struct also has a `#[command(flatten)]` field, so a
    /// `#[group(required = true)]` on such a struct silently enforces nothing.
    /// This is debug-asserted against below; treat it as a development-time
    /// safety net, not the actual guarantee.
    #[must_use]
    pub fn from_args<T: clap::Args>(name: impl Into<String>, short: impl Into<String>) -> Self {
        let name = name.into();
        let placeholder = Command::new("__placeholder");
        let augmented = T::augment_args(placeholder);
        let args: Vec<Arg> = augmented
            .get_arguments()
            // `cli-engine` registers its own global `--help` flag. Retain a
            // command-specific `--version` flag: it may represent a resource
            // version rather than the CLI binary version.
            .filter(|arg| arg.get_id().as_str() != "help")
            .cloned()
            .collect();
        let arg_groups: Vec<ArgGroup> = augmented.get_groups().cloned().collect();
        debug_assert!(
            arg_groups
                .iter()
                .all(|group| !group.is_required_set() || group.get_args().count() > 0),
            "command {name:?} has a required ArgGroup with no member args — likely the \
             clap_derive flatten+group interaction emptying the implicit group's \
             member list; the constraint will not be enforced"
        );
        Self {
            name,
            short: short.into(),
            args,
            arg_groups,
            ..Self::default()
        }
    }

    /// Sets expanded command help.
    #[must_use]
    pub fn with_long(mut self, long: impl Into<String>) -> Self {
        self.long = Some(long.into());
        self
    }

    /// Adds one command alias.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// Hides or shows this command in discovery output.
    #[must_use]
    pub fn hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Sets the backend/system id for output metadata and error attribution.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Sets the default field projection used when `--fields` is absent.
    #[must_use]
    pub fn with_default_fields(mut self, default_fields: impl Into<String>) -> Self {
        self.default_fields = Some(default_fields.into());
        self
    }

    /// Assigns an inline human-output table view to this command.
    ///
    /// The columns are registered under the command's own path, so human output
    /// renders this table directly. Field selection still applies: `--fields`
    /// (defaulting to [`default_fields`](CommandSpec::default_fields)) narrows
    /// which of these columns show. Use
    /// [`with_view_id`](CommandSpec::with_view_id) instead to point at a shared
    /// view registered with `with_view` on the module or CLI.
    #[must_use]
    pub fn with_view(mut self, columns: impl Into<Vec<TableColumn>>) -> Self {
        self.view_columns = columns.into();
        self
    }

    /// Points this command at a shared human view by id.
    ///
    /// The id must match a [`HumanViewDef`](crate::HumanViewDef) registered with
    /// `with_view` on the module or CLI, letting several commands share one
    /// table. Takes precedence over inline [`with_view`](CommandSpec::with_view)
    /// columns.
    #[must_use]
    pub fn with_view_id(mut self, id: impl Into<String>) -> Self {
        self.view_id = Some(id.into());
        self
    }

    /// Selects the auth provider for this command.
    #[must_use]
    pub fn with_auth_provider(mut self, provider: impl Into<String>) -> Self {
        self.auth_provider = Some(provider.into());
        self
    }

    /// Marks the command as no-auth.
    ///
    /// `no_auth(true)` sets [`AuthRequirement::None`]: the command never resolves
    /// a credential and default-env injection is suppressed. `no_auth(false)`
    /// restores the default [`AuthRequirement::Required`].
    #[must_use]
    pub fn no_auth(mut self, no_auth: bool) -> Self {
        self.auth = if no_auth {
            AuthRequirement::None
        } else {
            AuthRequirement::Required
        };
        self
    }

    /// Sets the command's [`AuthRequirement`] explicitly.
    #[must_use]
    pub fn auth(mut self, requirement: AuthRequirement) -> Self {
        self.auth = requirement;
        self
    }

    /// Marks authentication as optional ([`AuthRequirement::Optional`]).
    ///
    /// The engine does not resolve a credential before the handler runs; the
    /// handler triggers the auth flow only by calling
    /// [`CredentialResolver::resolve`](crate::CredentialResolver::resolve)/
    /// [`try_resolve`](crate::CredentialResolver::try_resolve). Use for
    /// commands that should still run when the user is logged out.
    #[must_use]
    pub fn auth_optional(mut self) -> Self {
        self.auth = AuthRequirement::Optional;
        self
    }

    /// Sets the command risk tier.
    #[must_use]
    pub fn with_tier(mut self, tier: Tier) -> Self {
        self.tier = Some(tier);
        self
    }

    /// Declares this command's own feature flag: the key used for policy
    /// overrides and introspection, and the stage at which it becomes visible.
    #[must_use]
    pub fn with_feature_flag(mut self, key: impl Into<String>, stage: Stage) -> Self {
        self.feature_flag = Some(FeatureFlag::new(key, stage));
        self
    }

    /// Opts this command into paginated list output.
    ///
    /// Registers `--limit`/`--offset` for this command only — a command that
    /// never calls this does not get those flags at all, in `--help` or on
    /// the command line. When the user passes neither flag, `config.default_limit`
    /// applies instead of the framework's "pagination disabled" default of
    /// unlimited; an explicit `--limit` above `config.max_limit` (when set) is
    /// rejected before the command runs. See [`PaginationConfig`].
    #[must_use]
    pub fn with_pagination(mut self, config: PaginationConfig) -> Self {
        debug_assert!(
            config.max_limit == 0 || config.default_limit <= config.max_limit,
            "command {:?} has a default_limit ({}) greater than its max_limit ({})",
            self.name,
            config.default_limit,
            config.max_limit
        );
        self.pagination = Some(config);
        self
    }

    /// Adds provider-specific auth metadata.
    #[must_use]
    pub fn with_auth_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.auth_metadata.insert(key.into(), value.into());
        self
    }

    /// Declares the OAuth scopes this command requires.
    ///
    /// Sugar over [`with_auth_metadata`](CommandSpec::with_auth_metadata) with the
    /// `"scopes"` key (whitespace-joined). The scopes surface on
    /// [`CommandMeta::scopes`](crate::CommandMeta) and reach the auth provider via
    /// [`CredentialRequest`](crate::CredentialRequest); a provider that supports
    /// scope step-up re-authenticates when the cached token lacks them.
    #[must_use]
    pub fn with_scopes(mut self, scopes: &[impl AsRef<str>]) -> Self {
        let joined = scopes
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(" ");
        // Mirror `CommandMeta::set_scopes`: an empty list clears the key rather
        // than leaving an empty-but-present `auth_metadata["scopes"]`.
        if joined.is_empty() {
            self.auth_metadata.remove("scopes");
        } else {
            self.auth_metadata.insert("scopes".to_owned(), joined);
        }
        self
    }

    /// Adds a `clap` argument or option to this command.
    #[must_use]
    pub fn with_arg(mut self, arg: Arg) -> Self {
        self.args.push(arg);
        self
    }

    /// Adds a `clap` flag or option to this command.
    #[must_use]
    pub fn with_flag(self, flag: Arg) -> Self {
        self.with_arg(flag)
    }

    /// Adds an argument relation (an `ArgGroup`) to this command, e.g. to
    /// express "at least one of" or mutually-exclusive relationships between
    /// arguments added with [`with_arg`](CommandSpec::with_arg)/[`with_flag`](CommandSpec::with_flag).
    ///
    /// The group's `ArgGroup::args([...])` ids must reference args already (or
    /// later) added to this spec, matching `clap`'s own requirement that
    /// referenced arg ids exist on the built `Command`. This replaces
    /// hand-rolled `required_unless_present_any`/`conflicts_with` chains with a
    /// single declarative relation.
    #[must_use]
    pub fn with_arg_group(mut self, group: ArgGroup) -> Self {
        self.arg_groups.push(group);
        self
    }

    /// Registers a compact framework schema from an [`OutputSchema`] type.
    #[must_use]
    pub fn with_output_schema<T: OutputSchema>(mut self) -> Self {
        self.output_schema = Some(SchemaInfo {
            command: String::new(),
            fields: crate::output::fields_for::<T>(),
            schema: None,
        });
        self
    }

    /// Registers JSON Schema generated from a Rust type with `schemars`.
    #[must_use]
    pub fn with_json_schema<T: JsonSchema>(mut self) -> Self {
        self.output_schema = Some(crate::output::json_schema_info::<T>(""));
        self
    }

    /// Marks whether the command should short-circuit under `--dry-run`.
    #[must_use]
    pub fn mutates(mut self, mutates: bool) -> Self {
        self.mutates = mutates;
        self
    }

    /// Opts this command into handler-driven `--dry-run` instead of the
    /// engine's generic short-circuit.
    ///
    /// See [`handles_dry_run`](CommandSpec::handles_dry_run) (the field) for
    /// the contract a handler must follow once it opts in — in particular,
    /// **only use this with a context-aware handler**
    /// ([`RuntimeCommandSpec::new_with_context`](crate::RuntimeCommandSpec::new_with_context),
    /// [`new_streaming`](crate::RuntimeCommandSpec::new_streaming),
    /// [`new_typed_with_context`](crate::RuntimeCommandSpec::new_typed_with_context), or
    /// [`new_typed_streaming`](crate::RuntimeCommandSpec::new_typed_streaming)); a
    /// `new`/`new_typed` handler can't observe `--dry-run` and would execute
    /// its real side effects under it regardless of this flag.
    #[must_use]
    pub fn handles_dry_run(mut self, handles: bool) -> Self {
        self.handles_dry_run = handles;
        self
    }

    /// Forces this command's successful output to print verbatim to stdout.
    #[must_use]
    pub fn raw_output(mut self, raw_output: bool) -> Self {
        self.raw_output = raw_output;
        self
    }

    /// Builds middleware metadata from the spec.
    #[must_use]
    pub fn metadata(&self) -> CommandMeta {
        let mut auth_metadata = self.auth_metadata.clone();
        if let Some(provider) = &self.auth_provider
            && !provider.is_empty()
        {
            auth_metadata.insert("provider".to_owned(), provider.clone());
        }
        if let Some(tier) = self.tier
            && !auth_metadata.contains_key("tier")
        {
            auth_metadata.insert("tier".to_owned(), tier.to_string());
        }
        let scopes = auth_metadata
            .get("scopes")
            .map(|scopes| {
                scopes
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        CommandMeta {
            dry_run_prompt: self.mutates || self.tier.is_some_and(Tier::is_mutating),
            handles_dry_run: self.handles_dry_run,
            auth_metadata,
            scopes,
        }
    }

    /// Builds the `clap` command for parser registration.
    #[must_use]
    pub fn clap_command(&self) -> Command {
        let mut command = Command::new(self.name.clone()).about(self.short.clone());
        if let Some(long) = &self.long
            && !long.is_empty()
        {
            command = command.long_about(long.clone());
        }
        for alias in &self.aliases {
            command = command.alias(alias.clone());
        }
        if self.hidden {
            command = command.hide(true);
        }
        // Explicit `display_order` (rather than relying on clap's own
        // implicit per-`Command` counter) guarantees these render first, as
        // a block, in declaration order — see `flags::global_flag_order`
        // for why leaving it implicit lets a propagated global flag collide
        // with a low counter value here and interleave with these instead.
        for (index, arg) in self.args.iter().enumerate() {
            command = command.arg(arg.clone().display_order(index));
        }
        for group in &self.arg_groups {
            command = command.group(group.clone());
        }
        command
    }
}

#[cfg(test)]
mod tests {
    use clap::{Arg, ArgGroup};

    use super::*;

    #[test]
    fn command_spec_with_feature_flag_sets_key_and_stage() {
        let spec =
            CommandSpec::new("list", "List things").with_feature_flag("my-flag", Stage::Beta);

        let flag = spec
            .feature_flag
            .as_ref()
            .expect("feature flag should be set");
        assert_eq!(flag.key, "my-flag");
        assert_eq!(flag.stage, Stage::Beta);
    }

    #[test]
    fn command_spec_feature_flag_defaults_to_none() {
        let spec = CommandSpec::new("list", "List things");

        assert!(spec.feature_flag.is_none());
    }

    #[test]
    fn command_spec_with_arg_group_registers_group_on_clap_command() {
        let spec = CommandSpec::new("update", "Update a thing")
            .with_arg(Arg::new("a").long("a"))
            .with_arg(Arg::new("b").long("b"))
            .with_arg_group(ArgGroup::new("ab").args(["a", "b"]).required(true));

        assert!(
            spec.clap_command()
                .try_get_matches_from(["update"])
                .is_err(),
            "neither `a` nor `b` present should fail the required group"
        );
        assert!(
            spec.clap_command()
                .try_get_matches_from(["update", "--a", "x"])
                .is_ok()
        );
    }

    #[test]
    fn command_spec_from_args_preserves_derive_arg_group() {
        #[derive(clap::Args)]
        #[group(required = true, multiple = false)]
        struct ExclusiveArgs {
            #[arg(long)]
            one: bool,
            #[arg(long)]
            two: bool,
        }

        let spec = CommandSpec::from_args::<ExclusiveArgs>("bump", "Bump one thing");

        assert_eq!(spec.arg_groups.len(), 1);
        let group = &spec.arg_groups[0];
        assert!(group.is_required_set());
        assert_eq!(group.get_args().count(), 2);
    }

    #[test]
    fn command_spec_from_args_preserves_version_argument() {
        #[derive(clap::Args)]
        struct ReleaseArgs {
            #[arg(long)]
            version: String,
        }

        let spec = CommandSpec::from_args::<ReleaseArgs>("release", "Create a release");

        assert!(
            spec.clap_command()
                .try_get_matches_from(["release", "--version", "1.0.0"])
                .is_ok(),
            "typed command arguments named `version` must remain available as `--version`"
        );
    }
}
