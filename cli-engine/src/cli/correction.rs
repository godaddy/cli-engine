//! Unknown-command "did you mean" correction engine and `<group> help`
//! rewriting. Both operate purely on the positional command tokens of a raw
//! argument vector, independent of any [`super::Cli`] instance.

use std::collections::BTreeSet;

use clap::Command;

use super::lookup::{arg_matches_root_name, unknown_flag_consumes_value};

/// Appends a `— did you mean "…"?` suffix to an unknown-command error clause.
pub(super) fn format_did_you_mean(base: &str, suggestion: &str) -> String {
    format!("{base} — did you mean {suggestion:?}?")
}

/// First unknown group token (`unknown command "X" for "Y"`, no hint suffix).
pub(super) struct UnknownGroupCommand {
    pub(super) base: String,
}

/// Reports the first unknown token under a group. `positionals` must be pre-`--`
/// command keywords (slice to `command_keyword_count` like the group-help path).
pub(super) fn detect_unknown_group_command(
    root: &Command,
    positionals: &[String],
) -> Option<UnknownGroupCommand> {
    if positionals.is_empty() {
        return None;
    }

    let mut current = root;
    let mut path = vec![root.get_name().to_owned()];
    for token in positionals {
        if let Some(next) = current.find_subcommand(token) {
            current = next;
            path.push(next.get_name().to_owned());
            continue;
        }
        if current.get_subcommands().next().is_some() {
            let base = format!("unknown command {token:?} for {:?}", path.join(" "));
            return Some(UnknownGroupCommand { base });
        }
        return None;
    }
    None
}

/// Counts positional command tokens that precede any `--` separator.
pub(super) fn command_keyword_count(
    args: &[String],
    root_name: &str,
    bool_flags: &BTreeSet<String>,
    value_flags: &BTreeSet<String>,
) -> usize {
    let positionals = positional_command_tokens(args, root_name, bool_flags, value_flags);
    match args.iter().position(|arg| arg == "--") {
        Some(end) => {
            positional_command_tokens(&args[..end], root_name, bool_flags, value_flags).len()
        }
        None => positionals.len(),
    }
}

/// Rewrites `<group> help [sub...]` into `help <group> [sub...]` when the form
/// is present; otherwise returns `clap_args` unchanged.
pub(super) fn rewrite_group_help_if_needed(
    root: &Command,
    clap_args: &[String],
    root_name: &str,
    bool_flags: &BTreeSet<String>,
    value_flags: &BTreeSet<String>,
) -> Vec<String> {
    let positionals = positional_command_tokens(clap_args, root_name, bool_flags, value_flags);
    let keyword_count = command_keyword_count(clap_args, root_name, bool_flags, value_flags);
    let Some(parts) = group_help_target_parts(root, &positionals, keyword_count) else {
        return clap_args.to_vec();
    };
    rewrite_group_help_args(clap_args, root_name, bool_flags, value_flags, &parts)
}

