use std::{collections::HashMap, time::Duration};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::DetailedError;

/// Top-level output envelope rendered for successful and failed commands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Successful command data.
    #[serde(skip_serializing_if = "is_absent_or_null")]
    pub data: Option<Value>,
    /// Optional execution metadata, controlled by `--verbose`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Structured error information for failed commands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorEnvelope>,
    /// Non-fatal warnings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Suggested follow-up actions for the caller (agent or human).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<NextAction>,
    #[serde(default, skip)]
    serialization_error: Option<String>,
}

/// A suggested follow-up command the caller can run next.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NextAction {
    /// Executable command template, e.g. `"application info --name {{name}}"`.
    pub command: String,
    /// Human-readable description of what this action does.
    pub description: String,
    /// Optional parameter hints for agent-driven invocation.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub params: HashMap<String, NextActionParam>,
}

impl NextAction {
    /// Creates a next action with a command template and description.
    #[must_use]
    pub fn new(command: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            description: description.into(),
            params: HashMap::new(),
        }
    }

    /// Adds a parameter hint.
    #[must_use]
    pub fn with_param(mut self, name: impl Into<String>, param: NextActionParam) -> Self {
        self.params.insert(name.into(), param);
        self
    }
}

/// Metadata hint for a parameter in a [`NextAction`] command template.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct NextActionParam {
    /// Concrete value to substitute, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Allowed values for enumeration parameters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub r#enum: Vec<String>,
    /// Whether the parameter is required.
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    /// Default value when none is supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Human-readable description of this parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl NextActionParam {
    /// Creates a parameter hint with a known concrete value.
    #[must_use]
    pub fn value(value: impl Into<String>) -> Self {
        Self {
            value: Some(value.into()),
            ..Self::default()
        }
    }

    /// Creates a required parameter hint.
    #[must_use]
    pub fn required() -> Self {
        Self {
            required: true,
            ..Self::default()
        }
    }
}

/// Execution metadata attached to an [`Envelope`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    /// Backend/system id.
    pub system: String,
    /// UTC timestamp in RFC3339 seconds format.
    pub timestamp: String,
    /// Optional backend request id.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request_id: String,
    /// Whether the command was a dry-run response.
    #[serde(skip_serializing_if = "is_false")]
    pub dry_run: bool,
    /// Pagination metadata when client-side pagination ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pagination: Option<PaginationMeta>,
    /// Colon-separated command path.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// Rounded command duration.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub duration: String,
    /// Selected environment.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub env: String,
    /// Authenticated identity.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub identity: String,
    /// User-supplied args.
    #[serde(skip_serializing_if = "is_absent_null_or_empty_object")]
    pub args: Option<Value>,
    /// Effective args after defaults and middleware injection.
    #[serde(skip_serializing_if = "is_absent_null_or_empty_object")]
    pub effective_args: Option<Value>,
}

/// Client-side pagination metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaginationMeta {
    /// Total list items before pagination.
    pub total: i64,
    /// Applied offset.
    pub offset: i64,
    /// Applied limit.
    pub limit: i64,
    /// Item count after pagination.
    pub count: i64,
}

/// Structured error payload in an [`Envelope`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    /// Stable error code.
    pub code: String,
    /// Human-readable error message.
    pub message: String,
    /// Optional backend/system id.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub system: String,
    /// Optional backend request id.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request_id: String,
}

impl Envelope {
    /// Creates a success envelope from serializable data.
    #[must_use]
    pub fn success(data: impl Serialize, system: impl Into<String>) -> Self {
        let (data, serialization_error) = match serde_json::to_value(data) {
            Ok(data) => (Some(data), None),
            Err(err) => (None, Some(err.to_string())),
        };
        Self {
            data,
            metadata: Some(Metadata::new(system)),
            error: None,
            warnings: Vec::new(),
            next_actions: Vec::new(),
            serialization_error,
        }
    }

    /// Creates a generic error envelope.
    #[must_use]
    pub fn error(
        code: impl Into<String>,
        message: impl Into<String>,
        system: impl Into<String>,
    ) -> Self {
        let system = system.into();
        Self {
            data: None,
            metadata: Some(Metadata::new(system.clone())),
            error: Some(ErrorEnvelope {
                code: code.into(),
                message: message.into(),
                system,
                request_id: String::new(),
            }),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            serialization_error: None,
        }
    }

