use super::recovery::*;

#[test]
fn is_interactive_from_raw_args_non_interactive_flag() {
    let args: Vec<String> = vec![
        "my-cli".into(),
        "project".into(),
        "list".into(),
        "--non-interactive".into(),
    ];
    // --non-interactive wins even if auto_interactive is true
    assert!(!is_interactive_from_raw_args(&args, true));
}

#[test]
fn is_interactive_from_raw_args_interactive_flag() {
    let args: Vec<String> = vec![
        "my-cli".into(),
        "project".into(),
        "list".into(),
        "--interactive".into(),
    ];
    // --interactive works even without auto_interactive
    assert!(is_interactive_from_raw_args(&args, false));
}

#[test]
fn is_interactive_no_flags_auto_disabled() {
    let args: Vec<String> = vec!["my-cli".into(), "project".into(), "list".into()];
    // Without auto_interactive and no explicit flag, never interactive
    assert!(!is_interactive_from_raw_args(&args, false));
}

#[test]
fn format_prompt_message_from_flag_name() {
    let msg = format_prompt_message("--team-name", None);
    assert_eq!(msg, "team name:");
}

#[test]
fn format_prompt_message_from_positional() {
    let msg = format_prompt_message("<domain>", None);
    assert_eq!(msg, "domain:");
}

#[test]
fn strip_arg_decoration_preserves_flag_value_name_pairs() {
    assert_eq!(strip_arg_decoration("tld <TLD>"), "tld <TLD>");
}

#[test]
fn missing_arg_lookup_key_extracts_flag_id() {
    assert_eq!(missing_arg_lookup_key("tld <TLD>"), "tld");
    assert_eq!(missing_arg_lookup_key("--team-name"), "team-name");
    assert_eq!(missing_arg_lookup_key("<domain>"), "domain");
}

#[test]
fn format_prompt_message_from_flag_value_name_pair() {
    let msg = format_prompt_message("tld <TLD>", None);
    assert_eq!(msg, "TLD:");
}

#[test]
fn format_prompt_message_uses_help_text() {
    let arg = clap::Arg::new("team").long("team").help("Team identifier");
    let msg = format_prompt_message("--team", Some(&arg));
    assert_eq!(msg, "Team identifier:");
}

#[test]
fn build_resume_command_with_partial_args() {
    let resume = build_resume_command(
        "gddy",
        &[
            "domain".into(),
            "register".into(),
            "--period".into(),
            "2".into(),
        ],
    );
    assert_eq!(resume, "gddy domain register --period 2");
}

#[test]
fn whitespace_only_input_is_empty_for_the_scripted_prompter() {
    use super::{Answer, Prompter, ScriptedPrompter};
    use std::sync::Arc;

    let required: super::TextValidator = Arc::new(|input: &str| {
        if input.is_empty() {
            Err("A value is required.".to_owned())
        } else {
            Ok(())
        }
    });
    let prompter = ScriptedPrompter::new([Answer::text("   "), Answer::text("ok")]);
    let answer = prompter.text("Name", None, Some(required)).expect("answer");
    assert_eq!(answer, "ok");
    assert_eq!(
        prompter.transcript(),
        ["Name", "  rejected: A value is required.", "Name"]
    );
}

#[test]
fn resolve_leaf_command_walks_subcommands() {
    let root = clap::Command::new("my-cli").subcommand(
        clap::Command::new("project")
            .subcommand(clap::Command::new("list").arg(clap::Arg::new("team").long("team"))),
    );
    let args: Vec<String> = vec![
        "my-cli".into(),
        "project".into(),
        "list".into(),
        "--team".into(),
        "dev".into(),
    ];
    let leaf = resolve_leaf_command(&root, &args, "my-cli");
    assert!(leaf.is_some());
    assert_eq!(leaf.expect("tested").get_name(), "list");
}

