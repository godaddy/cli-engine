use std::borrow::Cow;

use thiserror::Error;

/// Crate-wide result type.
pub type Result<T> = std::result::Result<T, CliCoreError>;

/// Error trait for values that carry a process exit code.
pub trait ExitCoder {
    /// Returns the process-style exit code for the error.
    fn exit_code(&self) -> i32;
}

/// Error trait for values that carry structured output-envelope metadata.
pub trait DetailedError: std::error::Error {
    /// Stable error code.
    fn error_code(&self) -> Cow<'static, str>;
    /// Optional backend/system id.
    fn error_system(&self) -> Option<Cow<'static, str>>;
    /// Optional backend request id.
    fn error_request_id(&self) -> Option<Cow<'static, str>>;
    /// Optional recovery hint for the envelope's top-level `fix` (defaults to [`None`]).
    fn error_fix(&self) -> Option<Cow<'static, str>> {
        None
    }
}

/// Framework error type.
#[derive(Debug, Error)]
pub enum CliCoreError {
    /// Requested auth provider has not been registered.
    #[error("auth: no provider registered with name {0:?}")]
    MissingAuthProvider(String),
    /// Auth provider failed.
    #[error("auth: provider {provider:?}: {source}")]
    AuthProvider {
        /// Provider name.
        provider: String,
        /// Source error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Output format is not supported.
    #[error("invalid output format {0:?}: must be one of toon, json, human")]
    InvalidOutputFormat(String),
    /// Plain message error.
    #[error("{0}")]
    Message(String),
    /// Structured message with explicit envelope metadata.
    #[error("{message}")]
    SystemMessage {
        /// Error message.
        message: String,
        /// Backend/system id.
        system: String,
        /// Stable error code.
        code: String,
        /// Optional request id.
        request_id: String,
    },
    /// Wrapped source error with backend/system attribution.
    #[error("{source}")]
    System {
        /// Backend/system id.
        system: String,
        /// Source error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Wrapped source error with structured metadata captured up front.
    #[error("{source}")]
    Detailed {
        /// Stable error code.
        code: String,
        /// Backend/system id.
        system: String,
        /// Optional request id.
        request_id: String,
        /// Source error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Wrapped source error with explicit process exit code.
    #[error("{source}")]
    ExitCode {
        /// Process-style exit code.
        code: i32,
        /// Source error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Wrapped source error with a recovery hint for the output envelope.
    #[error("{source}")]
    Fix {
        /// Recovery guidance shown as the envelope's top-level `fix`.
        fix: String,
        /// Source error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// IO error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON serialization or decoding error.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Structured HTTP transport error.
    #[error(transparent)]
    Transport(#[from] crate::transport::Error),
}

impl CliCoreError {
    /// Creates a plain message error.
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    /// Creates a structured message attributed to a backend/system id.
    #[must_use]
    pub fn message_for_system(system: impl Into<String>, message: impl Into<String>) -> Self {
        Self::SystemMessage {
            message: message.into(),
            system: system.into(),
            code: "ERROR".to_owned(),
            request_id: String::new(),
        }
    }

    /// Wraps a source error with backend/system attribution.
    #[must_use]
    pub fn with_system(
        system: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::System {
            system: system.into(),
            source: Box::new(source),
        }
    }

    /// Wraps a source error with an explicit process exit code.
    #[must_use]
    pub fn with_exit_code(
        code: i32,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::ExitCode {
            code,
            source: Box::new(source),
        }
    }

    /// Wraps a source error with a recovery hint for the output envelope.
    ///
    /// Empty hints do not wrap: a [`CliCoreError`] source is returned as-is.
    #[must_use]
    pub fn with_fix(
        fix: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        let fix = fix.into();
        if fix.is_empty() {
            let source: Box<dyn std::error::Error + Send + Sync> = Box::new(source);
            return match source.downcast::<Self>() {
                Ok(inner) => *inner,
                Err(source) => Self::Message(source.to_string()),
            };
        }
        Self::Fix {
            fix,
            source: Box::new(source),
        }
    }

    /// Captures structured metadata from a detailed source error.
    #[must_use]
    pub fn with_detailed_error(source: impl DetailedError + Send + Sync + 'static) -> Self {
        let code = source.error_code().into_owned();
        let system = source
            .error_system()
            .map_or_else(String::new, Cow::into_owned);
        let request_id = source
            .error_request_id()
            .map_or_else(String::new, Cow::into_owned);
        let fix = source.error_fix().map_or_else(String::new, Cow::into_owned);
        Self::with_fix(
            fix,
            Self::Detailed {
                code,
                system,
                request_id,
                source: Box::new(source),
            },
        )
    }

    /// Reports whether this error originates from credential resolution.
    ///
    /// True for [`MissingAuthProvider`](Self::MissingAuthProvider) and
    /// [`AuthProvider`](Self::AuthProvider), including when those variants are
    /// wrapped by [`Fix`](Self::Fix) or [`ExitCode`](Self::ExitCode). The engine
    /// uses this to classify a command outcome as `auth-error` rather than a
    /// generic command error, based on the error a handler actually returns — so
    /// a handler that swallows a resolution failure and then fails for another
    /// reason is not misclassified.
    #[must_use]
    pub fn is_auth(&self) -> bool {
        match self {
            Self::MissingAuthProvider(_) | Self::AuthProvider { .. } => true,
            Self::ExitCode { source, .. } | Self::Fix { source, .. } => {
                source.downcast_ref::<Self>().is_some_and(Self::is_auth)
            }
            _ => false,
        }
    }

    /// Returns backend/system attribution when the error carries one.
    ///
    /// [`Fix`](Self::Fix) / [`ExitCode`](Self::ExitCode) wrappers delegate to their source.
    #[must_use]
    pub fn system(&self) -> Option<&str> {
        match self {
            Self::SystemMessage { system, .. }
            | Self::System { system, .. }
            | Self::Detailed { system, .. }
                if !system.is_empty() =>
            {
                Some(system)
            }
            Self::ExitCode { source, .. } | Self::Fix { source, .. } => {
                source.downcast_ref::<Self>().and_then(Self::system)
            }
            Self::MissingAuthProvider(_)
            | Self::AuthProvider { .. }
            | Self::InvalidOutputFormat(_)
            | Self::Message(_)
            | Self::SystemMessage { .. }
            | Self::System { .. }
            | Self::Detailed { .. }
            | Self::Io(_)
            | Self::Json(_)
            | Self::Transport(_) => None,
        }
    }
}

impl ExitCoder for CliCoreError {
    fn exit_code(&self) -> i32 {
        exit_code_for_error(self)
    }
}

/// Returns the exit code carried by an [`ExitCoder`].
#[must_use]
pub fn exit_code_for_exit_coder(err: &dyn ExitCoder) -> i32 {
    err.exit_code()
}

/// Maps an error chain to the framework's process-style exit code.
#[must_use]
pub fn exit_code_for_error(err: &(dyn std::error::Error + 'static)) -> i32 {
    let mut current = Some(err);
    while let Some(error) = current {
        if let Some(CliCoreError::ExitCode { code, .. }) = error.downcast_ref::<CliCoreError>() {
            return *code;
        }
        current = error.source();
    }

    let mut current = Some(err);
    while let Some(error) = current {
        if let Some(cli_err) = error.downcast_ref::<CliCoreError>() {
            match cli_err {
                CliCoreError::MissingAuthProvider(_) | CliCoreError::AuthProvider { .. } => {
                    return 2;
                }
                CliCoreError::InvalidOutputFormat(_) => return 3,
                CliCoreError::System { .. }
                | CliCoreError::Detailed { .. }
                | CliCoreError::ExitCode { .. }
                | CliCoreError::Fix { .. }
                | CliCoreError::Message(_)
                | CliCoreError::SystemMessage { .. }
                | CliCoreError::Io(_)
                | CliCoreError::Json(_)
                | CliCoreError::Transport(_) => {}
            }
        }
        current = error.source();
    }

    let msg = err.to_string().to_lowercase();
    if msg.contains("auth") {
        2
    } else if msg.contains("validation") || msg.contains("invalid") {
        3
    } else if msg.contains("not found") {
        4
    } else if msg.contains("permission") || msg.contains("forbidden") {
        5
    } else if msg.contains("denied") {
        6
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_walks_through_fix_and_exit_code_wrappers() {
        let err = CliCoreError::with_exit_code(
            2,
            CliCoreError::with_fix(
                "Run auth login",
                CliCoreError::message_for_system("auth", "not logged in"),
            ),
        );
        assert_eq!(err.system(), Some("auth"));
    }

    #[test]
    fn with_detailed_error_fix_preserves_system() {
        #[derive(Debug, thiserror::Error)]
        #[error("not logged in")]
        struct AuthRequired;

        impl DetailedError for AuthRequired {
            fn error_code(&self) -> Cow<'static, str> {
                Cow::Borrowed("AUTH_REQUIRED")
            }

            fn error_system(&self) -> Option<Cow<'static, str>> {
                Some(Cow::Borrowed("auth"))
            }

            fn error_request_id(&self) -> Option<Cow<'static, str>> {
                None
            }

            fn error_fix(&self) -> Option<Cow<'static, str>> {
                Some(Cow::Borrowed("Run auth login"))
            }
        }

        let err = CliCoreError::with_detailed_error(AuthRequired);
        assert!(matches!(err, CliCoreError::Fix { .. }));
        assert_eq!(err.system(), Some("auth"));
    }

    #[test]
    fn empty_with_fix_does_not_wrap() {
        let inner = CliCoreError::message_for_system("auth", "not logged in");
        let err = CliCoreError::with_fix("", inner);
        assert!(matches!(err, CliCoreError::SystemMessage { .. }));
        assert_eq!(err.system(), Some("auth"));
        assert!(!matches!(err, CliCoreError::Fix { .. }));
    }

    #[test]
    fn is_auth_walks_through_fix_and_exit_code_wrappers() {
        let err = CliCoreError::with_exit_code(
            2,
            CliCoreError::with_fix(
                "Run auth login",
                CliCoreError::MissingAuthProvider("primary".to_owned()),
            ),
        );
        assert!(err.is_auth());
        assert!(!CliCoreError::with_fix("hint", CliCoreError::message("boom")).is_auth());
    }
}
