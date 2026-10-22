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
/// link reference definitions, empty list items, and indented code are left
/// exactly as authored, as are explicit hard breaks (a trailing backslash or
/// two trailing spaces).
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

/// An open code fence.
#[derive(Clone, Copy)]
struct Fence {
    marker: char,
    len: usize,
    indent: usize,
}

/// A block copied through verbatim until its end is seen.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum RawBlock {
    #[default]
    None,
    /// Tables and the common HTML block kinds end at the next blank line.
    UntilBlank,
    /// `<pre>`, `<script>`, comments and the like end at their closing
    /// delimiter and may contain blank lines.
    UntilMarker(&'static str),
}

/// What may still follow a link reference definition line.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum RefDefNext {
    #[default]
    Nothing,
    /// `[label]:` with the destination on the next line.
    Destination,
    /// An optional quoted title on the next line.
    Title,
}

#[derive(Default)]
struct Reflow<'src> {
    out: Vec<String>,
    /// Raw lines of the paragraph or list item being accumulated.
    pending: Vec<&'src str>,
    fence: Option<Fence>,
    raw: RawBlock,
    ref_def: RefDefNext,
}

impl<'src> Reflow<'src> {
    fn push(&mut self, line: &'src str) {
        if self.continue_open_block(line) {
            return;
        }
        if let Some(fence) = opens_fence(line) {
            self.emit(line);
            self.fence = Some(fence);
        } else if line.trim().is_empty() {
            self.emit("");
            self.raw = RawBlock::None;
        } else if self.raw == RawBlock::UntilBlank {
            self.out.push(line.to_owned());
        } else {
            self.push_block_line(line);
        }
    }

    /// Handles a non-blank line outside any verbatim block.
    fn push_block_line(&mut self, line: &'src str) {
        let interrupting = self.paragraph_is_open();
        if is_table_delimiter(line) && !self.pending.is_empty() {
            self.start_table_after_header(line);
        } else if let Some(next) = self.reference_definition(line) {
            self.emit(line);
            self.ref_def = next;
        } else if self.is_standalone(line) {
            self.raw = raw_block_start(line.trim_start()).unwrap_or_default();
            self.emit(line);
        } else if is_empty_list_item(line) && !interrupting {
            self.emit(line);
        } else if starts_list_item(line, interrupting) {
            self.flush();
            self.pending.push(line);
        } else {
            self.pending.push(line);
        }
    }

    /// Copies lines that belong to a block opened earlier. Returns whether
    /// `line` was consumed.
    fn continue_open_block(&mut self, line: &str) -> bool {
        if let Some(fence) = self.fence {
            if closes_fence(line, fence) {
                self.fence = None;
            }
            self.out.push(line.to_owned());
            return true;
        }
        if let RawBlock::UntilMarker(end) = self.raw {
            if line.to_ascii_lowercase().contains(end) {
                self.raw = RawBlock::None;
            }
            self.out.push(line.to_owned());
            return true;
        }
        self.continue_reference_definition(line)
    }

    fn continue_reference_definition(&mut self, line: &str) -> bool {
        let trimmed = line.trim_start();
        let (continues, next) = match std::mem::take(&mut self.ref_def) {
            RefDefNext::Nothing => return false,
            RefDefNext::Destination => (!trimmed.is_empty(), RefDefNext::Title),
            RefDefNext::Title => (trimmed.starts_with(['"', '\'', '(']), RefDefNext::Nothing),
        };
        if continues {
            self.ref_def = next;
            self.out.push(line.to_owned());
        }
        continues
    }