/// Rewrites the `target`-th positional command token to `replacement`, preserving
/// flags. Token classification mirrors [`positional_command_tokens`].
pub(super) fn replace_positional_command_token(
    args: &[String],
    root_name: &str,
    bool_flags: &BTreeSet<String>,
    value_flags: &BTreeSet<String>,
    target: usize,
    replacement: &str,
) -> Vec<String> {
    let mut out = args.to_vec();
    let mut index = 0;
    if out
        .first()
        .is_some_and(|arg| arg_matches_root_name(arg, root_name))
    {
        index = 1;
    }

    let mut positional = 0;
    while index < out.len() {
        let arg = &out[index];
        if arg == "--" {
            break;
        }
        if arg.contains('=') {
            index += 1;
            continue;
        }
        if bool_flags.contains(arg) {
            index += 1;
            continue;
        }
        if value_flags.contains(arg)
            || unknown_flag_consumes_value(arg, out.get(index + 1).as_ref())
        {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if positional == target {
            out[index] = replacement.to_owned();
            break;
        }
        positional += 1;
        index += 1;
    }
    out
}

/// Finds the closest visible subcommand name or alias within edit-distance
/// `max(1, token_len / 3)`. Returns the canonical name; ties break alphabetically.
fn nearest_subcommand(command: &Command, token: &str) -> Option<String> {
    let token = token.to_ascii_lowercase();
    let max_distance = 1.max(token.chars().count() / 3);

    command
        .get_subcommands()
        .filter(|child| !child.is_hide_set())
        .filter_map(|child| {
            let best = std::iter::once(child.get_name())
                .chain(child.get_all_aliases())
                .map(|candidate| strsim::osa_distance(&token, &candidate.to_ascii_lowercase()))
                .min()?;
            (best <= max_distance).then(|| (best, child.get_name().to_owned()))
        })
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
        .map(|(_, name)| name)
}

/// Corrects every unknown group token to its nearest subcommand. Returns `None`
/// when any token has no near match, or when there is nothing to correct.
/// Stops at a leaf operand, curated `<group> help`, or an unfixable token.
pub(super) fn full_command_correction(
    root: &Command,
    positionals: &[String],
) -> Option<Vec<(usize, String)>> {
    let mut current = root;
    let mut corrections = Vec::new();
    for (index, token) in positionals.iter().enumerate() {
        if let Some(next) = current.find_subcommand(token) {
            current = next;
            continue;
        }
        if current.get_subcommands().next().is_none() {
            break;
        }
        if token == "help" && current.find_subcommand("help").is_none() {
            break;
        }
        let suggestion = nearest_subcommand(current, token)?;
        let next = current.find_subcommand(&suggestion)?;
        corrections.push((index, suggestion));
        current = next;
    }
    (!corrections.is_empty()).then_some(corrections)
}

/// Prompt/display text for a correction. Last-token-only fixes show the bare
/// token; anything else shows the full corrected command path.
pub(super) fn correction_display(
    root_name: &str,
    positionals: &[String],
    corrections: &[(usize, String)],
) -> String {
    if let [(index, only)] = corrections
        && *index + 1 == positionals.len()
    {
        return only.clone();
    }
    let mut tokens = vec![root_name.to_owned()];
    for (index, token) in positionals.iter().enumerate() {
        let corrected = corrections
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, replacement)| replacement.clone())
            .unwrap_or_else(|| token.clone());
        tokens.push(corrected);
    }
    tokens.join(" ")
}

/// Detects the `<group> help [sub...]` form and returns the command path whose
/// help should be rendered.
///
/// The engine ships a curated root `help` command, so it disables clap's
/// auto-generated help subcommand on the root. That setting propagates to every
/// subcommand and cannot be re-enabled per child, so `<group> help` would
/// otherwise hit clap's "unrecognized subcommand" error even though the group's
/// help listing advertises a `help` entry. We recognize the form here so the
/// caller can route it through the curated help renderer, matching clap's
/// documented equivalence between `cmd group help sub` and `cmd help group sub`.
///
/// Only groups (commands that have subcommands) are matched: a group is pure
/// subcommand dispatch, so a `help` token in that position is unambiguously a
/// help request. Leaf commands may accept a literal `help` positional argument,
/// so they are left for clap to parse (`<leaf> --help` still works). A group
/// that registers its own real `help` subcommand is likewise deferred to clap,
/// which dispatches the user-defined command (only auto-generated help is
/// suppressed).
///
/// `command_keyword_count` is the number of leading positionals that are
/// genuine command keywords (those before any `--`). A `help` at or beyond that
/// index is a literal operand after `--`, not a help request, so it is ignored.
pub(super) fn group_help_target_parts(
    root: &Command,
    positionals: &[String],
    command_keyword_count: usize,
) -> Option<Vec<String>> {
    let help_index = positionals.iter().position(|token| token == "help")?;
    // A leading `help` is the curated root help command; let it flow through.
    if help_index == 0 {
        return None;
    }
    // A `help` after a `--` separator is a literal operand; leave it for clap.
    if help_index >= command_keyword_count {
        return None;
    }
    let prefix = &positionals[..help_index];
    let mut current = root;
    for token in prefix {
        current = current.find_subcommand(token)?;
    }
    // The token before `help` must resolve to a group; leaves are left to clap.
    current.get_subcommands().next()?;
    // Defer to clap when the group defines a real `help` subcommand of its own.
    if current.find_subcommand("help").is_some() {
        return None;
    }
    // `<group> help <sub...>` shows help for `<group> <sub...>`.
    let suffix = &positionals[help_index + 1..];
    Some(prefix.iter().chain(suffix).cloned().collect())
}

