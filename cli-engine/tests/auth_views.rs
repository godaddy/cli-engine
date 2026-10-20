//! Default and overridden human views for the built-in `auth status` and
//! `auth login` commands.

use std::sync::Arc;

use async_trait::async_trait;
use cli_engine::{
    AuthProvider, Cli, CliConfig, Credential, HumanViewDef, Result, TableColumn,
    default_auth_status_view,
};
use serde_json::Value;

#[derive(Debug)]
struct FakeProvider;

#[async_trait]
impl AuthProvider for FakeProvider {
    fn name(&self) -> &str {
        "primary"
    }

    async fn get_credential(&self, env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        Ok(Credential {
            token: "token".to_owned(),
            expires_at: "2099-01-01T00:00:00Z".to_owned(),
            env: env.to_owned(),
            identity: "tester".to_owned(),
            ..Credential::default()
        })
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        self.get_credential(env, "", "").await
    }

    async fn logout(&self, _env: &str) -> Result<()> {
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        Ok(vec!["prod".to_owned()])
    }
}

fn config() -> CliConfig {
    CliConfig {
        name: "my-cli".to_owned(),
        short: "Auth view test CLI".to_owned(),
        app_id: "my-cli".to_owned(),
        default_auth_provider: Some("primary".to_owned()),
        ..CliConfig::default()
    }
}

fn cli_with(config: CliConfig) -> Cli {
    let mut cli = Cli::new(config);
    cli.register_auth_provider(Arc::new(FakeProvider));
    cli
}

/// Column headers of the first table-looking line in human output.
fn header_line(rendered: &str) -> &str {
    rendered
        .lines()
        .find(|line| line.contains("IDENTITY"))
        .expect("human output has an IDENTITY header line")
}

fn position(header: &str, column: &str) -> usize {
    assert!(header.contains(column), "{column} missing from: {header}");
    header.find(column).expect("checked above")
}

async fn run(cli: &Cli, args: &[&str]) -> String {
    let mut argv = vec!["my-cli"];
    argv.extend_from_slice(args);
    let output = cli.run(argv).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    output.rendered
}

#[tokio::test]
async fn default_status_view_leads_with_identity_then_expires() {
    let cli = cli_with(config());
    let rendered = run(&cli, &["auth", "status", "--output", "human"]).await;
    let header = header_line(&rendered);
    assert!(position(header, "IDENTITY") < position(header, "EXPIRES"));
    assert!(position(header, "EXPIRES") < position(header, "ENV"));
    assert!(header.contains("PROVIDER"));
}

#[tokio::test]
async fn default_login_view_leads_with_identity_then_expires() {
    let cli = cli_with(config());
    let rendered = run(
        &cli,
        &["auth", "login", "--env", "prod", "--output", "human"],
    )
    .await;
    assert!(rendered.contains("tester"), "{rendered}");
    let identity = position(&rendered, "Identity");
    let expires = position(&rendered, "Expires");
    assert!(identity < expires, "{rendered}");
}

#[tokio::test]
async fn config_view_overrides_status_columns_without_replacing_the_group() {
    let columns = default_auth_status_view()
        .columns
        .into_iter()
        .filter(|column| column.field != "provider")
        .collect::<Vec<TableColumn>>();
    let cli = cli_with(config().with_view(HumanViewDef::new("auth:status", columns)));

    let rendered = run(&cli, &["auth", "status", "--output", "human"]).await;
    assert!(!header_line(&rendered).contains("PROVIDER"), "{rendered}");

    // The built-in group is intact: login/logout still resolve.
    run(&cli, &["auth", "logout", "--env", "prod"]).await;
}

#[tokio::test]
async fn override_survives_a_later_provider_registration() {
    let cli_config = config().with_view(HumanViewDef::new(
        "auth:status",
        vec![TableColumn::new("identity", "Identity")],
    ));
    let mut cli = Cli::new(cli_config);
    cli.register_auth_provider(Arc::new(FakeProvider));

    let rendered = run(&cli, &["auth", "status", "--output", "human"]).await;
    assert!(!rendered.contains("EXPIRES"), "{rendered}");
}

#[tokio::test]
async fn json_output_still_carries_every_field() {
    let cli = cli_with(config());
    let rendered = run(&cli, &["auth", "status", "--output", "json"]).await;
    let envelope: Value = serde_json::from_str(&rendered).expect("valid json");
    let row = &envelope["data"][0];
    for field in [
        "provider",
        "env",
        "identity",
        "expires_at",
        "expired",
        "refreshable",
    ] {
        assert!(row.get(field).is_some(), "missing {field}: {row}");
    }
}

