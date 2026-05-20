# Authentication And Transport

`cli_engine` keeps authentication pluggable. The framework knows how to request credentials, route
auth operations by provider name, expose built-in `auth` commands, and inject credentials into HTTP
requests. Provider-specific login flows live outside the crate.

This keeps command modules small: a command declares the provider or risk metadata it needs, then
middleware resolves the credential before the handler runs.

## Credential Model

Auth providers return [`Credential`](../src/auth/credential.rs):

```rust
use cli_engine::Credential;

let credential = Credential {
    token: "eyJhb...".to_owned(),
    expires_at: "2026-05-06T14:00:00Z".to_owned(),
    cached_at: "2026-05-06T13:30:00Z".to_owned(),
    provider: "primary".to_owned(),
    env: "prod".to_owned(),
    identity: "jsmith".to_owned(),
    sub: "12345678".to_owned(),
    account_type: "employee".to_owned(),
    ..Credential::default()
};
```

The provider JSON contract uses these field names:

| Field | Meaning |
| --- | --- |
| `token` | Bearer token, JWT, or provider-specific access token. |
| `expires_at` | RFC 3339 expiration timestamp. |
| `cached_at` | RFC 3339 cache timestamp. When present, the framework uses a 30-minute TTL from this value. |
| `provider` | Provider name such as `primary` or `oauth`. |
| `env` | Environment name such as `dev`, `test`, `staging`, or `prod`. |
| `identity` | Human-readable identity, usually an account name or email address. |
| `sub` | Stable subject identifier. |
| `account_type` | Provider-specific account type. |

`Credential::effective_expiry()` prefers `cached_at + CACHE_TTL` when `cached_at` is valid, and
falls back to `expires_at`. `Credential::is_expired()` follows the same precedence. Invalid
`expires_at` values are treated as expired; credentials with neither timestamp are treated as not
expired so status display can handle partial provider responses.

The Rust struct also accepts `realm` as an environment alias for older provider binaries. New
providers should use `env`.

## AuthProvider

Custom providers implement [`AuthProvider`](../src/auth/mod.rs):

```rust
use async_trait::async_trait;
use cli_engine::{AuthProvider, Credential, Result};

#[derive(Debug)]
struct MyProvider;

#[async_trait]
impl AuthProvider for MyProvider {
    fn name(&self) -> &str {
        "primary"
    }

    async fn get_credential(&self, env: &str, command: &str, tier: &str) -> Result<Credential> {
        // Resolve or refresh a credential for this command.
        Ok(Credential {
            provider: self.name().to_owned(),
            env: env.to_owned(),
            ..Credential::default()
        })
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        // Return cached credential status for one environment.
        Ok(Credential {
            provider: self.name().to_owned(),
            env: env.to_owned(),
            ..Credential::default()
        })
    }

    async fn logout(&self, env: &str) -> Result<()> {
        // Clear cached credentials for one environment.
        let _ = env;
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        // Return environments with cached credentials.
        Ok(vec!["prod".to_owned()])
    }
}
```

`command` is the colon-separated command path, such as `project:list`. `tier` is the command risk
tier. Providers may use these as policy or login hints. Transport injectors that do not have command
context pass empty strings.

## ExecProvider

[`ExecProvider`](../src/auth/exec.rs) is the built-in provider implementation for external provider
commands. It writes a JSON request to the provider's stdin and reads a JSON response from stdout.

```rust
use std::time::Duration;

use cli_engine::auth::exec::ExecProvider;

let provider = ExecProvider::new("primary", "/opt/my-cli/bin/auth-provider")
    .with_args(["--config", "/etc/my-cli/auth.yaml"])
    .with_timeout(Duration::from_secs(30));
```

The provider name is sent in the request so one binary can serve multiple provider identities if a
CLI needs that.

## Provider Binary Contract

Every provider invocation receives an `AuthnRequest` JSON object on stdin.

```json
{
  "action": "authenticate",
  "provider": "primary",
  "env": "prod",
  "command": "project:list",
  "tier": "read"
}
```

| Field | Values | Notes |
| --- | --- | --- |
| `action` | `authenticate`, `status`, `logout`, `list-environments` | Required. |
| `provider` | Provider name such as `primary` or `oauth` | Set from `ExecProvider::new(provider_name, ...)`. |
| `env` | Application-defined environment | Required for credential operations. |
| `command` | Colon-separated command path | Optional; set by middleware when available. |
| `tier` | `read`, `mutate`, `destructive`, or app-defined policy text | Optional; set by middleware when available. |

For `authenticate` and `status`, stdout must be a credential:

```json
{
  "token": "eyJhb...",
  "expires_at": "2026-05-06T14:00:00Z",
  "cached_at": "2026-05-06T13:30:00Z",
  "provider": "primary",
  "env": "prod",
  "identity": "jsmith",
  "sub": "12345678",
  "account_type": "employee"
}
```

For `list-environments`, stdout must be:

```json
{
  "environments": ["dev", "test", "staging", "prod"]
}
```