#[test]
fn confirm_command_correction_declines_when_non_interactive() {
    let args: Vec<String> = vec!["my-cli".into(), "projet".into()];
    assert_eq!(
        confirm_command_correction(&args, "project", false),
        CommandCorrection::Declined
    );

    let args: Vec<String> = vec!["my-cli".into(), "projet".into(), "--non-interactive".into()];
    assert_eq!(
        confirm_command_correction(&args, "project", true),
        CommandCorrection::Declined
    );
}

#[test]
fn try_recover_returns_none_for_non_missing_arg_error() {
    let cmd = clap::Command::new("test").arg(
        clap::Arg::new("name")
            .long("name")
            .value_parser(["alpha", "beta"]),
    );
    let err = cmd
        .try_get_matches_from(["test", "--name", "invalid"])
        .expect_err("should fail");
    let args: Vec<String> = vec!["test".into(), "--name".into(), "invalid".into()];
    let result = try_recover_missing_args(&err, &args, &clap::Command::new("test"), "test", true);
    assert!(result.is_none());
}

#[test]
fn try_recover_returns_none_when_non_interactive() {
    // Build a command that knows about --non-interactive (like the real CLI)
    // so clap produces a MissingRequiredArgument error, not UnknownArgument.
    let cmd = clap::Command::new("test")
        .arg(clap::Arg::new("name").long("name").required(true))
        .arg(
            clap::Arg::new("non-interactive")
                .long("non-interactive")
                .action(clap::ArgAction::SetTrue),
        );
    let err = cmd
        .try_get_matches_from(["test", "--non-interactive"])
        .expect_err("should fail with missing --name");
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    let args: Vec<String> = vec!["test".into(), "--non-interactive".into()];
    let lookup_cmd = clap::Command::new("test")
        .arg(clap::Arg::new("name").long("name").required(true))
        .arg(
            clap::Arg::new("non-interactive")
                .long("non-interactive")
                .action(clap::ArgAction::SetTrue),
        );
    let result = try_recover_missing_args(&err, &args, &lookup_cmd, "test", true);
    assert!(result.is_none());
}

#[test]
fn try_recover_returns_none_when_auto_interactive_disabled() {
    let cmd = clap::Command::new("test").arg(clap::Arg::new("name").long("name").required(true));
    let err = cmd
        .try_get_matches_from(["test"])
        .expect_err("should fail with missing --name");
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    let args: Vec<String> = vec!["test".into()];
    let lookup_cmd =
        clap::Command::new("test").arg(clap::Arg::new("name").long("name").required(true));
    // auto_interactive = false, no explicit --interactive flag → no recovery
    let result = try_recover_missing_args(&err, &args, &lookup_cmd, "test", false);
    assert!(result.is_none());
}

#[allow(clippy::panic)]
fn missing_arg_names(cmd: &clap::Command, argv: &[&str]) -> Vec<String> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    let err = cmd
        .clone()
        .try_get_matches_from(argv)
        .expect_err("expected missing required argument");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    match err.get(ContextKind::InvalidArg) {
        Some(ContextValue::Strings(names)) => names.clone(),
        Some(ContextValue::String(name)) => vec![name.clone()],
        other => panic!("unexpected InvalidArg context: {other:?}"),
    }
}

#[test]
fn clap_reports_flag_value_name_pair_for_missing_flag_value() {
    let cmd = clap::Command::new("agreements").arg(
        clap::Arg::new("tld")
            .long("tld")
            .value_name("TLD")
            .required(true),
    );
    let names = missing_arg_names(&cmd, &["agreements"]);
    assert_eq!(names, vec!["--tld <TLD>"]);
    let arg_def = find_arg_def(&cmd, &names[0]);
    assert!(arg_def.is_some());
    assert_eq!(format_prompt_message(&names[0], arg_def), "TLD:");
    assert_eq!(format_missing_arg_label(&names[0], arg_def), "--tld");
}

