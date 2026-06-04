use clap::{Arg, Command};
use cli_engine::{
    GuideEntry, SearchDocument, command_path_from_matches, global_flags_from_matches,
    parse_guides_from_markdown, register_global_flags,
};
use cli_engine::{
    guide::guide_content,
    search::{SearchIndex, tokenize},
};

fn parser() -> Command {
    register_global_flags(
        Command::new("my-cli")
            .subcommand_required(false)
            .subcommand(
                Command::new("project").subcommand(
                    Command::new("list")
                        .arg(Arg::new("team").long("team"))
                        .arg(Arg::new("active").long("active").num_args(0..=1)),
                ),
            ),
    )
}

#[test]
fn global_bool_flags_accept_full_documented_bool_matrix() {
    let true_values = ["true", "t", "TRUE", "True", "1"];
    for value in true_values {
        let args = [
            "my-cli".to_owned(),
            format!("--schema={value}"),
            format!("--dry-run={value}"),
        ];
        let matches = parser().try_get_matches_from(args);
        assert!(matches.is_ok(), "true value {value:?} should parse");
        let matches = matches.expect("checked ok");
        let flags = global_flags_from_matches(&matches, "json");
        assert!(flags.schema, "schema value {value:?}");
        assert!(flags.dry_run, "dry-run value {value:?}");
    }

    let false_values = ["false", "f", "FALSE", "False", "0"];
    for value in false_values {
        let args = [
            "my-cli".to_owned(),
            format!("--schema={value}"),
            format!("--dry-run={value}"),
        ];
        let matches = parser().try_get_matches_from(args);
        assert!(matches.is_ok(), "false value {value:?} should parse");
        let matches = matches.expect("checked ok");
        let flags = global_flags_from_matches(&matches, "json");
        assert!(!flags.schema, "schema value {value:?}");
        assert!(!flags.dry_run, "dry-run value {value:?}");
    }
}

#[test]
fn global_optional_value_flags_have_missing_value_defaults() {
    let matches = parser()
        .try_get_matches_from(["my-cli", "--verbose", "--debug"])
        .expect("optional flags should accept omitted values");
    assert_eq!(command_path_from_matches("my-cli", &matches), "");
    assert_eq!(global_flags_from_matches(&matches, "json").verbose, "all");
    assert_eq!(global_flags_from_matches(&matches, "json").debug, "*");
}

#[test]
fn global_flags_reject_invalid_bool_and_numeric_inputs() {
    for args in [
        ["my-cli", "--schema=maybe"].as_slice(),
        ["my-cli", "--dry-run=maybe"].as_slice(),
        ["my-cli", "--limit", "abc"].as_slice(),
        ["my-cli", "--offset", "abc"].as_slice(),
    ] {
        assert!(
            parser().try_get_matches_from(args).is_err(),
            "args should fail: {args:?}"
        );
    }
}

#[test]
fn guide_parsing_matrix_covers_front_matter_and_topic_resolution() {
    let guides = parse_guides_from_markdown([
        (
            "guides/deploy.md",
            b"---\nsummary: Deploy safely\n---\n# Deploy\n".as_slice(),
        ),
        (
            "guides/no-summary.md",
            b"---\nowner: platform\n---\n# No Summary\n".as_slice(),
        ),
        (
            "guides/broken.md",
            b"---\nsummary: Broken\n# Missing terminator\n".as_slice(),
        ),
        ("guides/not-markdown.txt", b"ignored".as_slice()),
        ("guides/windows\\operate.md", b"# Operate\n".as_slice()),
    ]);

    assert_eq!(
        guides
            .iter()
            .map(|guide| (guide.name.as_str(), guide.summary.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("broken", ""),
            ("deploy", "Deploy safely"),
            ("no-summary", ""),
            ("operate", ""),
        ]
    );

    assert_eq!(
        guide_content(&guides, Some("deploy")).expect("topic"),
        "# Deploy\n"
    );
    let missing = guide_content(&guides, Some("missing")).expect_err("missing topic");
    assert!(missing.contains("valid topics: broken, deploy, no-summary, operate"));
}

#[test]
fn search_tokenization_and_ranking_handle_noise_and_empty_queries() {
    assert_eq!(
        tokenize("The PROJECTS, projected project-ing owners!"),
        vec!["project", "project", "project", "ing", "owner"]
    );

    let index = SearchIndex::new(vec![
        SearchDocument::new("cmd:project:list", "command", "project list")
            .with_summary("List projects")
            .with_content("list project projects owner status"),
        SearchDocument::new("guide:deploy", "guide", "deploy guide")
            .with_summary("Deploy services")
            .with_content("deploy release rollout"),
    ]);

    assert!(index.search("the and or", 10).is_empty());
    let one = index.search("projects owner", 1);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].command, "project list");
    assert!(one[0].confidence > 0.0);
}

#[test]
fn duplicate_guide_topics_use_last_entry_for_content() {
    let entries = vec![
        GuideEntry::new("deploy", "Old", "old content"),
        GuideEntry::new("deploy", "New", "new content"),
    ];
    assert_eq!(
        guide_content(&entries, Some("deploy")).expect("duplicate topic"),
        "new content"
    );
}
