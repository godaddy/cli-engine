use std::future::Future;
use std::sync::Arc;

use serde_json::Value;

use super::{
    CommandContext, CommandHandler, CommandResult, CommandSpec, StreamSender,
    StreamingCommandHandler,
};
use crate::{CredentialResolver, Result, middleware::ValueMap};

/// Executable leaf command.
///
/// `RuntimeCommandSpec` pairs a [`CommandSpec`] with async business logic.
/// This split keeps metadata inspectable for help/search/schema generation
/// before the handler ever runs.
///
/// Use [`RuntimeCommandSpec::new_streaming`] for commands that emit incremental
/// NDJSON progress events (e.g. long-running deployments with `--follow`).
///
/// Construct with one of the `new*` constructors — never as a struct literal.
/// Literal construction would bypass the `handles_dry_run`/handler-shape
/// misuse checks those constructors debug-assert. `#[non_exhaustive]` also
/// means the engine can add fields without a breaking release.
#[derive(Clone)]
#[non_exhaustive]
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
    ///
    /// This handler shape has no [`CommandContext`], so it can never call
    /// [`CommandContext::dry_run`] — do not pair this with
    /// [`CommandSpec::handles_dry_run`] (debug-asserted; see that field's docs).
    #[must_use]
    pub fn new<F, Fut, Output>(spec: CommandSpec, handler: F) -> Self
    where
        F: Fn(CredentialResolver, ValueMap) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Output>> + Send + 'static,
        Output: Into<CommandResult> + Send + 'static,
    {
        debug_assert!(
            !spec.handles_dry_run,
            "command {:?} sets handles_dry_run but RuntimeCommandSpec::new's handler \
             (CredentialResolver, args) has no CommandContext and can never check \
             CommandContext::dry_run(), so it would silently run its real side effects \
             under --dry-run; use RuntimeCommandSpec::new_with_context (or \
             new_typed_with_context to keep typed args) instead",
            spec.name
        );
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
        debug_assert!(
            !spec.raw_output,
            "command {:?} sets raw_output but RuntimeCommandSpec::new_streaming writes \
             chunked NDJSON events, which does not fit a single-verbatim-string contract; \
             raw_output is only supported on non-streaming commands",
            spec.name
        );
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
    /// args, use [`RuntimeCommandSpec::new_typed_with_context`] (or
    /// [`RuntimeCommandSpec::new_with_context`] with
    /// [`CommandContext::typed_args`]) instead.
    ///
    /// This handler shape has no [`CommandContext`], so it can never call
    /// [`CommandContext::dry_run`] — do not pair this with
    /// [`CommandSpec::handles_dry_run`] (debug-asserted; see that field's docs).
    #[must_use]
    pub fn new_typed<T, F, Fut, Output>(spec: CommandSpec, handler: F) -> Self
    where
        T: clap::FromArgMatches + Send + 'static,
        F: Fn(CredentialResolver, T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Output>> + Send + 'static,
        Output: Into<CommandResult> + Send + 'static,
    {
        debug_assert!(
            !spec.handles_dry_run,
            "command {:?} sets handles_dry_run but RuntimeCommandSpec::new_typed's handler \
             (CredentialResolver, args) has no CommandContext and can never check \
             CommandContext::dry_run(), so it would silently run its real side effects \
             under --dry-run; use RuntimeCommandSpec::new_with_context (or \
             new_typed_with_context to keep typed args) instead",
            spec.name
        );
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

    /// Creates a runtime command with full context and typed argument
    /// deserialization.
    ///
    /// Combines [`new_with_context`](RuntimeCommandSpec::new_with_context)'s
    /// access to [`CommandContext`] (command path, middleware snapshot,
    /// user-supplied args, [`CommandContext::dry_run`]) with
    /// [`new_typed`](RuntimeCommandSpec::new_typed)'s automatic
    /// deserialization: the engine parses `T` from the raw matches before
    /// invoking the handler, so the handler never needs to call
    /// [`CommandContext::typed_args`] itself.
    ///
    /// Use this instead of `new_with_context` + `context.typed_args::<T>()`
    /// when a command needs full context and wants eager, guaranteed-parsed
    /// typed args rather than parsing on demand. Because the handler receives
    /// a [`CommandContext`], this is a valid pairing with
    /// [`CommandSpec::handles_dry_run`].
    ///
    /// # Errors
    ///
    /// The returned handler surfaces a `CliCoreError::Message` if `T` fails to
    /// deserialize from the parsed matches (this should not happen for args
    /// generated by `CommandSpec::from_args::<T>()`, since `clap` already
    /// validated them during parsing).
    #[must_use]
    pub fn new_typed_with_context<T, F, Fut, Output>(spec: CommandSpec, handler: F) -> Self
    where
        T: clap::FromArgMatches + Send + 'static,
        F: Fn(CommandContext, T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Output>> + Send + 'static,
        Output: Into<CommandResult> + Send + 'static,
    {
        let handler = Arc::new(handler);
        Self {
            spec,
            handler: Arc::new(move |context| {
                let parsed = T::from_arg_matches(context.raw_matches.as_ref());
                let handler = handler.clone();
                Box::pin(async move {
                    let args = parsed.map_err(|e| {
                        crate::CliCoreError::Message(format!("argument parse error: {e}"))
                    })?;
                    handler(context, args).await.map(Into::into)
                })
            }),
            streaming_handler: None,
        }
    }

    /// Creates a streaming command with full context and typed argument
    /// deserialization.
    ///
    /// Combines [`new_streaming`](RuntimeCommandSpec::new_streaming)'s NDJSON
    /// event emission with [`new_typed`](RuntimeCommandSpec::new_typed)'s
    /// automatic deserialization: the engine parses `T` from the raw matches
    /// before invoking the handler.
    ///
    /// # Errors
    ///
    /// The returned handler surfaces a `CliCoreError::Message` if `T` fails to
    /// deserialize from the parsed matches.
    #[must_use]
    pub fn new_typed_streaming<T, F, Fut>(spec: CommandSpec, handler: F) -> Self
    where
        T: clap::FromArgMatches + Send + 'static,
        F: Fn(CommandContext, T, StreamSender) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        debug_assert!(
            !spec.raw_output,
            "command {:?} sets raw_output but RuntimeCommandSpec::new_typed_streaming writes \
             chunked NDJSON events, which does not fit a single-verbatim-string contract; \
             raw_output is only supported on non-streaming commands",
            spec.name
        );
        let handler = Arc::new(handler);
        let streaming: StreamingCommandHandler = Arc::new(move |context, sender| {
            let parsed = T::from_arg_matches(context.raw_matches.as_ref());
            let handler = handler.clone();
            Box::pin(async move {
                let args = parsed.map_err(|e| {
                    crate::CliCoreError::Message(format!("argument parse error: {e}"))
                })?;
                handler(context, args, sender).await
            })
        });
        Self {
            spec,
            streaming_handler: Some(streaming),
            handler: Arc::new(|_context| Box::pin(async { Ok(CommandResult::new(Value::Null)) })),
        }
    }
}