#[test]
fn clap_reports_positional_value_name_for_missing_positional() {
    let cmd = clap::Command::new("suggest").arg(
        clap::Arg::new("query")
            .value_name("QUERY")
            .help("Seed domain or keywords to base suggestions on")
            .required(true),
    );
    let names = missing_arg_names(&cmd, &["suggest"]);
    assert_eq!(names, vec!["<QUERY>"]);
    let arg_def = find_arg_def(&cmd, &names[0]);
    assert!(arg_def.is_some());
    assert_eq!(
        format_prompt_message(&names[0], arg_def),
        "Seed domain or keywords to base suggestions on:"
    );
    assert_eq!(
        format_missing_arg_label(&names[0], arg_def),
        "Seed domain or keywords to base suggestions on"
    );
}

#[test]
fn clap_reports_flag_value_name_pair_with_dashed_long_flag() {
    let cmd = clap::Command::new("test").arg(
        clap::Arg::new("team_name")
            .long("team-name")
            .value_name("TEAM")
            .required(true),
    );
    let names = missing_arg_names(&cmd, &["test"]);
    assert_eq!(names, vec!["--team-name <TEAM>"]);
    let arg_def = find_arg_def(&cmd, &names[0]);
    assert!(arg_def.is_some());
    assert_eq!(format_missing_arg_label(&names[0], arg_def), "--team-name");
    assert_eq!(format_prompt_message(&names[0], arg_def), "TEAM:");
}

#[test]
fn format_prompt_message_handles_legacy_flag_value_name_without_dashes() {
    // Older clap versions reported `tld <TLD>` without a `--` prefix.
    let cmd = clap::Command::new("agreements").arg(
        clap::Arg::new("tld")
            .long("tld")
            .value_name("TLD")
            .required(true),
    );
    let arg_def = find_arg_def(&cmd, "tld <TLD>");
    assert!(arg_def.is_some());
    assert_eq!(format_prompt_message("tld <TLD>", arg_def), "TLD:");
}

#[test]
fn format_prompt_message_avoids_double_colon_when_help_ends_with_colon() {
    let arg = clap::Arg::new("domain")
        .long("domain")
        .help("Enter domain:");
    let msg = format_prompt_message("--domain", Some(&arg));
    assert_eq!(msg, "Enter domain:");
}

#[test]
fn find_arg_def_prefers_the_exact_flag_over_one_sharing_its_value_name() {
    let cmd = clap::Command::new("test")
        .arg(
            clap::Arg::new("provider-name")
                .long("provider-name")
                .value_name("NAME")
                .required(true),
        )
        .arg(
            clap::Arg::new("service-name")
                .long("service-name")
                .value_name("NAME")
                .required(true),
        );
    let found = find_arg_def(&cmd, "--service-name <NAME>").expect("arg");
    assert_eq!(found.get_id().as_str(), "service-name");
    let found = find_arg_def(&cmd, "--provider-name <NAME>").expect("arg");
    assert_eq!(found.get_id().as_str(), "provider-name");
}

#[test]
fn find_arg_def_matches_short_flag_value_name_pair() {
    let cmd = clap::Command::new("test").arg(
        clap::Arg::new("tld")
            .short('t')
            .long("tld")
            .value_name("TLD")
            .required(true),
    );
    let arg_def = find_arg_def(&cmd, "t <TLD>");
    assert!(arg_def.is_some());
    assert_eq!(format_missing_arg_label("t <TLD>", arg_def), "--tld");
}

#[test]
fn required_arg_group_lists_member_flags_in_missing_context() {
    use clap::ArgGroup;
    let cmd = clap::Command::new("update")
        .arg(clap::Arg::new("a").long("a"))
        .arg(clap::Arg::new("b").long("b"))
        .group(ArgGroup::new("ab").args(["a", "b"]).required(true));
    let names = missing_arg_names(&cmd, &["update"]);
    assert_eq!(names.len(), 1);
    assert!(names[0].contains('|'));
    let alternatives = split_missing_group_alternatives(&names[0]);
    assert_eq!(alternatives, vec!["--a <a>", "--b <b>"]);
    assert!(find_arg_def(&cmd, &alternatives[0]).is_some());
    assert!(find_arg_def(&cmd, &alternatives[1]).is_some());
}