/// Rewrites a `<group> help [sub...]` invocation into the canonical
/// `help <group> [sub...]` argument vector.
///
/// Only the positional command tokens are reordered (from `[group..., help,
/// sub...]` to `[help, group..., sub...]`); every flag — including `key=value`
/// forms, value-consuming flags, unknown flags that consume a value, and
/// anything after `--` — is preserved in its original place. Reordering keeps
/// the positional count unchanged, so the rewritten stream is filled slot for
/// slot. `parts` is the resolved command path (group + subcommand) from
/// [`group_help_target_parts`].
pub(super) fn rewrite_group_help_args(
    clap_args: &[String],
    root_name: &str,
    bool_flags: &BTreeSet<String>,
    value_flags: &BTreeSet<String>,
    parts: &[String],
) -> Vec<String> {
    // New positional order: the curated `help` command, then the command path.
    let mut next_positional = std::iter::once("help".to_owned())
        .chain(parts.iter().cloned())
        .peekable();
    let mut out = Vec::with_capacity(clap_args.len());
    let mut iter = clap_args.iter().peekable();
    if iter
        .peek()
        .is_some_and(|arg| arg_matches_root_name(arg, root_name))
        && let Some(program) = iter.next()
    {
        out.push(program.clone());
    }

    let mut take_positional =
        |fallback: &String| next_positional.next().unwrap_or(fallback.clone());

    while let Some(arg) = iter.next() {
        if arg == "--" {
            out.push(arg.clone());
            // Everything after `--` is positional.
            for rest in iter.by_ref() {
                out.push(take_positional(rest));
            }
            break;
        }
        if arg.contains('=') || bool_flags.contains(arg) {
            out.push(arg.clone());
            continue;
        }
        if value_flags.contains(arg) || unknown_flag_consumes_value(arg, iter.peek()) {
            out.push(arg.clone());
            if let Some(value) = iter.next() {
                out.push(value.clone());
            }
            continue;
        }
        if arg.starts_with('-') {
            out.push(arg.clone());
            continue;
        }
        out.push(take_positional(arg));
    }
    // Defensive: emit any positionals not yet placed (counts normally match).
    out.extend(next_positional);
    out
}

pub(super) fn positional_command_tokens(
    args: &[String],
    root_name: &str,
    bool_flags: &BTreeSet<String>,
    value_flags: &BTreeSet<String>,
) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut iter = args.iter().peekable();
    if iter
        .peek()
        .is_some_and(|arg| arg_matches_root_name(arg, root_name))
    {
        iter.next();
    }

    while let Some(arg) = iter.next() {
        if arg == "--" {
            tokens.extend(iter.cloned());
            break;
        }
        if arg.contains('=') {
            continue;
        }
        if bool_flags.contains(arg) {
            continue;
        }
        if value_flags.contains(arg) || unknown_flag_consumes_value(arg, iter.peek()) {
            iter.next();
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        tokens.push(arg.clone());
    }
    tokens
}

/// Returns the sole visible leaf subcommand of a bare group, if unambiguous.
///
/// Clap may still attach a `help` subcommand on nested groups even when the
/// root disables the auto help subcommand, so that name is excluded.
pub(super) fn single_leaf_subcommand(group: &Command) -> Option<String> {
    let candidates: Vec<_> = group
        .get_subcommands()
        .filter(|child| !child.is_hide_set())
        .filter(|child| child.get_name() != "help")
        .filter(|child| child.get_subcommands().next().is_none())
        .collect();
    if candidates.len() == 1 {
        Some(candidates[0].get_name().to_string())
    } else {
        None
    }
}

/// Inserts `subcommand` immediately after the colon-separated `command_path`
/// tokens in `args`, before any trailing flags or positional values.
pub(super) fn inject_subcommand_after_command_path(
    args: &[String],
    root_name: &str,
    command_path: &str,
    subcommand: &str,
    bool_flags: &BTreeSet<String>,
    value_flags: &BTreeSet<String>,
) -> Vec<String> {
    let path_parts: Vec<&str> = command_path.split(':').collect();
    let mut result = Vec::with_capacity(args.len() + 1);
    let mut iter = args.iter().peekable();

    if iter
        .peek()
        .is_some_and(|arg| arg_matches_root_name(arg, root_name))
    {
        result.push(iter.next().expect("peeked").clone());
    }

    let mut matched = 0_usize;
    while let Some(arg) = iter.next() {
        if arg == "--" {
            result.push(arg.clone());
            result.extend(iter.cloned());
            break;
        }
        if arg.contains('=') {
            result.push(arg.clone());
            continue;
        }
        if bool_flags.contains(arg) {
            result.push(arg.clone());
            continue;
        }
        if value_flags.contains(arg) || unknown_flag_consumes_value(arg, iter.peek()) {
            result.push(arg.clone());
            if let Some(value) = iter.next() {
                result.push(value.clone());
            }
            continue;
        }
        if arg.starts_with('-') {
            result.push(arg.clone());
            continue;
        }

        result.push(arg.clone());
        if matched < path_parts.len() && arg == path_parts[matched] {
            matched += 1;
            if matched == path_parts.len() {
                result.push(subcommand.to_string());
            }
        }
    }
    result
}

