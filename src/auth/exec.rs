use std::{io::ErrorKind, path::PathBuf, process::Stdio, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command, time};

use super::{AuthProvider, Credential};
use crate::{CliCoreError, Result};

/// Provider action requesting a credential.
pub const ACTION_AUTHENTICATE: &str = "authenticate";
/// Provider action requesting cached credential status.
pub const ACTION_STATUS: &str = "status";
/// Provider action clearing cached credentials.
pub const ACTION_LOGOUT: &str = "logout";
/// Provider action listing cached environments.
pub const ACTION_LIST_ENVIRONMENTS: &str = "list-environments";
/// Legacy provider action listing cached realms.
pub const ACTION_LIST_REALMS: &str = "list-realms";

/// JSON payload sent to an external auth provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthnRequest {
    /// Provider action.
    pub action: String,
    /// Provider name.
    pub provider: String,
    /// Environment name.
    pub env: String,
    /// Deprecated alias of `env` kept for older provider binaries.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub realm: String,
    /// Colon-separated command path.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// Risk tier.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tier: String,
}

/// JSON payload returned by providers for `list-environments`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentsResponse {
    /// Environment names with cached credentials.
    pub environments: Vec<String>,
}

/// Auth provider implemented by spawning an external provider command.
///
/// The provider receives [`AuthnRequest`] JSON on stdin and returns credential
/// JSON on stdout. This keeps auth flows language-agnostic and easy to test.
#[derive(Clone, Debug)]
pub struct ExecProvider {
    provider_name: String,
    command: PathBuf,
    args: Vec<String>,
    timeout: Option<Duration>,
}

impl ExecProvider {
    /// Creates an exec provider with no extra arguments or timeout.
    #[must_use]
    pub fn new(provider_name: impl Into<String>, command: impl Into<PathBuf>) -> Self {
        Self {
            provider_name: provider_name.into(),
            command: command.into(),
            args: Vec::new(),
            timeout: None,
        }
    }

    /// Adds extra command-line arguments passed to the provider binary.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Sets a provider process timeout. A zero duration disables the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = (!timeout.is_zero()).then_some(timeout);
        self
    }

    /// Executes an arbitrary provider request and decodes a credential response.
    pub async fn exec_with_request(&self, request: &AuthnRequest) -> Result<Credential> {
        let out = self.exec_raw(request).await?;
        serde_json::from_slice(&out).map_err(|err| {
            CliCoreError::message(format!(
                "auth: parse credential from {}: {err}",
                self.command.display()
            ))
        })
    }

    async fn exec_action(&self, request: &AuthnRequest) -> Result<Vec<u8>> {
        self.exec_raw(request).await
    }

    async fn exec_raw(&self, request: &AuthnRequest) -> Result<Vec<u8>> {
        let request_json = serde_json::to_vec(request)?;
        let mut command = Command::new(&self.command);
        command
            .args(&self.args)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|err| self.exec_error(err, ""))?;
        let Some(mut stdin) = child.stdin.take() else {
            return Err(CliCoreError::message("auth: provider stdin unavailable"));
        };
        if let Err(err) = stdin.write_all(&request_json).await
            && err.kind() != ErrorKind::BrokenPipe
        {
            return Err(self.exec_error(err, ""));
        }
        drop(stdin);

        let output_fut = child.wait_with_output();
        let output = if let Some(timeout) = self.timeout {
            match time::timeout(timeout, output_fut).await {
                Ok(result) => result.map_err(|err| self.exec_error(err, ""))?,
                Err(_) => {
                    return Err(CliCoreError::message(format!(
                        "auth: exec {}: signal: killed: ",
                        self.command.display()
                    )));
                }
            }
        } else {
            output_fut.await.map_err(|err| self.exec_error(err, ""))?
        };

        if output.status.success() {
            return Ok(output.stdout);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(CliCoreError::message(format!(
            "auth: exec {}: {}: {stderr}",
            self.command.display(),
            compat_exit_status(&output.status)
        )))
    }

    fn request(&self, action: &str, env: &str, command: &str, tier: &str) -> AuthnRequest {
        AuthnRequest {
            action: action.to_owned(),
            provider: self.provider_name.clone(),
            env: env.to_owned(),
            realm: env.to_owned(),
            command: command.to_owned(),
            tier: tier.to_owned(),
        }
    }

    async fn list_realms_compat(&self) -> Result<Vec<String>> {
        let out = self
            .exec_raw(&AuthnRequest {
                action: ACTION_LIST_REALMS.to_owned(),
                provider: String::new(),
                env: String::new(),
                realm: String::new(),
                command: String::new(),
                tier: String::new(),
            })
            .await?;
        #[derive(Deserialize)]
        struct RealmsResponse {
            #[serde(default)]
            realms: Vec<String>,
        }
        let response: RealmsResponse = serde_json::from_slice(&out).map_err(|err| {
            CliCoreError::message(format!(
                "auth: parse realms from {}: {err}",
                self.command.display()
            ))
        })?;
        Ok(response.realms)
    }

    fn exec_error(&self, err: std::io::Error, stderr: &str) -> CliCoreError {
        CliCoreError::message(format!(
            "auth: exec {}: {err}: {stderr}",
            self.command.display()
        ))
    }
}

#[cfg(unix)]
fn compat_exit_status(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        return format!("exit status {code}");
    }
    if let Some(signal) = status.signal() {
        return format!("signal: {signal}");
    }
    status.to_string()
}

#[cfg(not(unix))]
fn compat_exit_status(status: &std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit status {code}");
    }
    status.to_string()
}

#[async_trait::async_trait]
impl AuthProvider for ExecProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    async fn get_credential(&self, env: &str, command: &str, tier: &str) -> Result<Credential> {
        self.exec_with_request(&self.request(ACTION_AUTHENTICATE, env, command, tier))
            .await
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        self.exec_with_request(&self.request(ACTION_STATUS, env, "", ""))
            .await
    }

    async fn logout(&self, env: &str) -> Result<()> {
        let _output = self
            .exec_action(&self.request(ACTION_LOGOUT, env, "", ""))
            .await?;
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        let request = AuthnRequest {
            action: ACTION_LIST_ENVIRONMENTS.to_owned(),
            provider: String::new(),
            env: String::new(),
            realm: String::new(),
            command: String::new(),
            tier: String::new(),
        };
        let out = match self.exec_raw(&request).await {
            Ok(out) => out,
            Err(_) => return self.list_realms_compat().await,
        };

        if let Ok(response) = serde_json::from_slice::<EnvironmentsResponse>(&out)
            && !response.environments.is_empty()
        {
            return Ok(response.environments);
        }

        #[derive(Deserialize, Default)]
        struct RealmsResponse {
            #[serde(default)]
            realms: Vec<String>,
        }
        if let Ok(response) = serde_json::from_slice::<RealmsResponse>(&out) {
            return Ok(response.realms);
        }

        Err(CliCoreError::message(format!(
            "auth: parse environments from {}",
            self.command.display()
        )))
    }
}