#[test]
fn derive_exclusive_group_reports_alternatives() {
    use clap::CommandFactory;

    #[derive(clap::Parser)]
    #[command(name = "bump")]
    struct Bump {
        #[command(flatten)]
        args: ExclusiveArgs,
    }

    #[derive(clap::Args)]
    #[group(required = true, multiple = false)]
    struct ExclusiveArgs {
        #[arg(long)]
        one: bool,
        #[arg(long)]
        two: bool,
    }

    let names = missing_arg_names(&Bump::command(), &["bump"]);
    assert_eq!(names.len(), 1);
    let alternatives = split_missing_group_alternatives(&names[0]);
    assert_eq!(alternatives, vec!["--one", "--two"]);
}

#[test]
fn only_options_are_prompted_never_positionals() {
    use super::optional::{OptionalPromptResult, prompt_optional_args};
    use super::{Answer, ScriptedPrompter};

    let root = clap::Command::new("app").subcommand(
        clap::Command::new("go")
            .arg(clap::Arg::new("source"))
            .arg(clap::Arg::new("target")),
    );
    let argv: Vec<String> = ["app", "go"].iter().map(|s| (*s).to_owned()).collect();
    let matches = root.clone().try_get_matches_from(&argv).expect("parses");
    let leaf = root.find_subcommand("go").expect("leaf");
    // No answers are scripted: a positional must not trigger a prompt.
    let prompter = ScriptedPrompter::new(Vec::<Answer>::new());

    let result = prompt_optional_args(
        &prompter,
        &root,
        leaf,
        matches.subcommand_matches("go").expect("leaf matches"),
        &["target".to_owned()],
        &argv,
        "app",
    );

    assert!(
        matches!(&result, OptionalPromptResult::Done { args } if *args == argv),
        "expected nothing to be asked, got {result:?}"
    );
}

#[test]
fn multi_value_and_variadic_positionals_resolve_as_positionals() {
    let cmd = clap::Command::new("t")
        .arg(
            clap::Arg::new("range")
                .value_names(["START", "END"])
                .num_args(2)
                .required(true),
        )
        .arg(clap::Arg::new("files").num_args(1..).required(true))
        // Options that would win if the label were read as an option.
        .arg(clap::Arg::new("START").long("START"))
        .arg(clap::Arg::new("files-opt").long("files"));

    let range = find_arg_def(&cmd, "<START> <END>").expect("multi-value positional");
    assert_eq!(range.get_id().as_str(), "range");
    let files = find_arg_def(&cmd, "<files>...").expect("variadic positional");
    assert_eq!(files.get_id().as_str(), "files");
}

#[test]
fn the_leaf_is_found_past_the_value_of_a_global_option() {
    let root = clap::Command::new("app")
        .arg(clap::Arg::new("region").long("region").global(true))
        .subcommand(clap::Command::new("project").subcommand(clap::Command::new("list")))
        .subcommand(clap::Command::new("domain").subcommand(clap::Command::new("make")));
    let args: Vec<String> = [
        "app",
        "--region",
        "project",
        "domain",
        "make",
        "--interactive",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();

    let leaf = resolve_leaf_command(&root, &args, "app").expect("leaf");
    assert_eq!(leaf.get_name(), "make");
    // `--region=project` carries its own value, so it consumes nothing else.
    let attached: Vec<String> = ["app", "--region=project", "domain", "make"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let leaf = resolve_leaf_command(&root, &attached, "app").expect("leaf");
    assert_eq!(leaf.get_name(), "make");
}

#[test]
fn an_argument_the_matched_command_lacks_is_not_populated_in_any_profile() {
    let cmd = clap::Command::new("app").arg(clap::Arg::new("known").long("known"));
    let matches = cmd
        .clone()
        .try_get_matches_from(["app", "--known", "x"])
        .expect("parses");
    let known = clap::Arg::new("known").long("known");
    let stranger = clap::Arg::new("stranger").long("stranger");

    assert!(populated(&matches, &known));
    // Debug builds report an unknown id as an error and release builds as
    // absent; both must read as not populated.
    assert!(!populated(&matches, &stranger));
}
