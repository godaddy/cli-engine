//! Rendering for the engine's built-in commands and error/discovery paths:
//! `--schema`, bare-group discovery, `search`, bare-root, `guide`,
//! `completion --print`, and curated `help`.

use clap::ArgMatches;

use super::{Cli, CliRunOutput, help::render_next_actions_human, lookup, tree_render};
use crate::{
    CliCoreError, Middleware,
    command::leaf_matches,
    error::exit_code_for_error,
    guide::{guide_content, render_guide_human},
    output::NextAction,
    search::SearchIndex,
};

pub(super) fn render_schema(
    cli: &Cli,
    data: impl serde::Serialize,
    output_format: &str,
) -> CliRunOutput {
    let format: crate::output::OutputFormat = match output_format.parse() {
        Ok(format) => format,
        Err(err) => {
            return CliRunOutput {
                exit_code: exit_code_for_error(&err),
                rendered: err.to_string(),
            };
        }
    };
    let envelope = crate::Envelope::success(data, cli.config.app_id.clone()).prepare_for_render("");
    match crate::output::render(format, &envelope) {
        Ok(rendered) => CliRunOutput {
            exit_code: 0,
            rendered,
        },
        Err(err) => CliRunOutput {
            exit_code: exit_code_for_error(&err),
            rendered: err.to_string(),
        },
    }
}

/// Renders a bare group invocation (no subcommand given).
///
/// Human output keeps the existing clap help text; every other format,
/// explicit `--output json`/`--toon`, or the non-TTY default an agent
/// sees with no `--output` flag at all — gets an explicit JSON
/// command-tree subset scoped to this group, built with the same
/// [`crate::tree`] machinery as the top-level `tree` command.
pub(super) fn render_bare_group_discovery(
    cli: &Cli,
    group: &clap::Command,
    command_path: &str,
    middleware: &Middleware,
) -> CliRunOutput {
    let format: crate::output::OutputFormat = match middleware.output_format.parse() {
        Ok(format) => format,
        Err(err) => {
            return CliRunOutput {
                exit_code: exit_code_for_error(&err),
                rendered: err.to_string(),
            };
        }
    };
    if format == crate::output::OutputFormat::Human {
        return CliRunOutput {
            exit_code: 0,
            rendered: group.clone().render_long_help().to_string(),
        };
    }
    let path = format!("{} {}", cli.config.name, command_path.replace(':', " "));
    let tree = crate::tree::build_tree_from_clap_with_path(group, path);
    tree_render::render_tree_envelope(tree, &cli.config.app_id, middleware, format)
}

pub(super) fn render_search(
    cli: &Cli,
    query: &str,
    scope: &str,
    output_format: &str,
) -> CliRunOutput {
    let format: crate::output::OutputFormat = match output_format.parse() {
        Ok(format) => format,
        Err(err) => {
            return CliRunOutput {
                exit_code: exit_code_for_error(&err),
                rendered: err.to_string(),
            };
        }
    };
    let docs = lookup::search_documents(cli, scope);
    let results = SearchIndex::new(docs).search(query, 10);
    let envelope =
        crate::Envelope::success(results, cli.config.app_id.clone()).prepare_for_render("");
    match crate::output::render(format, &envelope) {
        Ok(rendered) => CliRunOutput {
            exit_code: 0,
            rendered,
        },
        Err(err) => CliRunOutput {
            exit_code: exit_code_for_error(&err),
            rendered: err.to_string(),
        },
    }
}

/// Renders the bare-root response. For human output, renders long help plus
/// a "Next actions" section so a human invoking the CLI with no arguments
/// gets readable guidance; for machine-readable output, emits a discovery
/// envelope (light metadata + next actions). The output format has already
/// resolved the TTY/env/flag policy, so this just branches on it.
pub(super) fn render_root(
    cli: &Cli,
    middleware: &Middleware,
    actions: Vec<NextAction>,
) -> CliRunOutput {
    // Reject an invalid explicit `--output` here too, matching the normal
    // command path (`Middleware::render_envelope`). `OutputFormat::from_str`
    // is infallible and would otherwise silently coerce an unrecognized
    // value (e.g. `--output yaml`) to JSON instead of reporting the error.
    if !crate::output::is_valid_output_format(&middleware.output_format) {
        let err = CliCoreError::InvalidOutputFormat(middleware.output_format.clone());
        return CliRunOutput {
            exit_code: exit_code_for_error(&err),
            rendered: err.to_string(),
        };
    }
    let format = middleware
        .output_format
        .parse()
        .unwrap_or(crate::output::OutputFormat::Json);
    if format == crate::output::OutputFormat::Human {
        // Fold the suggested actions into the root long-about so they render
        // alongside the other curated sections (before Usage) instead of
        // dangling beneath clap's options dump.
        let base_long = cli
            .root
            .get_long_about()
            .map(ToString::to_string)
            .unwrap_or_default();
        let long = format!("{base_long}{}", render_next_actions_human(&actions));
        let rendered = cli
            .root
            .clone()
            .long_about(long)
            .render_long_help()
            .to_string();
        return CliRunOutput {
            exit_code: 0,
            rendered,
        };
    }
    let description = cli
        .config
        .long
        .as_deref()
        .filter(|long| !long.is_empty())
        .unwrap_or(cli.config.short.as_str());
    let data = serde_json::json!({
        "description": description,
        "version": cli.config.build.version,
    });
    let envelope = crate::Envelope::success(data, cli.config.app_id.clone())
        .with_next_actions(actions)
        .prepare_for_render(&middleware.verbose);
    match crate::output::render(format, &envelope) {
        Ok(rendered) => CliRunOutput {
            exit_code: 0,
            rendered,
        },
        Err(err) => CliRunOutput {
            exit_code: exit_code_for_error(&err),
            rendered: err.to_string(),
        },
    }
}