    /// A definition starts a block, so it cannot interrupt a paragraph.
    fn reference_definition(&self, line: &str) -> Option<RefDefNext> {
        if !self.pending.is_empty() {
            return None;
        }
        link_reference_definition(line.trim_start())
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
        self.raw = RawBlock::UntilBlank;
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
            || raw_block_start(trimmed).is_some()
            || trimmed.starts_with('>')
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

fn opens_fence(line: &str) -> Option<Fence> {
    fence_marker(line).map(|(marker, len, _)| Fence {
        marker,
        len,
        indent: indent_width(line),
    })
}

/// A closing fence may be indented up to three columns past its opener; deeper
/// than that the line is code inside the block.
fn closes_fence(line: &str, fence: Fence) -> bool {
    fence_marker(line).is_some_and(|(marker, len, rest)| {
        marker == fence.marker
            && len >= fence.len
            && rest.trim().is_empty()
            && indent_width(line) <= fence.indent + 3
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

/// HTML blocks whose content may include blank lines, paired with the
/// delimiter that ends them.
const HTML_RAW_TAGS: [(&str, &str); 4] = [
    ("<pre", "</pre>"),
    ("<script", "</script>"),
    ("<style", "</style>"),
    ("<textarea", "</textarea>"),
];

/// If `trimmed` starts a table row or HTML block, returns how that block ends.
///
/// A block that also ends on its first line yields [`RawBlock::None`].
fn raw_block_start(trimmed: &str) -> Option<RawBlock> {
    if trimmed.starts_with('|') {
        return Some(RawBlock::UntilBlank);
    }
    if !trimmed.starts_with('<') {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let Some(end) = html_end_marker(&lower) else {
        return Some(RawBlock::UntilBlank);
    };
    let rest_of_line = &lower[1..];
    Some(if rest_of_line.contains(end) {
        RawBlock::None
    } else {
        RawBlock::UntilMarker(end)
    })
}

fn html_end_marker(lower: &str) -> Option<&'static str> {
    for (open, close) in HTML_RAW_TAGS {
        let ends_name = |rest: &str| {
            rest.chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || c == '>')
        };
        if lower.strip_prefix(open).is_some_and(ends_name) {
            return Some(close);
        }
    }
    if lower.starts_with("<!--") {
        Some("-->")
    } else if lower.starts_with("<?") {
        Some("?>")
    } else if lower.starts_with("<![cdata[") {
        Some("]]>")
    } else if lower
        .strip_prefix("<!")
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_alphabetic()))
    {
        Some(">")
    } else {
        None
    }
}

/// Recognizes `[label]: destination`, with optional whitespace after the colon
/// and the destination and title possibly on following lines.
fn link_reference_definition(trimmed: &str) -> Option<RefDefNext> {
    let (label, after) = trimmed.strip_prefix('[')?.split_once("]:")?;
    if label.is_empty() || label.contains(']') {
        return None;
    }
    Some(if after.trim().is_empty() {
        RefDefNext::Destination
    } else {
        RefDefNext::Title
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

/// Splits a list marker off `line`, returning the text after it and whether an
/// ordered marker is numbered 1 (bullets count as such).
fn split_list_marker(line: &str) -> Option<(&str, bool)> {
    let trimmed = line.trim_start();
    let (rest, starts_at_one) = if let Some(rest) = trimmed.strip_prefix(['*', '-', '+']) {
        (rest, true)
    } else {
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || digits > 9 {
            return None;
        }
        let rest = trimmed[digits..].strip_prefix(['.', ')'])?;
        (rest, trimmed[..digits].parse::<u32>().is_ok_and(|n| n == 1))
    };
    (rest.is_empty() || rest.starts_with(' ')).then_some((rest, starts_at_one))
}

/// Whether `line` opens a bullet or ordered list item with content.
///
/// While a paragraph is open (`interrupting`), an ordered marker only counts
/// when it is `1.`/`1)`, as in CommonMark, so a wrapped line that merely begins
/// with a number such as `2024.` stays part of the prose.
fn starts_list_item(line: &str, interrupting: bool) -> bool {
    split_list_marker(line)
        .is_some_and(|(rest, at_one)| !rest.trim().is_empty() && (!interrupting || at_one))
}

/// A bare marker such as `* `. It stays on its own line rather than absorbing
/// whatever follows.
fn is_empty_list_item(line: &str) -> bool {
    split_list_marker(line).is_some_and(|(rest, _)| rest.trim().is_empty())
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
    fn html_blocks_with_closing_delimiters_keep_internal_blank_lines() {
        let source = "<pre>\nfirst\n\nsecond\nthird\n</pre>\n\nProse\nagain.\n\n<!-- note\n\nmore -->\nafter\ncomment\n";
        assert_eq!(
            reflow_markdown(source),
            "<pre>\nfirst\n\nsecond\nthird\n</pre>\n\nProse again.\n\n<!-- note\n\nmore -->\nafter comment\n"
        );
    }

    #[test]
    fn single_line_html_comment_does_not_swallow_following_prose() {
        let source = "<!-- hidden -->\nwrapped\nprose\n";
        assert_eq!(reflow_markdown(source), "<!-- hidden -->\nwrapped prose\n");
    }

    #[test]
    fn empty_list_items_stay_separate_entries() {
        let source = "* first item\n* \n* third item\n  wrapped\n1. one\n2.\n3. three\n";
        assert_eq!(
            reflow_markdown(source),
            "* first item\n* \n* third item wrapped\n1. one\n2.\n3. three\n"
        );
    }

    #[test]
    fn empty_marker_cannot_interrupt_a_paragraph() {
        assert_eq!(
            reflow_markdown("some text\n*\nmore\n"),
            "some text * more\n"
        );
    }

    #[test]
    fn over_indented_fence_line_is_code_not_a_closing_fence() {
        let source = "```\nfirst\n    ```\nsecond\n```\n\nprose\nhere\n";
        assert_eq!(
            reflow_markdown(source),
            "```\nfirst\n    ```\nsecond\n```\n\nprose here\n"
        );
    }

    #[test]
    fn slightly_indented_closing_fence_still_closes() {
        let source = "```\ncode\n   ```\nprose\nhere\n";
        assert_eq!(reflow_markdown(source), "```\ncode\n   ```\nprose here\n");
    }

    #[test]
    fn compact_and_multiline_reference_definitions_are_preserved() {
        let source = "[docs]:https://example.com\n[more]: https://example.org\n  \"More\"\n[far]:\n  https://far.example\n  'Far'\nRead the\ndocumentation.\n";
        assert_eq!(
            reflow_markdown(source),
            "[docs]:https://example.com\n[more]: https://example.org\n  \"More\"\n[far]:\n  https://far.example\n  'Far'\nRead the documentation.\n"
        );
    }

    #[test]
    fn bracketed_text_inside_a_paragraph_is_not_a_reference_definition() {
        let source = "See the\n[note]: it says wrap\nbehaviour.\n";
        assert_eq!(
            reflow_markdown(source),
            "See the [note]: it says wrap behaviour.\n"
        );
    }

    #[test]
    fn is_idempotent() {
        let source = "A\nb\n\n* c\n  d\n\n```\ne\nf\n```\n";
        let once = reflow_markdown(source);
        assert_eq!(reflow_markdown(&once), once);
    }
}