    /// Creates a structured error envelope with request id.
    #[must_use]
    pub fn error_detail(
        code: impl Into<String>,
        message: impl Into<String>,
        system: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
        let system = system.into();
        let request_id = request_id.into();
        Self {
            data: None,
            metadata: Some(Metadata {
                request_id: request_id.clone(),
                ..Metadata::new(system.clone())
            }),
            error: Some(ErrorEnvelope {
                code: code.into(),
                message: message.into(),
                system,
                request_id,
            }),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            serialization_error: None,
        }
    }

    /// Attaches suggested follow-up actions.
    #[must_use]
    pub fn with_next_actions(mut self, actions: Vec<NextAction>) -> Self {
        self.next_actions = actions;
        self
    }

    /// Marks the envelope as a dry-run response.
    #[must_use]
    pub fn with_dry_run(mut self) -> Self {
        if let Some(metadata) = &mut self.metadata {
            metadata.dry_run = true;
        }
        self
    }

    /// Adds command execution context to envelope metadata.
    pub fn with_context(
        &mut self,
        command: &str,
        env: &str,
        identity: &str,
        duration: Duration,
        user_args: Option<Value>,
        effective_args: Option<Value>,
    ) {
        if let Some(metadata) = &mut self.metadata {
            metadata.command = command.to_owned();
            metadata.env = env.to_owned();
            metadata.identity = identity.to_owned();
            metadata.duration = format_duration(duration);
            metadata.args = user_args;
            metadata.effective_args = effective_args;
        }
    }

    /// Returns a copy with metadata stripped or filtered according to `--verbose`.
    #[must_use]
    pub fn prepare_for_render(&self, verbose: &str) -> Self {
        let mut copy = self.clone();
        if verbose.is_empty() {
            copy.metadata = None;
            return copy;
        }
        if verbose == "all" {
            return copy;
        }
        if let Some(metadata) = &self.metadata {
            copy.metadata = Some(metadata.filter_fields(verbose));
        }
        copy
    }

    /// Appends a non-fatal warning.
    pub fn add_warning(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    pub(crate) fn serialization_result(&self) -> crate::Result<()> {
        if let Some(error) = &self.serialization_error {
            return Err(crate::CliCoreError::message(error.clone()));
        }
        Ok(())
    }
}

impl Metadata {
    /// Creates metadata with system and timestamp.
    #[must_use]
    pub fn new(system: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            request_id: String::new(),
            dry_run: false,
            pagination: None,
            command: String::new(),
            duration: String::new(),
            env: String::new(),
            identity: String::new(),
            args: None,
            effective_args: None,
        }
    }

    fn filter_fields(&self, verbose: &str) -> Self {
        let wanted = verbose
            .split(',')
            .map(str::trim)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        Self {
            system: keep_string(&wanted, "system", &self.system),
            timestamp: keep_string(&wanted, "timestamp", &self.timestamp),
            request_id: keep_string(&wanted, "request_id", &self.request_id),
            dry_run: wanted.contains(&"dry_run") && self.dry_run,
            pagination: wanted
                .contains(&"pagination")
                .then(|| self.pagination.clone())
                .flatten(),
            command: keep_string(&wanted, "command", &self.command),
            duration: keep_string(&wanted, "duration", &self.duration),
            env: keep_string(&wanted, "env", &self.env),
            identity: keep_string(&wanted, "identity", &self.identity),
            args: wanted
                .contains(&"args")
                .then(|| self.args.clone())
                .flatten(),
            effective_args: wanted
                .contains(&"effective_args")
                .then(|| self.effective_args.clone())
                .flatten(),
        }
    }
}