For `logout`, any successful stdout body is ignored.

Exit code `0` means success. A non-zero exit code becomes a framework error that includes the
provider stderr output. Invalid JSON on stdout is reported as a parse error for the expected
response type.

For compatibility with existing provider binaries, `ExecProvider` also sends `realm` with the same
value as `env`, accepts credential responses containing `realm`, and can fall back to the
`list-realms` action when `list-environments` is not available.

## Dispatcher

[`Dispatcher`](../src/auth/dispatcher.rs) routes auth calls by provider name:

```rust
use std::sync::Arc;

use cli_engine::{Dispatcher, auth::exec::ExecProvider};

let mut dispatcher = Dispatcher::new();
dispatcher.register(Arc::new(ExecProvider::new(
    "primary",
    "/opt/my-cli/bin/auth-provider",
)));
dispatcher.register(Arc::new(ExecProvider::new(
    "oauth",
    "/opt/my-cli/bin/oauth-provider",
)));

let credential = dispatcher
    .get_credential("primary", "prod", "project:list", "read")
    .await?;
```

`Dispatcher::login(provider, env)` clears any cached credential first, ignores logout failures, then
requests a fresh credential. `Dispatcher::all_statuses()` asks each provider for cached
environments and then queries status for each environment.

`Dispatcher::for_provider(name)` returns a single-provider facade backed by the same shared
dispatcher. This is useful when transport code needs an `AuthProvider` for one provider name:

```rust
use std::sync::Arc;

use cli_engine::{Dispatcher, transport::ProviderBearerInjector};

let dispatcher = Dispatcher::new();
let provider = Arc::new(dispatcher.for_provider("oauth"));
let injector = ProviderBearerInjector::new(provider, "prod");
```

The facade remains linked to the dispatcher, so later provider registration or replacement is
visible to existing injectors.

## Built-In Auth Commands

When a CLI registers auth providers or configures a default provider, `cli_engine` registers an
`auth` command group:

| Command | Behavior |
| --- | --- |
| `auth login --provider NAME --env ENV` | Clears cached credentials for the environment and authenticates. |
| `auth status --provider NAME --env ENV` | Shows cached status for one provider and environment. |
| `auth status` | Shows status for all providers and cached environments. |
| `auth logout --provider NAME --env ENV` | Clears cached credentials for the environment. |

These commands are implemented with the same `CommandSpec`, middleware, output envelope, and renderers
as application commands.

## Transport Injectors

Transport injectors implement [`AuthInjector`](../src/transport/injector.rs) and mutate outbound
`reqwest::Request` values before they are sent.

| Injector | Request mutation |
| --- | --- |
| `BearerTokenInjector` | Sets `Authorization: Bearer <token>`. |
| `CookieInjector` | Appends `Cookie: <name>=<token>`. |
| `BasicAuthInjector` | Sets `Authorization: Basic <base64(username:password)>`. |
| `ApiKeyInjector` | Sets `x-api-key: <key>`. |
| `ClientCredentialsInjector` | Performs OAuth2 `client_credentials` and sets `Authorization: Bearer <token>`. |
| `ProviderBearerInjector` | Requests a credential from an `AuthProvider` and sets `Authorization: Bearer <token>`. |
| `NoopInjector` | Leaves the request unchanged. |

Token callback injectors use `TokenFunc`, an async callback returning a token string:

```rust
use std::{future::Future, pin::Pin, sync::Arc};

use cli_engine::{Result, transport::BearerTokenInjector};

let token = Arc::new(|| {
    Box::pin(async { Ok("token-value".to_owned()) })
        as Pin<Box<dyn Future<Output = Result<String>> + Send>>
});

let injector = BearerTokenInjector::new(token);
```

## HttpClient

[`HttpClient`](../src/transport/client.rs) wraps `reqwest` with the behavior command implementations
usually need:

- Auth injection before every request.
- Default headers and user-agent configuration.
- JSON request and response helpers.
- Raw response streaming helpers.
- Multipart helpers.
- ETag and `If-Match` helpers.
- GraphQL envelope helpers.
- Retries for idempotent requests on retryable status codes.
- Structured errors that preserve code, system, and request id in output envelopes.

```rust
use std::sync::Arc;

use cli_engine::transport::{HttpClient, NoopInjector};

let client = HttpClient::builder("https://api.example.test", Arc::new(NoopInjector))
    .user_agent("my-cli/1.2.3")
    .build();

let project: serde_json::Value = client.get("/v1/projects/project-1").await?;
```

Non-2xx responses are parsed into [`cli_engine::transport::Error`](../src/transport/mod.rs). The
transport error implements `DetailedError`, so rendering through `cli_engine` preserves its `code`,
`system`, and `request_id` fields in the output envelope.

## Scope Boundary

`cli_engine` owns provider routing and request injection. Product-specific login flows, token exchange
flows, and request-signing schemes belong in provider binaries or consumer application modules
unless they become broadly reusable framework concerns.
