use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

use clap::{Arg, ArgAction, ArgMatches, Command};
use schemars::JsonSchema;
use serde_json::{Number, Value};
use tokio::sync::mpsc;

use crate::{
    AuthRequirement, CommandMeta, Credential, CredentialResolver, Middleware, OutputSchema, Result,
    SchemaInfo, Tier, middleware::ValueMap, output::NextAction,
};

/// Sender half for streaming command output.
///
/// Streaming handlers call [`StreamSender::send`] for each progress event.
/// The engine drains the channel and writes each event as an NDJSON line.
#[derive(Clone, Debug)]
pub struct StreamSender(pub(crate) mpsc::Sender<Value>);

impl StreamSender {
    /// Sends one event. Silently drops the event if the receiver is gone.
    pub async fn send(&self, event: Value) {
        drop(self.0.send(event).await);
    }
}

/// Boxed future returned by runtime command handlers.
pub type CommandFuture = Pin<Box<dyn Future<Output = Result<CommandResult>> + Send>>;
/// Shared command handler used by [`RuntimeCommandSpec`].
pub type CommandHandler = Arc<dyn Fn(CommandContext) -> CommandFuture + Send + Sync>;

/// Boxed future returned by streaming command handlers.
pub type StreamingCommandFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
/// Shared streaming handler: receives context and an event sender; returns when the stream ends.
pub type StreamingCommandHandler =
    Arc<dyn Fn(CommandContext, StreamSender) -> StreamingCommandFuture + Send + Sync>;

/// Data returned by a command handler.
///
/// Command handlers should return renderable data and keep output metadata on
/// [`CommandSpec`]. The metadata field is reserved for future command-result
/// extensions that are not known when the command is registered.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandResult {
    /// JSON data rendered by the configured output formatter.
    pub data: Value,
    /// Optional command-result extension metadata.
    pub metadata: CommandResultMetadata,
}

impl CommandResult {
    /// Creates a command result from renderable JSON data.
    #[must_use]
    pub fn new(data: Value) -> Self {
        Self {
            data,
            metadata: CommandResultMetadata::default(),
        }
    }

    /// Attaches suggested follow-up actions to this result.
    #[must_use]
    pub fn with_next_actions(mut self, actions: Vec<NextAction>) -> Self {
        self.metadata.next_actions = actions;
        self
    }
}

impl From<Value> for CommandResult {
    fn from(data: Value) -> Self {
        Self::new(data)
    }
}

/// Optional metadata a command can attach to its result.
#[non_exhaustive]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandResultMetadata {
    /// Suggested follow-up actions for the caller.
    pub next_actions: Vec<NextAction>,
}

/// Runtime context passed to advanced command handlers.
///
/// Most commands can use [`RuntimeCommandSpec::new`] and receive just the
/// credential and effective args. Use this context when a command needs the
/// colon path, user-supplied args, or a snapshot of middleware state.
///
/// This struct is constructed by the framework during command dispatch.
/// Consumer code receives it in handler closures and should not construct it
/// directly.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CommandContext {
    /// Lazy credential resolver.
    pub credential: CredentialResolver,
    /// Effective arguments, including defaults and framework-injected values.
    pub args: ValueMap,
    /// Arguments explicitly supplied by the user.
    pub user_args: ValueMap,
    /// Colon-separated command path such as `project:list`.
    pub command_path: String,
    /// Middleware snapshot for this invocation.
    pub middleware: Middleware,
    /// Raw `clap` matches for typed argument deserialization via derive.
    pub raw_matches: Arc<ArgMatches>,
}

impl CommandContext {
    /// Returns the per-application config file as loaded at startup.
    ///
    /// Read a consumer-owned section with
    /// [`ConfigFile::section`](crate::config::ConfigFile::section), for example
    /// `ctx.config().section::<DeployConfig>("deploy")?`. Engine-reserved
    /// settings are available via
    /// [`ConfigFile::engine`](crate::config::ConfigFile::engine).
    ///
    /// **Snapshot semantics**: this is the config loaded once when
    /// [`crate::cli::Cli::new`] was called. Changes made by `config set` during the same process
    /// invocation (e.g. from a previous `Cli::run`) are not reflected here;
    /// restart the CLI (a new `Cli::new`) to pick them up. For a one-shot CLI
    /// process this is always the current on-disk state.
    #[must_use]
    pub fn config(&self) -> &crate::config::ConfigFile {
        &self.middleware.config
    }