#[cfg(test)]
mod unknown_command_suggestion_tests {
    use super::*;
    use crate::flags::{derive_bool_flags, derive_value_flags};

    fn sample_group() -> Command {
        Command::new("gddy").subcommand(
            Command::new("domain")
                .alias("dns-domain")
                .subcommand(Command::new("list"))
                .subcommand(Command::new("available")),
        )
    }

    #[test]
    fn osa_distance_treats_adjacent_transposition_as_one_edit() {
        // Guard against swapping to `strsim::levenshtein`, which counts swaps as two edits.
        assert_eq!(strsim::osa_distance("domain", "domain"), 0);
        assert_eq!(strsim::osa_distance("domian", "domain"), 1);
        assert_eq!(strsim::osa_distance("lst", "list"), 1);
        assert_eq!(strsim::osa_distance("lsit", "list"), 1);
        assert_eq!(strsim::osa_distance("cat", "set"), 2);
    }

    #[test]
    fn nearest_subcommand_matches_close_typos() {
        let root = sample_group();
        let domain = root.find_subcommand("domain").expect("domain registered");
        assert_eq!(nearest_subcommand(domain, "lst").as_deref(), Some("list"));
        assert_eq!(nearest_subcommand(domain, "ilst").as_deref(), Some("list"));
        assert_eq!(
            nearest_subcommand(domain, "avaliable").as_deref(),
            Some("available")
        );
    }

    #[test]
    fn nearest_subcommand_rejects_unrelated_tokens() {
        let root = sample_group();
        let domain = root.find_subcommand("domain").expect("domain registered");
        assert_eq!(nearest_subcommand(domain, "missing"), None);
    }

    #[test]
    fn nearest_subcommand_returns_canonical_name_for_alias_typos() {
        let root = sample_group();
        assert_eq!(
            nearest_subcommand(&root, "dns-domian").as_deref(),
            Some("domain")
        );
    }

    #[test]
    fn nearest_subcommand_skips_hidden_commands() {
        let root = Command::new("gddy")
            .subcommand(Command::new("visible"))
            .subcommand(Command::new("hiddeen").hide(true));
        assert_eq!(nearest_subcommand(&root, "hidden"), None);
    }

    #[test]
    fn nearest_subcommand_rejects_short_unrelated_tokens() {
        let root = Command::new("gddy").subcommand(
            Command::new("config")
                .subcommand(Command::new("get"))
                .subcommand(Command::new("set"))
                .subcommand(Command::new("add")),
        );
        let config = root.find_subcommand("config").expect("config registered");
        assert_eq!(nearest_subcommand(config, "cat"), None);
        assert_eq!(nearest_subcommand(config, "x"), None);
        assert_eq!(nearest_subcommand(config, "st").as_deref(), Some("set"));
    }

    #[test]
    fn unknown_group_command_formats_did_you_mean_suffix() {
        let root = sample_group();
        let unknown = detect_unknown_group_command(&root, &["domian".to_owned()])
            .expect("domian is an unknown top-level command");
        assert_eq!(unknown.base, "unknown command \"domian\" for \"gddy\"");
        assert_eq!(
            format_did_you_mean(&unknown.base, "domain"),
            "unknown command \"domian\" for \"gddy\" — did you mean \"domain\"?"
        );
    }

    #[test]
    fn detect_unknown_group_command_reports_nested_typos() {
        let root = sample_group();
        let unknown = detect_unknown_group_command(&root, &["domain".to_owned(), "lst".to_owned()])
            .expect("lst is an unknown subcommand of domain");
        assert_eq!(unknown.base, "unknown command \"lst\" for \"gddy domain\"");
        assert_eq!(
            format_did_you_mean(&unknown.base, "list"),
            "unknown command \"lst\" for \"gddy domain\" — did you mean \"list\"?"
        );
    }

    #[test]
    fn detect_unknown_group_command_omits_hint_for_unrelated_tokens() {
        let root = sample_group();
        let unknown = detect_unknown_group_command(&root, &["missing".to_owned()])
            .expect("missing is an unknown top-level command");
        assert_eq!(unknown.base, "unknown command \"missing\" for \"gddy\"");
    }

