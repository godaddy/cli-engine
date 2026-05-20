use clap::Arg;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Dispatcher;
use crate::{
    CommandResult, CommandSpec, Credential, GroupSpec, Result, RuntimeCommandSpec,
    RuntimeGroupSpec, Tier,
};

/// Data rendered after a successful `auth login`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthLoginResult {
    /// Provider used for login.
    pub provider: String,
    /// Environment used for login.
    pub env: String,
    /// Authenticated identity.
    pub identity: String,
    /// Credential expiration timestamp.
    pub expires_at: String,
}

/// Data rendered by `auth status`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthStatusEntry {
    /// Provider name.
    pub provider: String,
    /// Environment name.
    pub env: String,
    /// Cached identity, empty when missing or unavailable.
    pub identity: String,
    /// Credential expiration timestamp, empty when missing or unavailable.
    pub expires_at: String,
    /// Whether the cached credential is expired or unavailable.
    pub expired: bool,
}

/// Builds the built-in runtime `auth` command group.
#[must_use]
pub fn auth_command_group(default_provider: &str, registered_names: &[String]) -> RuntimeGroupSpec {
    let effective_default = effective_default_provider(default_provider, registered_names);
    RuntimeGroupSpec::new(GroupSpec::new("auth", "Manage authentication credentials"))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("login", "Authenticate and cache credentials")
                .with_system("auth")
                .with_tier(Tier::Mutate)
                .mutates(true)
                .no_auth(true)
                .with_arg(provider_arg(&effective_default, registered_names))
                .with_arg(Arg::new("env").long("env").value_name("ENV").required(true)),
            async |context| {
                let provider = string_arg(&context.args, "provider");
                let env = string_arg(&context.args, "env");
                serde_json::to_value(
                    login_and_build(&context.middleware.auth, &provider, &env).await?,
                )
                .map(CommandResult::new)
                .map_err(Into::into)
            },
        ))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("status", "Show cached credential status")
                .with_system("auth")
                .no_auth(true)
                .with_arg(provider_arg(&effective_default, registered_names))
                .with_arg(Arg::new("env").long("env").value_name("ENV")),
            async |context| {
                let provider = string_arg(&context.args, "provider");
                let env = string_arg(&context.args, "env");
                status_result(&context.middleware.auth, &provider, &env)
                    .await
                    .map(CommandResult::new)
            },
        ))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("logout", "Clear cached credentials")
                .with_system("auth")
                .with_tier(Tier::Mutate)
                .mutates(true)
                .no_auth(true)
                .with_arg(provider_arg(&effective_default, registered_names))
                .with_arg(Arg::new("env").long("env").value_name("ENV").required(true)),
            async |context| {
                let provider = string_arg(&context.args, "provider");
                let env = string_arg(&context.args, "env");
                logout_result(&context.middleware.auth, &provider, &env)
                    .await
                    .map(CommandResult::new)
            },
        ))
}

fn effective_default_provider(default_provider: &str, registered_names: &[String]) -> String {
    if default_provider.is_empty() {
        registered_names.first().cloned().unwrap_or_default()
    } else {
        default_provider.to_owned()
    }
}

fn string_arg(args: &serde_json::Map<String, Value>, name: &str) -> String {
    args.get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn provider_arg(default_provider: &str, registered_names: &[String]) -> Arg {
    let names = registered_names.join(", ");
    let help = format!("Auth provider name (one of: [{names}])");
    let mut arg = Arg::new("provider")
        .long("provider")
        .value_name("NAME")
        .help(help);
    if !default_provider.is_empty() {
        arg = arg.default_value(default_provider.to_owned());
    }
    arg
}

/// Runs dispatcher login and converts the credential to renderable output.
pub async fn login_and_build(
    dispatcher: &Dispatcher,
    provider: &str,
    env: &str,
) -> Result<AuthLoginResult> {
    let credential = dispatcher.login(provider, env).await?;
    Ok(AuthLoginResult {
        provider: provider.to_owned(),
        env: env.to_owned(),
        identity: credential.identity,
        expires_at: credential.expires_at,
    })
}

/// Builds the JSON value rendered by `auth status`.
pub async fn status_result(dispatcher: &Dispatcher, provider: &str, env: &str) -> Result<Value> {
    if !provider.is_empty() && !env.is_empty() {
        let credential = dispatcher.status(provider, env).await?;
        return serde_json::to_value(to_status_entry(provider, env, Some(&credential)))
            .map_err(Into::into);
    }

    let out = dispatcher
        .all_statuses()
        .await
        .iter()
        .map(|entry| {
            if entry.error.is_some() {
                AuthStatusEntry {
                    provider: entry.provider.clone(),
                    env: entry.env.clone(),
                    identity: String::new(),
                    expires_at: String::new(),
                    expired: true,
                }
            } else {
                to_status_entry(&entry.provider, &entry.env, entry.credential.as_ref())
            }
        })
        .collect::<Vec<_>>();
    serde_json::to_value(out).map_err(Into::into)
}

/// Runs dispatcher logout and builds the renderable result.
pub async fn logout_result(dispatcher: &Dispatcher, provider: &str, env: &str) -> Result<Value> {
    dispatcher.logout(provider, env).await?;
    Ok(json!({
        "provider": provider,
        "env": env,
        "status": "logged out",
    }))
}

/// Converts an optional credential into an auth status row.
#[must_use]
pub fn to_status_entry(
    provider: &str,
    env: &str,
    credential: Option<&Credential>,
) -> AuthStatusEntry {
    credential.map_or_else(
        || AuthStatusEntry {
            provider: provider.to_owned(),
            env: env.to_owned(),
            identity: String::new(),
            expires_at: String::new(),
            expired: true,
        },
        |credential| AuthStatusEntry {
            provider: provider.to_owned(),
            env: env.to_owned(),
            identity: credential.identity.clone(),
            expires_at: credential.expires_at.clone(),
            expired: credential.is_expired(),
        },
    )
}