    /// Deserializes the raw argument matches into a typed args struct.
    ///
    /// Use this with `#[derive(clap::Args)]` structs to get type-safe access
    /// to command arguments instead of working with the `ValueMap` directly.
    ///
    /// # Errors
    ///
    /// Returns an error if the matches cannot be deserialized into `T`.
    pub fn typed_args<T: clap::FromArgMatches>(&self) -> Result<T> {
        T::from_arg_matches(self.raw_matches.as_ref())
            .map_err(|e| crate::CliCoreError::Message(format!("argument parse error: {e}")))
    }

    /// Resolves the credential for this command, triggering the auth flow on
    /// first use and memoizing the result.
    ///
    /// Convenience wrapper over [`self.credential.resolve()`](CredentialResolver::resolve).
    ///
    /// # Errors
    ///
    /// Returns an error when the command is marked `no_auth`, or when the auth
    /// provider fails to produce a credential.
    pub async fn credential(&self) -> Result<Credential> {
        self.credential.resolve().await
    }

    /// Resolves the credential when one is available, returning `Ok(None)` for
    /// no-auth commands.
    ///
    /// Convenience wrapper over [`self.credential.try_resolve()`](CredentialResolver::try_resolve).
    ///
    /// # Errors
    ///
    /// Propagates the auth provider error when resolution is attempted and fails.
    pub async fn try_credential(&self) -> Result<Option<Credential>> {
        self.credential.try_resolve().await
    }

    /// Resolves a credential that additionally covers `extra` scopes, on top of
    /// the command's declared scopes.
    ///
    /// Use this when the required scopes are only known at runtime (for example
    /// a generic API caller that derives scopes from the target endpoint). A
    /// scope-aware auth provider re-authenticates when the cached token does not
    /// already cover the requested set.
    ///
    /// Convenience wrapper over
    /// [`self.credential.resolve_with_scopes()`](CredentialResolver::resolve_with_scopes).
    ///
    /// If the handler also issues HTTP requests through the transport bearer
    /// injector, call this **before** the first request: the injector resolves
    /// and caches a scope-unaware token, so stepping up afterwards would not
    /// affect requests it already authorized. See
    /// [`CredentialResolver::resolve_with_scopes`] for the full ordering note.
    ///
    /// # Errors
    ///
    /// Returns an error when the command is marked `no_auth`, or when the auth
    /// provider fails to produce a credential.
    pub async fn credential_with_scopes(&self, extra: &[String]) -> Result<Credential> {
        self.credential.resolve_with_scopes(extra).await
    }
}

/// Declarative leaf command metadata and parser arguments.
///
/// `CommandSpec` intentionally keeps command metadata next to the command's
/// handler. This is the primary copy/paste surface for teams adding commands.
#[derive(Clone, Debug, Default)]
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
    /// Provider-specific auth metadata.
    pub auth_metadata: BTreeMap<String, String>,
    /// Command-specific `clap` arguments.
    pub args: Vec<Arg>,
    /// Optional output schema published through `--schema` and help.
    pub output_schema: Option<SchemaInfo>,
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
    /// `Args` types do not carry those.
    #[must_use]
    pub fn from_args<T: clap::Args>(name: impl Into<String>, short: impl Into<String>) -> Self {
        let placeholder = Command::new("__placeholder");
        let augmented = T::augment_args(placeholder);
        let args: Vec<Arg> = augmented
            .get_arguments()
            .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
            .cloned()
            .collect();
        Self {
            name: name.into(),
            short: short.into(),
            args,
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
    /// [`CredentialResolver::resolve`]/[`try_resolve`](CredentialResolver::try_resolve).
    /// Use for commands that should still run when the user is logged out.
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
        for arg in &self.args {
            command = command.arg(arg.clone());
        }
        command
    }
}