    #[test]
    fn full_command_correction_fixes_a_single_group_typo() {
        let root = sample_group();
        let corrections = full_command_correction(&root, &["domian".to_owned()])
            .expect("domian is correctable to domain");
        assert_eq!(corrections, vec![(0, "domain".to_owned())]);
    }

    #[test]
    fn full_command_correction_fixes_every_typo_in_a_nested_path() {
        let root = sample_group();
        let corrections = full_command_correction(&root, &["domian".to_owned(), "lst".to_owned()])
            .expect("both tokens are correctable");
        assert_eq!(
            corrections,
            vec![(0, "domain".to_owned()), (1, "list".to_owned())]
        );
    }

    #[test]
    fn full_command_correction_bails_when_a_token_has_no_near_match() {
        let root = sample_group();
        assert_eq!(
            full_command_correction(&root, &["domain".to_owned(), "missing".to_owned()]),
            None
        );
    }

    #[test]
    fn full_command_correction_is_none_when_there_is_nothing_to_correct() {
        let root = sample_group();
        assert_eq!(full_command_correction(&root, &["domain".to_owned()]), None);
        assert_eq!(full_command_correction(&root, &[]), None);
    }

    #[test]
    fn full_command_correction_corrects_the_group_before_curated_help() {
        let root = sample_group();
        let corrections = full_command_correction(&root, &["domian".to_owned(), "help".to_owned()])
            .expect("domian is correctable even ahead of a help token");
        assert_eq!(corrections, vec![(0, "domain".to_owned())]);
    }

    #[test]
    fn full_command_correction_keeps_corrections_when_a_leaf_is_followed_by_an_operand() {
        let root = sample_group();
        let corrections = full_command_correction(
            &root,
            &[
                "domain".to_owned(),
                "avaliable".to_owned(),
                "example.com".to_owned(),
            ],
        )
        .expect("avaliable is correctable to available");
        assert_eq!(corrections, vec![(1, "available".to_owned())]);
    }

    #[test]
    fn correction_display_shows_the_bare_token_for_a_single_fix() {
        let corrections = vec![(1, "list".to_owned())];
        assert_eq!(
            correction_display(
                "gddy",
                &["domain".to_owned(), "lst".to_owned()],
                &corrections
            ),
            "list"
        );
    }

    #[test]
    fn correction_display_shows_the_full_command_when_a_single_fix_is_not_the_last_token() {
        let corrections = vec![(0, "domain".to_owned())];
        assert_eq!(
            correction_display(
                "gddy",
                &["domian".to_owned(), "list".to_owned()],
                &corrections
            ),
            "gddy domain list"
        );
    }

    #[test]
    fn correction_display_shows_the_full_command_for_multiple_fixes() {
        let corrections = vec![(0, "domain".to_owned()), (1, "list".to_owned())];
        assert_eq!(
            correction_display(
                "gddy",
                &["domian".to_owned(), "lst".to_owned()],
                &corrections
            ),
            "gddy domain list"
        );
    }

    #[test]
    fn replace_positional_command_token_rewrites_only_the_target() {
        let bool_flags: BTreeSet<String> = ["--verbose".to_owned()].into_iter().collect();
        let value_flags: BTreeSet<String> = ["--output".to_owned()].into_iter().collect();
        let args = vec![
            "gddy".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "domain".to_owned(),
            "lst".to_owned(),
        ];
        let corrected =
            replace_positional_command_token(&args, "gddy", &bool_flags, &value_flags, 1, "list");
        assert_eq!(
            corrected,
            vec!["gddy", "--output", "json", "domain", "list"]
        );
    }

    #[test]
    fn rewrite_group_help_if_needed_runs_after_typo_correction() {
        let root = sample_group();
        let bool_flags = derive_bool_flags(&root);
        let value_flags = derive_value_flags(&root);
        let args = vec!["gddy".to_owned(), "domian".to_owned(), "help".to_owned()];
        let corrected =
            replace_positional_command_token(&args, "gddy", &bool_flags, &value_flags, 0, "domain");
        assert_eq!(corrected, vec!["gddy", "domain", "help"]);
        let rewritten =
            rewrite_group_help_if_needed(&root, &corrected, "gddy", &bool_flags, &value_flags);
        assert_eq!(rewritten, vec!["gddy", "help", "domain"]);
    }
}