#[tokio::test]
async fn fields_flag_narrows_the_default_view() {
    let cli = cli_with(config());
    let rendered = run(
        &cli,
        &[
            "auth", "status", "--output", "human", "--fields", "identity",
        ],
    )
    .await;
    assert!(!rendered.contains("EXPIRES"), "{rendered}");
}

const NO_PROVIDER: &str = "identity,expires_at,env,expired,refreshable,scopes";

#[tokio::test]
async fn default_fields_leave_provider_out_but_keep_it_selectable() {
    let cli = cli_with(config().with_auth_status_default_fields(NO_PROVIDER));

    let default = run(&cli, &["auth", "status", "--output", "human"]).await;
    assert!(!header_line(&default).contains("PROVIDER"), "{default}");

    let explicit = run(
        &cli,
        &[
            "auth",
            "status",
            "--output",
            "human",
            "--fields",
            "identity,provider",
        ],
    )
    .await;
    let header = header_line(&explicit);
    assert!(position(header, "IDENTITY") < position(header, "PROVIDER"));
    assert!(!header.contains("EXPIRES"), "{explicit}");
}

#[tokio::test]
async fn login_default_fields_leave_provider_out_but_keep_it_selectable() {
    let cli = cli_with(config().with_auth_login_default_fields("identity,expires_at,env"));
    let login = ["auth", "login", "--env", "prod", "--output", "human"];

    let default = run(&cli, &login).await;
    assert!(!default.contains("Provider"), "{default}");

    let mut explicit = login.to_vec();
    explicit.extend(["--fields", "provider"]);
    let explicit = run(&cli, &explicit).await;
    assert!(explicit.contains("Provider: primary"), "{explicit}");
}

#[tokio::test]
async fn default_fields_project_json_like_any_other_command() {
    let cli = cli_with(config().with_auth_status_default_fields(NO_PROVIDER));

    let default = run(&cli, &["auth", "status", "--output", "json"]).await;
    let envelope: Value = serde_json::from_str(&default).expect("valid json");
    assert!(envelope["data"][0].get("provider").is_none(), "{default}");

    let explicit = run(
        &cli,
        &["auth", "status", "--output", "json", "--fields", "provider"],
    )
    .await;
    let envelope: Value = serde_json::from_str(&explicit).expect("valid json");
    assert_eq!(envelope["data"][0]["provider"], "primary");
}

#[tokio::test]
async fn help_lists_every_output_field_and_marks_the_defaults() {
    let cli = cli_with(config().with_auth_status_default_fields(NO_PROVIDER));
    let help = run(&cli, &["auth", "status", "--help"]).await;
    assert!(help.contains("Output fields:"), "{help}");
    let provider = help
        .lines()
        .find(|line| line.trim_start().starts_with("provider"))
        .expect("provider is listed");
    assert!(!provider.contains("(default)"), "{provider}");
    let identity = help
        .lines()
        .find(|line| line.trim_start().starts_with("identity"))
        .expect("identity is listed");
    assert!(identity.contains("(default)"), "{identity}");
}

#[tokio::test]
async fn fields_default_precedes_the_field_table() {
    let cli = cli_with(config().with_auth_status_default_fields(NO_PROVIDER));
    let help = run(&cli, &["auth", "status", "--help"]).await;
    let default = format!("[default: {NO_PROVIDER}]");
    assert_eq!(help.matches("[default: identity").count(), 1, "{help}");
    let default_at = help.find(&default).expect("default is shown");
    let table_at = help.find("Output fields:").expect("table is shown");
    assert!(default_at < table_at, "{help}");
}

#[tokio::test]
async fn expr_reshaped_results_fall_back_to_generic_rendering() {
    let cli = cli_with(config());
    let login = ["auth", "login", "--env", "prod"];
    let status = ["auth", "status", "--provider", "primary", "--env", "prod"];
    for (base, expr) in [
        (&status[..], "{user: identity}"),
        (&login[..], "{user: identity}"),
    ] {
        let mut args = base.to_vec();
        args.extend(["--expr", expr, "--output", "human"]);
        let rendered = run(&cli, &args).await;
        assert!(rendered.to_lowercase().contains("user"), "{rendered}");
        assert!(rendered.contains("tester"), "{rendered}");
    }

    let rows = run(
        &cli,
        &[
            "auth",
            "status",
            "--expr",
            "[].{user: identity}",
            "--output",
            "human",
        ],
    )
    .await;
    assert!(rows.contains("USER") && rows.contains("tester"), "{rows}");
}

#[tokio::test]
async fn expr_that_keeps_the_shape_keeps_the_view() {
    let cli = cli_with(config());
    let rendered = run(
        &cli,
        &[
            "auth",
            "status",
            "--expr",
            "[?env=='prod']",
            "--output",
            "human",
        ],
    )
    .await;
    let header = header_line(&rendered);
    assert!(position(header, "IDENTITY") < position(header, "EXPIRES"));
}