/// Declarative command group metadata.
///
/// Groups are noun-based containers. They do not run business logic directly;
/// when invoked bare, the CLI renders group help.
#[derive(Clone, Debug, Default)]
pub struct GroupSpec {
    /// Group command name.
    pub name: String,
    /// One-line group description.
    pub short: String,
    /// Optional long help text.
    pub long: Option<String>,
    /// Alternate group names accepted by the parser.
    pub aliases: Vec<String>,
    /// Whether the group runs but is hidden from discovery output.
    pub hidden: bool,
    /// Declarative child commands used for static tree construction.
    pub commands: Vec<CommandSpec>,
    /// Declarative nested groups used for static tree construction.
    pub groups: Vec<GroupSpec>,
}

impl GroupSpec {
    /// Creates a command group with the required name and one-line help.
    #[must_use]
    pub fn new(name: impl Into<String>, short: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            short: short.into(),
            ..Self::default()
        }
    }

    /// Sets expanded group help.
    #[must_use]
    pub fn with_long(mut self, long: impl Into<String>) -> Self {
        self.long = Some(long.into());
        self
    }

    /// Adds one group alias.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// Hides or shows this group in discovery output.
    #[must_use]
    pub fn hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Adds one declarative child command.
    #[must_use]
    pub fn with_command(mut self, command: CommandSpec) -> Self {
        self.commands.push(command);
        self
    }

    /// Adds one declarative nested group.
    #[must_use]
    pub fn with_group(mut self, group: GroupSpec) -> Self {
        self.groups.push(group);
        self
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
        for group in &self.groups {
            command = command.subcommand(group.clap_command());
        }
        for child in &self.commands {
            command = command.subcommand(child.clap_command());
        }
        command
    }
}

/// Executable leaf command.
///
/// `RuntimeCommandSpec` pairs a [`CommandSpec`] with async business logic.
/// This split keeps metadata inspectable for help/search/schema generation
/// before the handler ever runs.
///
/// Use [`RuntimeCommandSpec::new_streaming`] for commands that emit incremental
/// NDJSON progress events (e.g. long-running deployments with `--follow`).
#[derive(Clone)]
pub struct RuntimeCommandSpec {
    /// Declarative command metadata.
    pub spec: CommandSpec,
    /// Async command implementation.
    pub handler: CommandHandler,
    /// Optional streaming handler. When set, the engine writes NDJSON events
    /// to stdout as they arrive instead of collecting a single envelope.
    pub streaming_handler: Option<StreamingCommandHandler>,
}

impl std::fmt::Debug for RuntimeCommandSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeCommandSpec")
            .field("spec", &self.spec)
            .field("is_streaming", &self.streaming_handler.is_some())
            .finish_non_exhaustive()
    }
}

impl RuntimeCommandSpec {
    /// Creates a runtime command with the common handler shape.
    ///
    /// The handler receives a lazy [`CredentialResolver`] and the effective args.
    /// Call `resolver.resolve().await?` only when the command actually needs a
    /// credential; commands that ignore it never trigger an auth flow. The
    /// handler returns [`CommandResult`], where `data` must be JSON-serializable.
    #[must_use]
    pub fn new<F, Fut, Output>(spec: CommandSpec, handler: F) -> Self
    where
        F: Fn(CredentialResolver, ValueMap) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Output>> + Send + 'static,
        Output: Into<CommandResult> + Send + 'static,
    {
        Self {
            spec,
            streaming_handler: None,
            handler: Arc::new(move |context| {
                let future = handler(context.credential, context.args);
                Box::pin(async move { future.await.map(Into::into) })
            }),
        }
    }

    /// Creates a runtime command with the full invocation context.
    #[must_use]
    pub fn new_with_context<F, Fut, Output>(spec: CommandSpec, handler: F) -> Self
    where
        F: Fn(CommandContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Output>> + Send + 'static,
        Output: Into<CommandResult> + Send + 'static,
    {
        Self {
            spec,
            streaming_handler: None,
            handler: Arc::new(move |context| {
                let future = handler(context);
                Box::pin(async move { future.await.map(Into::into) })
            }),
        }
    }

    /// Creates a streaming command that emits NDJSON events to stdout.
    ///
    /// The handler receives context and a [`StreamSender`]. It should call
    /// `sender.send(event).await` for each progress event, then return `Ok(())`.
    /// The engine writes each event as a JSON line; stdout is flushed after each.
    #[must_use]
    pub fn new_streaming<F, Fut>(spec: CommandSpec, handler: F) -> Self
    where
        F: Fn(CommandContext, StreamSender) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let streaming: StreamingCommandHandler = Arc::new(move |context, sender| {
            let future = handler(context, sender);
            Box::pin(future)
        });
        Self {
            spec,
            streaming_handler: Some(streaming),
            handler: Arc::new(|_context| Box::pin(async { Ok(CommandResult::new(Value::Null)) })),
        }
    }

