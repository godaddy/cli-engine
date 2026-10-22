//! Joins hard-wrapped markdown prose into single-line paragraphs.
//!
//! The terminal renderer is line-oriented and keeps every source newline, so a
//! paragraph wrapped at column 80 in the source would never use a wider
//! terminal. Joining those lines first lets the renderer wrap to the real
//! width. The pass is deliberately conservative: anything it does not
//! positively recognize as flowing prose is passed through unchanged.

/// Joins the hard-wrapped lines of each paragraph and list item into one line.
///
/// Fenced code, headings, thematic breaks, tables, block quotes, HTML blocks,
/// link reference definitions, and indented code are left exactly as authored,
/// as are explicit hard breaks (a trailing backslash or two trailing spaces).
pub(crate) fn reflow_markdown(content: &str) -> String {
    let mut reflow = Reflow::default();
    for line in content.lines() {
        reflow.push(line);
    }
    let mut out = reflow.finish();
    if content.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[derive(Default)]
struct Reflow<'src> {
    out: Vec<String>,
    /// Raw lines of the paragraph or list item being accumulated.
    pending: Vec<&'src str>,
    /// Open code fence as `(marker char, marker length)`.
    fence: Option<(char, usize)>,
    /// Inside a table or HTML block, which run verbatim until a blank line.
    raw_block: bool,
}

impl<'src> Reflow<'src> {
    fn push(&mut self, line: &'src str) {
        if let Some(fence) = self.fence {
            if closes_fence(line, fence) {
                self.fence = None;
            }
            self.out.push(line.to_owned());
        } else if let Some(fence) = opens_fence(line) {
            self.emit(line);
            self.fence = Some(fence);
        } else if line.trim().is_empty() {
            self.emit("");
            self.raw_block = false;
        } else if self.raw_block {
            self.out.push(line.to_owned());
        } else if is_table_delimiter(line) && !self.pending.is_empty() {
            self.start_table_after_header(line);
        } else if self.is_standalone(line) {
            self.raw_block = starts_raw_block(line);
            self.emit(line);
        } else if starts_list_item(line, self.paragraph_is_open()) {
            self.flush();
            self.pending.push(line);
        } else {
            self.pending.push(line);
        }
    }

    fn finish(mut self) -> String {
        self.flush();
        self.out.join("\n")
    }

    /// Flushes the open paragraph, then writes `line` as-is.
    fn emit(&mut self, line: &str) {
        self.flush();
        self.out.push(line.to_owned());
    }

    /// The last pending line is the table's header row; it must not be joined
    /// into the preceding prose.
    fn start_table_after_header(&mut self, delimiter: &str) {
        let header = self.pending.pop();
        self.flush();
        self.out.extend(header.map(str::to_owned));
        self.out.push(delimiter.to_owned());
        self.raw_block = true;
    }

    /// Whether a plain paragraph (not a list item) is being accumulated.
    fn paragraph_is_open(&self) -> bool {
        self.pending
            .first()
            .is_some_and(|first| !starts_list_item(first, false))
    }

    fn is_standalone(&self, line: &str) -> bool {
        let trimmed = line.trim_start();
        // 4+ columns of indent only means code when no paragraph is open;
        // otherwise it is a lazy continuation line.
        let indented_code = self.pending.is_empty() && indent_width(line) >= 4;
        indented_code
            || is_heading(trimmed)
            || is_thematic_break_or_setext(trimmed)
            || starts_raw_block(trimmed)
            || trimmed.starts_with('>')
            || is_link_reference_definition(trimmed)
    }

    fn flush(&mut self) {
        let mut current = String::new();
        for line in std::mem::take(&mut self.pending) {
            let hard_break = has_hard_break(line);
            let text = if hard_break { line } else { line.trim_end() };
            if current.is_empty() {
                current.push_str(text);
            } else {
                current.push(' ');
                current.push_str(text.trim_start());
            }
            if hard_break {
                self.out.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            self.out.push(current);
        }
    }
}

fn indent_width(line: &str) -> usize {
    line.chars()
        .take_while(|c| matches!(c, ' ' | '\t'))
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
    let trimmed = line.trim_start();
    let marker = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = trimmed.chars().take_while(|c| *c == marker).count();
    (len >= 3).then(|| (marker, len, &trimmed[len..]))
}

fn opens_fence(line: &str) -> Option<(char, usize)> {
    fence_marker(line).map(|(marker, len, _)| (marker, len))
}

fn closes_fence(line: &str, (open, open_len): (char, usize)) -> bool {
    fence_marker(line).is_some_and(|(marker, len, rest)| {
        marker == open && len >= open_len && rest.trim().is_empty()
    })
}

fn has_hard_break(line: &str) -> bool {
    line.ends_with("  ") || line.ends_with('\\')
}

fn is_heading(trimmed: &str) -> bool {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes)
        && trimmed[hashes..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
}

/// Matches `---`/`***`/`___` rules and `===`/`---` setext heading underlines.
fn is_thematic_break_or_setext(trimmed: &str) -> bool {
    let mut marks = trimmed.chars().filter(|c| !c.is_whitespace());
    let Some(first) = marks.next() else {
        return false;
    };
    let rest = marks.clone().count() + 1;
    let uniform = marks.all(|c| c == first);
    match first {
        '=' => uniform && !trimmed.contains(' '),
        '-' | '*' | '_' => uniform && rest >= 3,
        _ => false,
    }
}

/// Tables and HTML blocks, which continue verbatim until the next blank line.
fn starts_raw_block(trimmed: &str) -> bool {
    let trimmed = trimmed.trim_start();
    trimmed.starts_with('|') || trimmed.starts_with('<')
}

fn is_link_reference_definition(trimmed: &str) -> bool {
    trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("]:"))
        .is_some_and(|(label, after)| {
            !label.is_empty() && !label.contains(']') && after.starts_with(char::is_whitespace)
        })
}

