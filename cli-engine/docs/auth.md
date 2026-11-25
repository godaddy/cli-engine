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

## PkceAuthProvider

`PkceAuthProvider` is a built-in OAuth 2.0 Authorization Code + PKCE provider (RFC 7636). It
requires the `pkce-auth` Cargo feature.

```toml
[dependencies]
cli-engine = { path = "...", features = ["pkce-auth"] }
```

The provider manages the full browser-based login flow:

1. Generates a random PKCE code verifier and SHA-256 challenge.
2. Starts a local TCP server on `127.0.0.1` to receive the OAuth callback.
3. Opens the system browser to the authorization endpoint.
4. Exchanges the returned code for tokens at the token endpoint.
5. Persists tokens through a `CredentialStorage` backend (the system keychain by default; see [Credential Storage](#credential-storage)).
6. Refreshes expired tokens automatically using the stored refresh token.

```rust
use std::sync::Arc;
use cli_engine::{CliConfig, auth::pkce::PkceAuthProvider};

let provider = Arc::new(
    PkceAuthProvider::new(
        "primary",
        "https://auth.example.com/oauth/authorize",
        "https://auth.example.com/oauth/token",
        "my-client-id",
        &["openid", "profile"],
    )
    .with_app_id("my-cli"),   // keychain service prefix
);

let config = CliConfig::new("my-cli", "My CLI", "my-cli")
    .with_default_auth_provider("primary")
    .with_auth_provider(provider);
```

Tokens are stored per `(app_id, provider_name, env)` tuple. The in-process cache avoids
redundant keychain reads. A 30-second expiry buffer triggers proactive refresh.

The redirect port defaults to `7443`. Override it with `PkceAuthProvider::with_redirect_port`, or opt into an OS-assigned ephemeral port with `PkceAuthProvider::with_ephemeral_redirect_port` — only if the authorization server does RFC 8252 §7.3 port-agnostic loopback matching for this client; the default is unchanged until that's confirmed.

### Using The id_token As The Bearer

By default `Credential.token` is the OAuth access token. Some resource servers validate the OIDC id_token instead. `PkceAuthProvider::with_bearer(Bearer::IdToken)` makes the id_token the credential:

```rust
use cli_engine::auth::pkce::{Bearer, PkceAuthProvider};

let provider = PkceAuthProvider::new(
    "okta",
    "https://idp.example.com/oauth2/v1/authorize",
    "https://idp.example.com/oauth2/v1/token",
    "my-client-id",
    &["openid", "email"],
)
.with_bearer(Bearer::IdToken);
```

With the id_token as the bearer:

- `Credential.token` is the id_token and `Credential.expires_at` is its `exp` claim. `exp` is read without verifying the signature, like every other claim this provider decodes; the resource server decides whether the token is acceptable.
- A login must return an id_token that has an `exp` claim, so request the `openid` scope. One that does not fails and nothing is stored; the provider never falls back to the access token.
- Reuse is judged by the id_token's expiry, not the access token's. An expired one is renewed with the refresh token only when the refresh response carries a fresh id_token. The previous id_token is never carried forward, so a refresh without one is discarded and the next command logs in interactively.
- A stored token with no usable id_token, such as one written before this option was enabled, counts as not logged in: commands log in again and `auth status` reports an error for it.

The default, `Bearer::AccessToken`, is unchanged, including carrying the previous id_token across a refresh so identity claims keep resolving.

### OIDC Discovery

An OpenID Connect provider publishes its endpoints in a discovery document at
`<issuer>/.well-known/openid-configuration`. `PkceAuthProvider::with_oidc_discovery` takes the authorization and
token endpoints from it instead of from configuration, so a CLI needs only the issuer and a client id:

```rust
use std::sync::Arc;
use cli_engine::auth::pkce::{OidcDiscovery, PkceAuthProvider};

let discovery = Arc::new(OidcDiscovery::new("https://idp.example.com")?);
// The endpoints come from discovery, so the ones given here are not used.
let provider = PkceAuthProvider::new("idp", "", "", "my-client-id", &["openid", "email"])
    .with_oidc_discovery(discovery);
# Ok::<(), cli_engine::CliCoreError>(())
```

A document can say where requests go, and nothing in it vouches for that, so `OidcDiscovery` does not trust it:

- Every endpoint must be an `https` URL, with no port, credentials or fragment, on an allowed host. The issuer's own host is
  always allowed. A provider that serves endpoints from another domain needs it named with
  `OidcDiscovery::with_allowed_hosts`, which takes bare host names and matches them exactly, so
  `accounts.example.com` does not allow `login.accounts.example.com` or `accounts.example.com.evil.test`.
- The document must name the issuer it was fetched for, exactly as configured (OpenID Connect Discovery 1.0
  section 4.3), so a response served for another issuer is refused.
- A document missing an endpoint, or naming one that fails these checks, is an error that names the rule and
  never echoes the document.
- Redirects are never followed, the response is read in chunks and abandoned past 128 KiB, and the request carries
  the user-agent current at request time and no attribution headers.

The document is fetched only when an endpoint is needed, which is to log in or to refresh. Resolving the
authorization and token endpoints therefore never touches the network for a valid stored credential, `auth status`
or `auth logout`. That is a statement about endpoint resolution only: if an id_token verifier is configured, it
verifies a stored credential before reusing it or reporting it, and `JwksVerifier::from_discovery` fetches the
discovery document and the signing keys for that, so with it a discovery or key outage can make those operations
fail too. A successful fetch is kept for the
life of the `OidcDiscovery`, and concurrent callers share one request; a failed one is not kept, so the next call
tries again. A failure stops a login before anything is opened or stored, and a refresh is not attempted without
the endpoint, so a refresh token is never sent anywhere it should not go.

An `auth_url` or `token_url` set for an environment, in `environments.toml` or through `with_environments`, still
wins for that environment: it is an explicit override of the provider's own, and an endpoint set that way is not
fetched. The `auth_url` and `token_url` given to `PkceAuthProvider::new` are ignored under discovery.

With the `id-token-verify` feature, `JwksVerifier::from_discovery` takes its issuer and key-set URL from the same
`OidcDiscovery`. Pass the same `Arc` to both so the document is fetched once:

```rust,ignore
let discovery = Arc::new(OidcDiscovery::new("https://idp.example.com")?);
let verifier = JwksVerifier::from_discovery(discovery.clone())?;
let provider = PkceAuthProvider::new("idp", "", "", "my-client-id", &["openid"])
    .with_oidc_discovery(discovery)
    .with_id_token_verifier(Arc::new(verifier));
```

A discovery failure while verifying is reported as unavailable, not as a bad token, so it does not start a new
login.

### Verifying The id_token

By default the provider decodes id_token and access-token claims without checking anything about them. That is fine for showing who is logged in, but not for a decision such as authorization: the stored copy is writable by anything running as the user, so a forged `email` in it would be believed.

`PkceAuthProvider::with_id_token_verifier` takes an `IdTokenVerifier` that authenticates the id_token before the provider trusts it. With one configured:

- `identity` and `sub` come only from the claims the verifier returns, never from the raw access token or the stored copy. A token that has not been verified in this process names no identity.
- The verifier runs on every id_token the provider handles: at login, after a refresh, and when a stored credential is read back, including by `auth status`.
- A login needs an id_token, so request the `openid` scope. One the verifier rejects is never stored, and nothing is deleted when it is. When the login was started by a command that needed a credential, the previous stored credential is therefore left in place. `auth login` is different: it clears the stored credential before it starts, for every provider, so a rejected `auth login` leaves you logged out.
- A fresh token is verified against the client id it was requested with, not a new read of the configuration, so a client id that changes while the browser is open does not reject a correctly issued token.
- A refresh must return a fresh id_token that verifies. The previous id_token is never reused, so a refresh without one is discarded and the next command logs in interactively.
- A stored id_token that is rejected is not reused. `auth status` reports it as an error.

A verifier answers one of two ways when it does not accept a token. `VerifyError::Invalid` means the token is not to be trusted, and the provider falls back to a refresh or a new login. `VerifyError::Unavailable` means no decision could be made, for example because the signing keys could not be fetched. The provider returns that as an error without starting a login, because a login needs the same keys and would only overwrite a credential that may still be good.

Reasons are shown to users and logged, so an implementation must never put the token or its claim values in one.

#### JwksVerifier

`JwksVerifier` is a ready-made verifier for an OIDC provider that publishes its keys as a JSON Web Key Set. It needs the `id-token-verify` Cargo feature, and a `jsonwebtoken` backend (below).

```rust
use std::sync::Arc;
use cli_engine::auth::pkce::{JwksVerifier, PkceAuthProvider};

let verifier = JwksVerifier::new(
    "https://idp.example.com",
    "https://idp.example.com/oauth2/v1/keys",
)?;
let provider = PkceAuthProvider::new(
    "idp",
    "https://idp.example.com/oauth2/v1/authorize",
    "https://idp.example.com/oauth2/v1/token",
    "my-client-id",
    &["openid", "email"],
)
.with_id_token_verifier(Arc::new(verifier));
# Ok::<(), cli_engine::CliCoreError>(())
```

It checks the signature against the key the token names in `kid`, and the `iss`, `aud` and `exp` claims. `aud` is compared with the OAuth client id. It also applies the OpenID Connect checks a generic JWT validator omits: `sub` must be a non-empty string, and `azp` (authorized party) must be this client when present, and must be present when `aud` lists several clients, since a token with a shared audience was issued to one of them and `azp` says which. A key that declares itself unfit is not used: `use` other than `sig`, `key_ops` without `verify`, or an `alg` other than the one the token used. A key that declares none of these is unrestricted. RSA, RSA-PSS and ECDSA signatures are accepted; HMAC never is, because a verifier that accepted it would let an attacker sign a token using the public key as the secret.

- The key set is fetched once and kept. It is fetched again, at most once per verification, when a token names a `kid` the kept set does not contain, which is how a key rotation reaches a long-running process.
- A key endpoint that cannot be reached, answers an error status, or returns something unparseable or larger than 256 KiB is `Unavailable`, not a verdict on the token.
- The key-set URL must be `https` on the issuer's host with no port or credentials, and redirects are not followed, so a poisoned URL cannot send the fetch elsewhere. `JwksVerifier::new` rejects anything else.
- A fetch is bounded to 15 seconds, and the command's `--timeout` bounds the whole command.

The program must enable exactly one `jsonwebtoken` signature backend itself:

```toml
[dependencies]
cli-engine = { version = "...", features = ["id-token-verify"] }
jsonwebtoken = { version = "11", default-features = false, features = ["rust_crypto"] } # or "aws_lc_rs"
```

`id-token-verify` does not choose one for you, deliberately. `jsonwebtoken` keeps its backend in a process-wide global and panics on the first token it decodes anywhere in the process, not only in this crate, when no backend is enabled or when both are. A backend chosen here would break any program that
already uses the other one. If a program enables none, the first verification panics with a message from `jsonwebtoken` saying so; if it enables both, the same happens unless it first calls `jsonwebtoken::crypto::CryptoProvider::install_default`. The two backends accept the same algorithms, so which one to use is a build-size and policy choice rather than a behavioral one.

### Environment Variable Overrides

At runtime, the provider checks for environment variable overrides before using its compiled-in
values. The prefix is the provider name uppercased with hyphens replaced by underscores:

| Variable | Purpose |
| --- | --- |
| `<PREFIX>_OAUTH_CLIENT_ID` | OAuth client ID. |
| `<PREFIX>_OAUTH_AUTH_URL` | Authorization endpoint URL. |
| `<PREFIX>_OAUTH_TOKEN_URL` | Token endpoint URL. |

For a provider named `"primary"`, the variables are `PRIMARY_OAUTH_CLIENT_ID`,
`PRIMARY_OAUTH_AUTH_URL`, and `PRIMARY_OAUTH_TOKEN_URL`.

## Credential Storage

Tokens are persisted through the injectable `CredentialStorage` trait rather than a hard-wired keychain. Each credential is keyed by `CredentialKey { app_id, provider, env }`. Three built-in backends correspond to the `CredentialStore` modes:

- **`keyring`** (`KeyringStorage`, default) — system keychain only (macOS Keychain, Linux Secret Service, Windows Credential Manager). A keychain failure is a hard error; no file is written.
- **`auto`** (`AutoStorage`) — try the keychain, and transparently fall back to an unencrypted file when the keychain backend is unavailable.
- **`file`** (`FileStorage`) — never contact the keychain. Tokens are written as **unencrypted JSON** to `<config-base>/<app_id>/credentials/<provider>-<env>.json` (`0600` on Unix), where `<config-base>` is `$XDG_CONFIG_HOME`, `$HOME/Library/Application Support` (macOS), `$HOME/.config` (other Unix), or `%APPDATA%` (Windows).

### Listing Stored Environments

`auth status` (with no `--env`) asks each provider which environments hold a credential. A storage backend cannot answer that: the system keychain has no way to list items by prefix, and scanning the credentials directory is ambiguous, because `<provider>-<env>.json` reads the same for provider `a` with env `b-c` as for provider `a-b` with env `c`.

So `PkceAuthProvider` keeps the list itself, as one small entry in the same backend as the credentials, which makes listing work the same for `keyring`, `file` and `auto`, and after a restart:

- Saving a credential records its environment, and logging out removes it. Removing the last one removes the entry.
- Listing keeps only names the backend really holds a credential for, so an entry left behind by a crash, or by a credential deleted outside the CLI, is dropped (and removed from the entry) instead of being reported.
- The entry lives under a key derived from the provider, `<provider>.index`, not from any environment, so no environment name can address or overwrite it. Do not name a provider as another provider's name plus `.index`.
- Failing to write it never fails a login: the credential is saved, and the worst outcome is that it is not listed until the next save.
- It holds at most 256 environments of at most 128 bytes each, so a damaged entry cannot make listing probe an unbounded list. A damaged or unreadable entry reads as empty, and the next save rewrites it.

A credential saved before there was an index is found when its environment is declared to the provider (`with_environments`), which is probed and, if present, indexed; with no declared environment there is nothing to probe, and a fresh login or any save records it. The index is also available directly, as `EnvironmentIndex`, for a custom `AuthProvider` built on `CredentialStorage`.

Two processes saving at the same moment can lose one of the two additions; the next save of that environment repairs it. When `auto` storage's keychain is unavailable the index falls back to the file along with the credentials, so what is listed always matches what can actually be loaded.

### Selecting a mode

`file` is the recommended escape hatch on **WSL and headless Linux**, where a Secret Service daemon is often missing or awkward to run. Operators can disable the keychain without code changes, with this precedence (highest first):

1. `PkceAuthProvider::with_storage(...)` — inject a custom backend, or `PkceAuthProvider::with_credential_store(CredentialStore::File)` — force a built-in mode.
2. `--credential-store auto|keyring|file` global flag.
3. `${PREFIX}_CREDENTIAL_STORE` env var (e.g. `MY_CLI_CREDENTIAL_STORE=file`), where `${PREFIX}` is the **app id** uppercased with non-alphanumerics replaced by `_`.
4. `[credentials].store` in `<config-base>/<app_id>/config.toml`:
   ```toml
   [credentials]
   store = "file"
   ```
5. Default: `keyring`.

> The escape-hatch trade-off: `file`/`auto` write credentials to disk **unencrypted** (owner-only permissions on Unix). Prefer the keychain where one is available.

`with_file_fallback(bool)` is deprecated: `true` maps to `CredentialStore::Auto` and `false` to `CredentialStore::Keyring`. Use `with_credential_store` instead.

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
| `auth login --provider NAME [--env ENV]` | Clears cached credentials for the explicit environment, or the active middleware environment when omitted, and authenticates. |
| `auth status --provider NAME --env ENV` | Shows cached status for one provider and environment. |
| `auth status` | Shows status for all providers and cached environments. |
| `auth logout --provider NAME [--env ENV]` | Clears cached credentials for the explicit environment, or the active middleware environment when omitted. |

These commands are implemented with the same `CommandSpec`, middleware, output envelope, and renderers
as application commands.

### Human views

`auth status` and `auth login` render their human output through the views `auth:status` and `auth:login` (`AUTH_STATUS_VIEW_ID`, `AUTH_LOGIN_VIEW_ID`). The engine registers `default_auth_status_view()` and `default_auth_login_view()` under those ids: identity first, then expiry, environment, provider, and the remaining fields.

To change which columns show by default, set the command's default fields with `CliConfig::with_auth_status_default_fields` / `with_auth_login_default_fields`. 

```rust
let config = config.with_auth_status_default_fields("identity,expires_at,env,expired,refreshable,scopes");
```

To change the columns themselves, register a `HumanViewDef` under the same id with `CliConfig::with_view` (or `register_global_human_view`). The built-in group is kept, and a consumer view always wins over the default. `default_auth_status_view().columns` is a convenient starting point.

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