/// Builds an error envelope, preserving structured details from known error types.
#[must_use]
pub fn build_error_envelope(err: &(dyn std::error::Error + 'static), system: &str) -> Envelope {
    if let Some((code, mut sys, request_id)) = find_detailed_error(err) {
        if sys.is_empty() {
            sys = system.to_owned();
        }
        return Envelope {
            data: None,
            metadata: Some(Metadata {
                request_id: request_id.clone(),
                ..Metadata::new(sys.clone())
            }),
            error: Some(ErrorEnvelope {
                code: if code.is_empty() {
                    "ERROR".to_owned()
                } else {
                    code
                },
                message: err.to_string(),
                system: sys,
                request_id,
            }),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            serialization_error: None,
        };
    }
    Envelope::error("ERROR", err.to_string(), system)
}

fn find_detailed_error(
    err: &(dyn std::error::Error + 'static),
) -> Option<(String, String, String)> {
    let mut current = Some(err);
    let mut fallback_system = None::<String>;
    while let Some(error) = current {
        if let Some(crate::CliCoreError::SystemMessage {
            system,
            code,
            request_id,
            ..
        }) = error.downcast_ref::<crate::CliCoreError>()
        {
            return Some((code.clone(), system.clone(), request_id.clone()));
        }
        if let Some(crate::CliCoreError::System { system, .. }) =
            error.downcast_ref::<crate::CliCoreError>()
            && !system.is_empty()
            && fallback_system.is_none()
        {
            fallback_system = Some(system.clone());
        }
        if let Some(crate::CliCoreError::Detailed {
            code,
            system,
            request_id,
            ..
        }) = error.downcast_ref::<crate::CliCoreError>()
        {
            return Some((
                code.clone(),
                fallback_system
                    .clone()
                    .filter(|_| system.is_empty())
                    .unwrap_or_else(|| system.clone()),
                request_id.clone(),
            ));
        }
        let detailed_transport = error.downcast_ref::<crate::transport::Error>().or_else(|| {
            match error.downcast_ref::<crate::CliCoreError>() {
                Some(crate::CliCoreError::Transport(transport)) => Some(transport),
                Some(
                    crate::CliCoreError::MissingAuthProvider(_)
                    | crate::CliCoreError::AuthProvider { .. }
                    | crate::CliCoreError::InvalidOutputFormat(_)
                    | crate::CliCoreError::Message(_)
                    | crate::CliCoreError::SystemMessage { .. }
                    | crate::CliCoreError::System { .. }
                    | crate::CliCoreError::Detailed { .. }
                    | crate::CliCoreError::ExitCode { .. }
                    | crate::CliCoreError::Io(_)
                    | crate::CliCoreError::Json(_),
                )
                | None => None,
            }
        });
        if let Some(detailed) = detailed_transport {
            let system = detailed
                .error_system()
                .map_or_else(String::new, std::borrow::Cow::into_owned);
            return Some((
                detailed.error_code().into_owned(),
                fallback_system
                    .clone()
                    .filter(|_| system.is_empty())
                    .unwrap_or(system),
                detailed
                    .error_request_id()
                    .map_or_else(String::new, std::borrow::Cow::into_owned),
            ));
        }
        current = error.source();
    }
    fallback_system.map(|system| ("ERROR".to_owned(), system, String::new()))
}

/// Builds an error envelope from a [`DetailedError`].
#[must_use]
pub fn build_detailed_error_envelope(err: &dyn DetailedError, system: &str) -> Envelope {
    let code = err.error_code().into_owned();
    let sys = err
        .error_system()
        .map_or_else(|| system.to_owned(), std::borrow::Cow::into_owned);
    let request_id = err
        .error_request_id()
        .map_or_else(String::new, std::borrow::Cow::into_owned);
    Envelope {
        data: None,
        metadata: Some(Metadata {
            request_id: request_id.clone(),
            ..Metadata::new(sys.clone())
        }),
        error: Some(ErrorEnvelope {
            code: if code.is_empty() {
                "ERROR".to_owned()
            } else {
                code
            },
            message: err.to_string(),
            system: sys,
            request_id,
        }),
        warnings: Vec::new(),
        next_actions: Vec::new(),
        serialization_error: None,
    }
}

fn keep_string(wanted: &[&str], field: &str, value: &str) -> String {
    if wanted.contains(&field) {
        value.to_owned()
    } else {
        String::new()
    }
}

fn format_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    let millis = (nanos + 500_000) / 1_000_000;
    if millis == 0 {
        return "0s".to_owned();
    }
    if millis >= 1000 {
        let secs = millis / 1000;
        let rem = millis % 1000;
        if rem == 0 {
            format!("{secs}s")
        } else {
            let mut fraction = format!("{rem:03}");
            while fraction.ends_with('0') {
                fraction.pop();
            }
            format!("{secs}.{fraction}s")
        }
    } else {
        format!("{millis}ms")
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

fn is_absent_or_null(value: &Option<Value>) -> bool {
    value.as_ref().is_none_or(Value::is_null)
}

fn is_absent_null_or_empty_object(value: &Option<Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::Object(map)) => map.is_empty(),
        Some(Value::Array(_) | Value::Bool(_) | Value::Number(_) | Value::String(_)) => false,
    }
}
