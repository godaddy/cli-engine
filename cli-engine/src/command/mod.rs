use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    Credential, CredentialResolver, Middleware, Result, middleware::ValueMap, output::NextAction,
};

mod group;
mod matches;
mod runtime;
mod spec;

pub use group::{GroupSpec, RuntimeGroupSpec};
pub use matches::{
    command_args_from_matches, command_path_from_matches, command_path_from_parts, leaf_matches,
};
pub use runtime::RuntimeCommandSpec;
pub use spec::{CommandSpec, PaginationConfig};

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
///
/// Construct with [`CommandResult::new`], then chain `with_*` methods —
/// never as a struct literal. `#[non_exhaustive]` enforces this so the engine
/// can add fields without a breaking release.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
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

    /// Marks this result as a dry-run preview outcome.
    ///
    /// Call only when the handler actually skipped its mutating step because
    /// [`CommandContext::dry_run`] was `true`. This requires the command to
    /// have opted in via [`CommandSpec::handles_dry_run`] — otherwise
    /// middleware never invokes the handler under `--dry-run` in the first
    /// place. Middleware tags the audit/activity outcome as `dry-run` instead
    /// of `ok` and marks the rendered envelope accordingly.
    #[must_use]
    pub fn with_dry_run(mut self) -> Self {
        self.metadata.dry_run = true;
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
    /// Set by [`CommandResult::with_dry_run`] when a
    /// [`handles_dry_run`](CommandSpec::handles_dry_run) handler skipped its
    /// mutating step. Middleware tags the audit/activity outcome and envelope
    /// as `dry-run` instead of `ok` when this is `true`.
    pub dry_run: bool,
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
    pub raw_matches: Arc<clap::ArgMatches>,
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

    /// Returns whether `--dry-run` was passed for this invocation.
    ///
    /// Only meaningful for commands that opted in via
    /// [`CommandSpec::handles_dry_run`] — other mutating commands never reach
    /// their handler under `--dry-run` at all, so there's nothing to branch
    /// on. An opted-in handler should run its real validation unconditionally
    /// and use this only to skip the actual mutating I/O, returning a preview
    /// result tagged with [`CommandResult::with_dry_run`].
    #[must_use]
    pub fn dry_run(&self) -> bool {
        self.middleware.dry_run
    }

    /// Returns the resolved interactivity mode for this invocation.
    ///
    /// Use this to decide whether to prompt for missing inputs, show progress
    /// spinners, or offer interactive choices. When `false`, the command should
    /// fail with a descriptive error if required inputs are missing.
    #[must_use]
    pub fn is_interactive(&self) -> bool {
        self.middleware.interactive
    }

    /// Returns the resolved [`InteractivityMode`](crate::InteractivityMode).
    ///
    /// Equivalent to [`is_interactive`](Self::is_interactive) but returns the
    /// enum for pattern matching.
    #[must_use]
    pub fn interactivity_mode(&self) -> crate::InteractivityMode {
        self.middleware.interactive.into()
    }

    /// Resolves the active environment's merged TOML table for this
    /// invocation, as an [`EnvSource`](crate::env_config::EnvSource).
    ///
    /// The active environment name is `self.middleware.env`, seeded at startup
    /// from the persisted active environment or configured default and
    /// overridden per invocation by the global `--env` flag. Resolution merges
    /// the compiled-in table and the `environments.toml` file layer (file
    /// wins). Use this for generic introspection (see the built-in `env info`
    /// command); for a typed section with the app-scoped environment-variable
    /// override tier applied, use
    /// [`environment_config`](Self::environment_config) instead.
    ///
    /// # Blocking
    ///
    /// When the `environments.toml` file layer is enabled, this performs
    /// synchronous filesystem I/O via
    /// [`Environments::source`](crate::environments::Environments::source).
    /// Call it once per invocation and reuse the result rather than calling it
    /// repeatedly inside an async handler on a latency-sensitive path.
    ///
    /// # Errors
    ///
    /// Returns an error if no environment system was registered via
    /// [`CliConfig::with_environments`](crate::CliConfig::with_environments) or
    /// if the active name does not resolve to a known environment.
    pub fn environment(&self) -> Result<crate::env_config::EnvSource> {
        let environments = self.middleware.environments.as_ref().ok_or_else(|| {
            crate::error::CliCoreError::message("no environment system configured")
        })?;
        environments.source(&self.middleware.env)
    }

    /// Resolves the active environment into a typed
    /// [`EnvConfig`](crate::env_config::EnvConfig) section, with the
    /// app-scoped environment-variable override tier applied (see
    /// [`Environments::resolve`](crate::environments::Environments::resolve)).
    ///
    /// # Blocking
    ///
    /// See [`environment`](Self::environment).
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as
    /// [`environment`](Self::environment), or when a field's present value
    /// fails to convert to its type, or a required field has no value in any
    /// source and no default.
    pub fn environment_config<T: crate::env_config::EnvConfig>(
        &self,
    ) -> std::result::Result<T, crate::env_config::EnvConfigError> {
        let environments = self.middleware.environments.as_ref().ok_or_else(|| {
            crate::error::CliCoreError::message("no environment system configured")
        })?;
        environments.resolve(&self.middleware.env)
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
