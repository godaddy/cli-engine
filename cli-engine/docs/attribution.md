# Client attribution

Client attribution lets a CLI built on cli-engine tell the services it calls what kind of caller it is (a person at a terminal, a CI job, a script, or an AI coding agent) so usage can be understood without any telemetry channel. It adds a few tokens to the `User-Agent` header and, when an AI harness exposes a session id, one correlation header. It sends nothing to any endpoint the CLI was not already calling, writes nothing to disk, and never contacts a collector.

It is off by default. A CLI opts in with `CliConfig::with_client_attribution`.

## What is sent

| Where | Value | When |
| --- | --- | --- |
| `User-Agent` | `<name>/<version> mode/<mode>` | Always, once enabled |
| `User-Agent` | `agent/<slug>` (for example `agent/claude-code`) | A known AI harness is detected |
| `x-client-session` (name configurable) | 16 hex characters: a salted SHA-256 prefix of the harness session id | The harness exposes a session id, and the user has not opted out |

`<mode>` is the first that applies: `agent` (a harness marker is present), `ci` (the `CI` variable is set to anything other than `0`, `false`, `no`, or `off`), `interactive` (stdin and stderr are terminals), or `script` (none of the above).

An example, from a Claude Code session: `User-Agent: gddy/1.4.0 mode/agent agent/claude-code` and `x-gddy-session: 3f9a0c51d27be8a4`.

## What is not sent

The raw session id never leaves the process. Harness ids are often meaningful on the user's machine (transcript file names, resume handles), so only a one-way hash is sent. The hash is salted with the CLI's app id and the agent slug, so the same session hashes differently in different CLIs and cannot be joined across unrelated products.

No persistent identifier is created. For a person at a terminal, or any caller without a harness session id, no session header is sent at all. Nothing is stored between runs.

## Opting out

Users can drop the session header with `<APP_ID>_NO_SESSION_ID=1`, where `<APP_ID>` is the CLI's app id uppercased with non-alphanumerics replaced by `_` (`gddy` becomes `GDDY_NO_SESSION_ID`). The user-agent tokens are not affected by this variable: they describe the kind of caller in the same way the binary name and version already do. CLI authors can also disable the header entirely with `AttributionConfig::without_session_id`.

## Seeing exactly what goes out

Run any command with `--debug=transport` to print each outbound request's headers to stderr, including the user-agent and the session header.

## How detection works, and its limits

Detection reads environment variables that AI harnesses publish to the processes they launch, using the [`is-ai-agent`](https://github.com/sdairs/is-ai-agent) crate. It is cooperative and heuristic. A match does not prove a model issued this particular command (a human can run commands in a terminal a harness opened), and no match does not prove a human did. Treat the result as attribution to a harness, not as proof of intent. Session ids have different scopes per harness (a conversation, a thread, a single run), so the hash correlates calls within one harness only.

## Enabling it (CLI authors)

```rust
use cli_engine::{BuildInfo, CliConfig};
use cli_engine::transport::AttributionConfig;

let config = CliConfig::new("my-cli", "Team CLI", "my-cli")
    .with_build(BuildInfo::new(env!("CARGO_PKG_VERSION")))
    .with_client_attribution(
        AttributionConfig::new().with_session_header("x-my-cli-session"),
    );
```

The engine resolves attribution once per execution, before any command runs, and publishes it process-wide. Publishing happens in the `execute*` entrypoints, after argv0 resolution, so an argv0 personality publishes its own identity and not the dispatcher's. `Cli::run` deliberately does not publish (so running a `Cli` in tests never mutates process-wide state); a harness that drives `Cli::run` and needs outbound requests to carry the identity must publish it itself. The user-agent and default headers are published and read together, so a client never sees one without the other. It is applied to every `HttpClient` and to every client built from `transport::reqwest_client_builder()` (the entry point for generated or hand-rolled `reqwest` clients). The user-agent tokens also reach the engine's own OAuth token requests; the session header does not.

Because the headers are process-wide defaults, they are sent to whatever host those clients call. Build clients that talk to third-party hosts from a plain `reqwest::Client` if the session header should not reach them.
