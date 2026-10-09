//! Client attribution: how a CLI tells the services it calls what kind of
//! caller it is, without any telemetry channel of its own.
//!
//! Everything here rides on requests the user already asked for:
//!
//! - **User-Agent tokens** — `mode/<agent|ci|interactive|script>` and, for a
//!   detected AI harness, `agent/<slug>`.
//! - **A correlation header** — when the harness exposes a session id, a salted
//!   hash of it (never the raw id) so a service can group one session's calls.
//!
//! Detection is cooperative and heuristic: it reads environment markers that
//! harnesses publish to their subprocesses. A match does not prove a model
//! issued the command, and no match does not prove a human did.

use std::{collections::BTreeMap, path::Path};

use sha2::{Digest, Sha256};

use crate::flags::{app_id_env_prefix, detect_interactive};

const DEFAULT_SESSION_HEADER: &str = "x-client-session";
const SESSION_HASH_BYTES: usize = 8;

/// Opts a CLI into client attribution: `mode/<agent|ci|interactive|script>`
/// and `agent/<slug>` tokens on the User-Agent, plus a correlation header
/// carrying a salted hash of the harness session id when one is available.
/// The raw session id is never sent. See `docs/attribution.md` for the full
/// user-facing description.
///
/// Attribution is off unless a CLI calls
/// [`CliConfig::with_client_attribution`](crate::CliConfig::with_client_attribution).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttributionConfig {
    session_header: String,
    send_session: bool,
}

impl Default for AttributionConfig {
    fn default() -> Self {
        Self {
            session_header: DEFAULT_SESSION_HEADER.to_owned(),
            send_session: true,
        }
    }
}

impl AttributionConfig {
    /// Creates the default configuration: user-agent tokens plus a hashed
    /// session id in the `x-client-session` header when a harness supplies one.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the request header that carries the hashed session id, for services
    /// that expect a product-specific name (for example `x-gddy-session`).
    ///
    /// An invalid header name disables the session header rather than failing
    /// startup; the user-agent tokens are unaffected.
    #[must_use]
    pub fn with_session_header(mut self, name: impl Into<String>) -> Self {
        self.session_header = name.into().to_ascii_lowercase();
        self
    }

    /// Never sends a session header, only the user-agent tokens.
    #[must_use]
    pub fn without_session_id(mut self) -> Self {
        self.send_session = false;
        self
    }
}

/// Coarse caller classification, in precedence order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientMode {
    /// A known AI harness marker is present.
    Agent,
    /// `CI` is set.
    Ci,
    /// A person at a terminal.
    Interactive,
    /// Neither: a script, cron job, or other unattended caller.
    Script,
}

impl ClientMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Ci => "ci",
            Self::Interactive => "interactive",
            Self::Script => "script",
        }
    }
}

/// A detected AI harness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentSignal {
    slug: &'static str,
    session_id: Option<String>,
}

/// What the process environment says about the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Signals {
    agent: Option<AgentSignal>,
    ci: bool,
    interactive: bool,
    session_opted_out: bool,
}

impl Signals {
    /// Reads the live process environment and terminal state.
    pub(crate) fn from_process(app_id: &str) -> Self {
        Self::from_lookup(
            app_id,
            |name| std::env::var(name).ok(),
            |path| Path::new(path).exists(),
            detect_interactive(),
        )
    }

    /// Builds signals from injected lookups so tests never touch the real
    /// environment.
    pub(crate) fn from_lookup(
        app_id: &str,
        env: impl Fn(&str) -> Option<String>,
        file_exists: impl Fn(&str) -> bool,
        interactive: bool,
    ) -> Self {
        let agent = is_ai_agent::detect_with(&env, file_exists).map(|agent| AgentSignal {
            slug: agent.id.as_str(),
            session_id: agent.session_id,
        });
        let truthy = |name: &str| env(name).is_some_and(|value| is_truthy(&value));
        Self {
            agent,
            ci: truthy("CI"),
            interactive,
            session_opted_out: truthy(&format!("{}_NO_SESSION_ID", app_id_env_prefix(app_id))),
        }
    }

    fn mode(&self) -> ClientMode {
        if self.agent.is_some() {
            ClientMode::Agent
        } else if self.ci {
            ClientMode::Ci
        } else if self.interactive {
            ClientMode::Interactive
        } else {
            ClientMode::Script
        }
    }
}

/// The resolved outbound identity additions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Attribution {
    /// Space-prefixed product tokens to append to the base user-agent.
    pub(crate) user_agent_suffix: String,
    /// Headers to send on every outbound request.
    pub(crate) headers: BTreeMap<String, String>,
}

impl Attribution {
    pub(crate) fn resolve(config: &AttributionConfig, app_id: &str, signals: &Signals) -> Self {
        let mode = signals.mode();
        let mut user_agent_suffix = format!(" mode/{}", mode.as_str());
        if let Some(agent) = &signals.agent {
            user_agent_suffix.push_str(" agent/");
            user_agent_suffix.push_str(agent.slug);
        }

        let mut headers = BTreeMap::new();
        if config.send_session
            && !signals.session_opted_out
            && is_valid_header_name(&config.session_header)
            && let Some(agent) = &signals.agent
            && let Some(session_id) = &agent.session_id
        {
            headers.insert(
                config.session_header.clone(),
                hash_session(app_id, agent.slug, session_id),
            );
        }
        Self {
            user_agent_suffix,
            headers,
        }
    }
}

/// Salted, truncated SHA-256 of the harness session id.
///
/// Harness ids are often meaningful on the user's machine (transcript file
/// names, resume handles), so the raw value never leaves the process. The app
/// id and agent slug salt it so the same id hashes differently per CLI and the
/// result cannot be joined across unrelated products.
fn hash_session(app_id: &str, agent_slug: &str, session_id: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [app_id, agent_slug, session_id] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .take(SESSION_HASH_BYTES)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_valid_header_name(name: &str) -> bool {
    reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_ok()
}

/// Env-flag semantics: set, non-blank, and not an explicit "off" spelling.
fn is_truthy(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !["0", "false", "no", "off"]
            .iter()
            .any(|off| value.eq_ignore_ascii_case(off))
}

#[cfg(test)]
mod tests;