pub(super) fn render_guide(cli: &Cli, matches: &ArgMatches, output_format: &str) -> CliRunOutput {
    use std::io::IsTerminal;

    // Reject an invalid explicit `--output` here too, matching the normal
    // command path and `render_root`; otherwise an unrecognized value (e.g.
    // `--output yaml`) would silently fall through and emit raw content.
    if !crate::output::is_valid_output_format(output_format) {
        let err = CliCoreError::InvalidOutputFormat(output_format.to_owned());
        return CliRunOutput {
            exit_code: exit_code_for_error(&err),
            rendered: err.to_string(),
        };
    }

    let leaf = leaf_matches(matches);
    let topic = leaf.get_one::<String>("topic").map(String::as_str);
    match guide_content(&cli.guide_entries, topic) {
        Ok(rendered) => {
            // Only reflow an actual guide topic body, and only for human output.
            // The topic list is plain text (not markdown) and json/toon keep the
            // raw markdown so their output stays deterministic.
            let rendered = if topic.is_some() && output_format == "human" {
                let is_tty = std::io::stdout().is_terminal();
                render_guide_human(&rendered, crate::output::terminal_width(), is_tty)
            } else {
                rendered
            };
            CliRunOutput {
                exit_code: 0,
                rendered,
            }
        }
        Err(err) => CliRunOutput {
            exit_code: 1,
            rendered: err,
        },
    }
}

pub(super) fn render_completion_print(
    cli: &Cli,
    shell_opt: Option<String>,
    middleware: &Middleware,
) -> CliRunOutput {
    use super::completion::{detect_shell, generate_script, parse_shell};
    let shell = match shell_opt {
        Some(s) => match parse_shell(&s) {
            Ok(s) => s,
            Err(e) => return render_cli_error(middleware, &e, &cli.config.app_id),
        },
        None => match detect_shell() {
            Ok(s) => s,
            Err(e) => return render_cli_error(middleware, &e, &cli.config.app_id),
        },
    };
    match generate_script(&cli.root, &cli.config.name, shell) {
        Ok(script) => CliRunOutput {
            exit_code: 0,
            rendered: script,
        },
        Err(e) => render_cli_error(middleware, &e, &cli.config.app_id),
    }
}

pub(super) fn render_help_command(cli: &Cli, matches: &ArgMatches) -> CliRunOutput {
    let leaf = leaf_matches(matches);
    let parts = leaf
        .get_many::<String>("command")
        .map(|values| values.map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    render_help_for_parts(cli, &parts)
}

/// Renders the curated help text for a resolved command path.
///
/// Empty `parts` render the root help. A path that resolves to a group or
/// command renders that command's long help; an unresolved path returns the
/// standard "unknown command" guidance with a non-zero exit code. Shared by
/// the root `help <path>` command and the `<group> help` subcommand form.
pub(super) fn render_help_for_parts(cli: &Cli, parts: &[&str]) -> CliRunOutput {
    if parts.is_empty() {
        return CliRunOutput {
            exit_code: 0,
            rendered: cli.root.clone().render_long_help().to_string(),
        };
    }
    let Some(command) = lookup::find_help_target(&cli.root, parts) else {
        return CliRunOutput {
            exit_code: 1,
            rendered: format!(
                "unknown command {:?} — run '{} help' for available commands",
                parts.join(" "),
                cli.config.name
            ),
        };
    };
    CliRunOutput {
        exit_code: 0,
        rendered: command.clone().render_long_help().to_string(),
    }
}

pub(super) fn render_cli_error(
    middleware: &Middleware,
    err: &(dyn std::error::Error + 'static),
    system: &str,
) -> CliRunOutput {
    let format = middleware
        .output_format
        .parse::<crate::output::OutputFormat>()
        .unwrap_or(crate::output::OutputFormat::Json);
    let envelope =
        crate::output::build_error_envelope(err, system).prepare_for_render(&middleware.verbose);
    match crate::output::render(format, &envelope) {
        Ok(rendered) => CliRunOutput {
            exit_code: exit_code_for_error(err),
            rendered,
        },
        Err(render_err) => CliRunOutput {
            exit_code: exit_code_for_error(err),
            rendered: render_err.to_string(),
        },
    }
}