/// A delimiter row such as `| --- | :-: |` or `--- | ---`.
fn is_table_delimiter(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('|')
        && trimmed.contains('-')
        && trimmed
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

/// Whether `line` opens a bullet or ordered list item.
///
/// While a paragraph is open (`interrupting`), an ordered marker only counts
/// when it is `1.`/`1)`, as in CommonMark, so a wrapped line that merely begins
/// with a number such as `2024.` stays part of the prose.
fn starts_list_item(line: &str, interrupting: bool) -> bool {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix(['*', '-', '+']) {
        return rest.starts_with(' ') && !rest.trim().is_empty();
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return false;
    }
    let Some(rest) = trimmed[digits..].strip_prefix(['.', ')']) else {
        return false;
    };
    let starts_at_one = trimmed[..digits].parse::<u32>().is_ok_and(|n| n == 1);
    rest.starts_with(' ') && !rest.trim().is_empty() && (!interrupting || starts_at_one)
}

#[cfg(test)]
mod tests {
    use super::reflow_markdown;

    #[test]
    fn joins_hard_wrapped_paragraph_lines() {
        let source =
            "First line of a\nwrapped paragraph that\ncontinues here.\n\nSecond paragraph.\n";
        assert_eq!(
            reflow_markdown(source),
            "First line of a wrapped paragraph that continues here.\n\nSecond paragraph.\n"
        );
    }

    #[test]
    fn preserves_presence_and_absence_of_trailing_newline() {
        assert_eq!(reflow_markdown("a\nb"), "a b");
        assert_eq!(reflow_markdown("a\nb\n"), "a b\n");
    }

    #[test]
    fn joins_list_item_continuations_without_merging_items() {
        let source = "* first item that\n  wraps here\n* second item\n- dash item\n  wrapped\n1. one\n   wrapped\n2. two\n";
        assert_eq!(
            reflow_markdown(source),
            "* first item that wraps here\n* second item\n- dash item wrapped\n1. one wrapped\n2. two\n"
        );
    }

    #[test]
    fn list_directly_after_paragraph_starts_a_new_block() {
        let source = "Intro text\nstill intro:\n* item\n";
        assert_eq!(reflow_markdown(source), "Intro text still intro:\n* item\n");
    }

    #[test]
    fn wrapped_line_starting_with_a_number_stays_in_the_paragraph() {
        let source = "The release shipped in\n2024. It was fine.\n";
        assert_eq!(
            reflow_markdown(source),
            "The release shipped in 2024. It was fine.\n"
        );
    }

    #[test]
    fn leaves_fenced_code_untouched() {
        let source = "Run:\n\n```sh\nmy-cli \\\n  --flag\n\n# comment\n```\n\nAfter\nthe block.\n";
        assert_eq!(
            reflow_markdown(source),
            "Run:\n\n```sh\nmy-cli \\\n  --flag\n\n# comment\n```\n\nAfter the block.\n"
        );
    }

    #[test]
    fn longer_fence_is_not_closed_by_a_shorter_one() {
        let source = "````md\n```\nnested\n```\n````\n";
        assert_eq!(reflow_markdown(source), source);
    }

    #[test]
    fn keeps_headings_rules_and_quotes_on_their_own_lines() {
        let source = "# Title\nBody line\ncontinues.\n---\n> quoted\n> lines\ntext\n";
        assert_eq!(
            reflow_markdown(source),
            "# Title\nBody line continues.\n---\n> quoted\n> lines\ntext\n"
        );
    }

    #[test]
    fn keeps_tables_intact_including_pipe_less_edges() {
        let source =
            "Lead in\ntext.\n| A | B |\n| - | - |\n| 1 | 2 |\n\nname | value\n--- | ---\nx | y\n";
        assert_eq!(
            reflow_markdown(source),
            "Lead in text.\n| A | B |\n| - | - |\n| 1 | 2 |\n\nname | value\n--- | ---\nx | y\n"
        );
    }

    #[test]
    fn honours_explicit_hard_breaks() {
        let source = "line one  \nline two\\\nline three\nline four\n";
        assert_eq!(
            reflow_markdown(source),
            "line one  \nline two\\\nline three line four\n"
        );
    }

    #[test]
    fn leaves_indented_code_and_html_blocks_alone() {
        let source = "Text\n\n    code a\n    code b\n\n<details>\n<summary>x</summary>\nhello\n</details>\n\nProse\nagain.\n";
        assert_eq!(
            reflow_markdown(source),
            "Text\n\n    code a\n    code b\n\n<details>\n<summary>x</summary>\nhello\n</details>\n\nProse again.\n"
        );
    }

    #[test]
    fn is_idempotent() {
        let source = "A\nb\n\n* c\n  d\n\n```\ne\nf\n```\n";
        let once = reflow_markdown(source);
        assert_eq!(reflow_markdown(&once), once);
    }
}