    /// Creates a runtime command with typed argument deserialization.
    ///
    /// The handler receives a lazy [`CredentialResolver`] and the deserialized
    /// args struct. Use with `CommandSpec::from_args::<T>()` to get end-to-end
    /// type safety from argument definition through handler consumption.
    ///
    /// If the handler also needs the command path, middleware, or user-supplied
    /// args, use [`RuntimeCommandSpec::new_with_context`] with
    /// [`CommandContext::typed_args`] instead.
    #[must_use]
    pub fn new_typed<T, F, Fut, Output>(spec: CommandSpec, handler: F) -> Self
    where
        T: clap::FromArgMatches + Send + 'static,
        F: Fn(CredentialResolver, T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Output>> + Send + 'static,
        Output: Into<CommandResult> + Send + 'static,
    {
        let handler = Arc::new(handler);
        Self {
            spec,
            handler: Arc::new(move |context| {
                let credential = context.credential.clone();
                let parsed = T::from_arg_matches(context.raw_matches.as_ref());
                let handler = handler.clone();
                Box::pin(async move {
                    let args = parsed.map_err(|e| {
                        crate::CliCoreError::Message(format!("argument parse error: {e}"))
                    })?;
                    handler(credential, args).await.map(Into::into)
                })
            }),
            streaming_handler: None,
        }
    }
}

/// Executable command group with runtime children.
#[derive(Clone, Debug, Default)]
pub struct RuntimeGroupSpec {
    /// Declarative group metadata.
    pub group: GroupSpec,
    /// Executable leaf commands under this group.
    pub commands: Vec<RuntimeCommandSpec>,
    /// Executable nested groups under this group.
    pub groups: Vec<RuntimeGroupSpec>,
}

impl RuntimeGroupSpec {
    /// Creates a runtime group from declarative group metadata.
    #[must_use]
    pub fn new(group: GroupSpec) -> Self {
        Self {
            group,
            ..Self::default()
        }
    }

    /// Adds one executable leaf command.
    #[must_use]
    pub fn with_command(mut self, command: RuntimeCommandSpec) -> Self {
        self.commands.push(command);
        self
    }

    /// Adds one executable nested group.
    #[must_use]
    pub fn with_group(mut self, group: RuntimeGroupSpec) -> Self {
        self.groups.push(group);
        self
    }

    /// Builds the `clap` command for parser registration.
    #[must_use]
    pub fn clap_command(&self) -> Command {
        let mut command = Command::new(self.group.name.clone()).about(self.group.short.clone());
        if let Some(long) = &self.group.long
            && !long.is_empty()
        {
            command = command.long_about(long.clone());
        }
        for alias in &self.group.aliases {
            command = command.alias(alias.clone());
        }
        if self.group.hidden {
            command = command.hide(true);
        }
        for group in &self.groups {
            command = command.subcommand(group.clap_command());
        }
        for child in &self.commands {
            command = command.subcommand(child.spec.clap_command());
        }
        command
    }

