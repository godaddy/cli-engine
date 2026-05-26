use std::sync::Arc;

use async_trait::async_trait;
use cli_engine::{
    AuthProvider, Cli, CliConfig, CliCoreError, CommandSpec, Credential, GroupSpec, Module, Result,
    RuntimeCommandSpec, RuntimeGroupSpec, StreamSender,
};
use serde_json::json;

#[derive(Debug)]
struct AlwaysFailAuth;

#[async_trait]
impl AuthProvider for AlwaysFailAuth {
    fn name(&self) -> &str {
        "always-fail"
    }

    async fn get_credential(&self, _env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        Err(CliCoreError::message(
            "auth failed: no credentials configured",
        ))
    }

    async fn status(&self, _env: &str) -> Result<Credential> {
        Err(CliCoreError::message("not logged in"))
    }

    async fn logout(&self, _env: &str) -> Result<()> {
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }
}

#[allow(closure_returning_async_block)]
fn cli_with_erroring_handler(msg: &'static str) -> Cli {
    let module = Module::new("Streaming Tests", move |_| {
        RuntimeGroupSpec::new(GroupSpec::new("deploy", "Deploy commands")).with_command(
            RuntimeCommandSpec::new_streaming(
                CommandSpec::new("run", "Run a deploy").no_auth(true),
                move |_ctx, _sender: StreamSender| async move {
                    Err::<(), _>(CliCoreError::message(msg))
                },
            ),
        )
    });
    Cli::new(CliConfig::new("test-cli", "Test CLI", "test").with_module(module))
}

fn cli_with_failing_auth() -> Cli {
    let module = Module::new("Streaming Tests", |_| {
        RuntimeGroupSpec::new(GroupSpec::new("deploy", "Deploy commands")).with_command(
            RuntimeCommandSpec::new_streaming(
                CommandSpec::new("run", "Run a deploy"),
                async |_ctx, sender: StreamSender| {
                    sender.send(json!({"status": "deploying"})).await;
                    Ok(())
                },
            ),
        )
    });
    Cli::new(
        CliConfig::new("test-cli", "Test CLI", "test")
            .with_auth_provider(Arc::new(AlwaysFailAuth))
            .with_default_auth_provider("always-fail")
            .with_module(module),
    )
}

/// Streaming handler errors must propagate as a non-zero exit code.
///
/// Before the fix, middleware.run rendered errors into Ok(MiddlewareOutput{exit_code:non-zero})
/// but the streaming match arm always returned exit_code:0.
#[tokio::test]
async fn streaming_handler_error_exits_nonzero() {
    let result = cli_with_erroring_handler("deployment failed")
        .run(["test-cli", "deploy", "run"])
        .await;

    assert_ne!(result.exit_code, 0, "handler error should exit non-zero");
    assert!(
        result.rendered.contains("deployment failed"),
        "expected error message in rendered output, got: {}",
        result.rendered
    );
}

fn cli_with_successful_handler() -> Cli {
    let module = Module::new("Streaming Tests", |_| {
        RuntimeGroupSpec::new(GroupSpec::new("deploy", "Deploy commands")).with_command(
            RuntimeCommandSpec::new_streaming(
                CommandSpec::new("run", "Run a deploy").no_auth(true),
                async |_ctx, sender: StreamSender| {
                    sender.send(json!({"status": "building"})).await;
                    sender.send(json!({"status": "done"})).await;
                    Ok(())
                },
            ),
        )
    });
    Cli::new(CliConfig::new("test-cli", "Test CLI", "test").with_module(module))
}

/// Successful streaming commands must exit 0 with no trailing rendered output.
///
/// NDJSON events are written directly to stdout by the writer task. The
/// framework must not append a rendered envelope (e.g. `{"data": null}`)
/// after the events.
#[tokio::test]
async fn streaming_success_exits_zero_with_no_rendered_output() {
    let result = cli_with_successful_handler()
        .run(["test-cli", "deploy", "run"])
        .await;

    assert_eq!(result.exit_code, 0, "successful stream should exit 0");
    assert!(
        result.rendered.is_empty(),
        "successful stream should produce no rendered output, got: {:?}",
        result.rendered
    );
}

/// Auth failures on streaming commands must exit non-zero.
///
/// Middleware renders auth errors as Ok(MiddlewareOutput{exit_code:non-zero}), which
/// the streaming match arm must preserve rather than discard.
#[tokio::test]
async fn streaming_auth_failure_exits_nonzero() {
    let result = cli_with_failing_auth()
        .run(["test-cli", "deploy", "run"])
        .await;

    assert_ne!(result.exit_code, 0, "auth failure should exit non-zero");
    assert!(
        result.rendered.contains("auth failed"),
        "expected auth error in rendered output, got: {}",
        result.rendered
    );
}