    pub(crate) fn register_commands(
        &self,
        prefix: &mut Vec<String>,
        out: &mut BTreeMap<String, RuntimeCommandSpec>,
    ) {
        prefix.push(self.group.name.clone());
        for group in &self.groups {
            group.register_commands(prefix, out);
        }
        for command in &self.commands {
            prefix.push(command.spec.name.clone());
            out.insert(prefix.join(":"), command.clone());
            prefix.pop();
        }
        prefix.pop();
    }
}

/// Extracts the colon-separated command path from parsed `clap` matches.
#[must_use]
pub fn command_path_from_matches(root_name: &str, matches: &ArgMatches) -> String {
    let mut parts = Vec::new();
    let mut current = matches;
    while let Some((name, submatches)) = current.subcommand() {
        if name != root_name {
            parts.push(name.to_owned());
        }
        current = submatches;
    }
    parts.join(":")
}

/// Builds a colon-separated command path from path parts.
///
/// The optional annotation is used only for isolated single-command tests.
#[must_use]
pub fn command_path_from_parts(parts: &[impl AsRef<str>], path_annotation: Option<&str>) -> String {
    if parts.is_empty() {
        return String::new();
    }
    if parts.len() > 1 {
        return parts[1..]
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(":");
    }
    path_annotation
        .filter(|annotation| !annotation.is_empty())
        .map_or_else(|| parts[0].as_ref().to_owned(), ToOwned::to_owned)
}

/// Returns the deepest subcommand matches.
#[must_use]
pub fn leaf_matches(matches: &ArgMatches) -> &ArgMatches {
    let mut current = matches;
    while let Some((_, submatches)) = current.subcommand() {
        current = submatches;
    }
    current
}

/// Converts parsed command arguments into the JSON-ish map consumed by middleware.
///
/// When `changed_only` is true, only arguments that came from the command line
/// are included. This is the user-args map used by authz and audit.
#[must_use]
pub fn command_args_from_matches(
    matches: &ArgMatches,
    spec: &CommandSpec,
    changed_only: bool,
) -> ValueMap {
    let mut args = ValueMap::new();
    for arg in &spec.args {
        let id = arg.get_id().to_string();
        let changed = matches
            .value_source(&id)
            .is_some_and(|source| source == clap::parser::ValueSource::CommandLine);
        if changed_only && !changed {
            continue;
        }
        if let Some(value) = arg_value_from_matches(matches, arg, &id) {
            args.insert(id, value);
        }
    }
    args
}

fn arg_value_from_matches(matches: &ArgMatches, flag: &Arg, id: &str) -> Option<Value> {
    matches.value_source(id)?;

    if matches!(flag.get_action(), ArgAction::SetTrue | ArgAction::SetFalse)
        && let Some(value) = matches.get_one::<bool>(id)
    {
        return Some(Value::Bool(*value));
    }

    if let Some(value) = typed_arg_value_from_matches(matches, id) {
        return Some(value);
    }

    if let Some(values) = matches.get_raw(id) {
        let rendered = values
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        return match rendered.as_slice() {
            [] => None,
            [single] => Some(Value::String(single.clone())),
            _ => Some(Value::Array(
                rendered.into_iter().map(Value::String).collect(),
            )),
        };
    }

    if let Some(value) = matches.get_one::<String>(id) {
        return Some(Value::String(value.clone()));
    }
    if let Some(value) = matches.get_one::<usize>(id) {
        return Some(serde_json::json!(value));
    }
    if let Some(value) = matches.get_one::<u64>(id) {
        return Some(serde_json::json!(value));
    }
    if let Some(value) = matches.get_one::<i64>(id) {
        return Some(serde_json::json!(value));
    }
    None
}

fn typed_arg_value_from_matches(matches: &ArgMatches, id: &str) -> Option<Value> {
    typed_values::<bool>(matches, id, Value::Bool)
        .or_else(|| typed_values::<i8>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<i16>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<i64>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<i32>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<u8>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<u16>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<u64>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<u32>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| {
            typed_values::<usize>(matches, id, |value| {
                u64::try_from(value).map_or(Value::Null, |value| Value::Number(value.into()))
            })
        })
        .or_else(|| {
            typed_values::<f64>(matches, id, |value| {
                Number::from_f64(value).map_or(Value::Null, Value::Number)
            })
        })
        .or_else(|| {
            typed_values::<f32>(matches, id, |value| {
                Number::from_f64(f64::from(value)).map_or(Value::Null, Value::Number)
            })
        })
        .or_else(|| typed_values::<String>(matches, id, Value::String))
}

fn typed_values<T>(matches: &ArgMatches, id: &str, to_value: impl Fn(T) -> Value) -> Option<Value>
where
    T: Clone + Send + Sync + 'static,
{
    let Ok(Some(values)) = matches.try_get_many::<T>(id) else {
        return None;
    };
    let values = values.cloned().map(to_value).collect::<Vec<_>>();
    match values.as_slice() {
        [] => None,
        [single] => Some(single.clone()),
        _ => Some(Value::Array(values)),
    }
}
