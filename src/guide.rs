use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Parsed guide document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuideEntry {
    /// Topic name, usually the markdown filename without `.md`.
    pub name: String,
    /// One-line summary from front matter.
    pub summary: String,
    /// Markdown body without front matter.
    pub content: String,
}

impl GuideEntry {
    /// Creates a guide entry from explicit topic metadata and markdown content.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        summary: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            summary: summary.into(),
            content: content.into(),
        }
    }

    /// Parses a guide entry from a markdown path and content.
    #[must_use]
    pub fn from_markdown_path(path: &str, content: &str) -> Self {
        let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let name = file_name
            .strip_suffix(".md")
            .unwrap_or(file_name)
            .to_owned();
        let (summary, body) = parse_front_matter(content);
        Self {
            name,
            summary,
            content: body,
        }
    }
}

/// Parses all markdown guide files under a directory.
pub fn parse_guides(root: impl AsRef<Path>) -> io::Result<Vec<GuideEntry>> {
    let mut markdown_paths = Vec::new();
    collect_markdown_paths(root.as_ref(), &mut markdown_paths)?;
    markdown_paths.sort();

    Ok(parse_guides_from_markdown(
        markdown_paths
            .into_iter()
            .filter_map(|path| fs::read(&path).ok().map(|content| (path, content))),
    ))
}

/// Parses guide entries from embedded `(path, bytes)` markdown pairs.
#[must_use]
pub fn parse_guides_from_markdown(
    files: impl IntoIterator<Item = (impl AsRef<Path>, impl AsRef<[u8]>)>,
) -> Vec<GuideEntry> {
    let mut files = files
        .into_iter()
        .filter_map(|(path, content)| {
            let path = path.as_ref().to_string_lossy().into_owned();
            path.ends_with(".md")
                .then(|| (path, content.as_ref().to_owned()))
        })
        .collect::<Vec<_>>();
    files.sort_by(|(left, _), (right, _)| left.cmp(right));
    files
        .into_iter()
        .map(|(path, content)| {
            let content = String::from_utf8_lossy(&content);
            GuideEntry::from_markdown_path(&path, content.as_ref())
        })
        .collect()
}

fn collect_markdown_paths(dir: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut entries = match fs::read_dir(dir) {
        Ok(entries) => entries.collect::<io::Result<Vec<_>>>()?,
        Err(_) => return Ok(()),
    };
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_markdown_paths(&path, paths)?;
        } else if path.extension().is_some_and(|extension| extension == "md") {
            paths.push(path);
        }
    }
    Ok(())
}

/// Parses optional YAML front matter and returns `(summary, body)`.
#[must_use]
pub fn parse_front_matter(content: &str) -> (String, String) {
    let Some(rest) = content.strip_prefix("---\n") else {
        return (String::new(), content.to_owned());
    };
    let Some(end) = rest.find("\n---\n") else {
        return (String::new(), content.to_owned());
    };
    let block = &rest[..end];
    let body = &rest[end + "\n---\n".len()..];
    let summary = block
        .lines()
        .filter_map(|line| line.strip_prefix("summary:").map(str::trim))
        .next_back()
        .unwrap_or_default()
        .to_owned();
    (summary, body.to_owned())
}

/// Renders the guide topic list.
#[must_use]
pub fn list_guides(entries: &[GuideEntry]) -> String {
    let mut out = String::from("Available guide topics:\n\n");
    for entry in entries {
        out.push_str(&format!("  {:<16} {}\n", entry.name, entry.summary));
    }
    out.push_str("\nUsage: <cli> guide <topic>");
    out
}

/// Returns either the guide topic list or one guide's content.
pub fn guide_content(entries: &[GuideEntry], topic: Option<&str>) -> Result<String, String> {
    let Some(topic) = topic else {
        return Ok(list_guides(entries));
    };
    entries
        .iter()
        .rev()
        .find(|entry| entry.name == topic)
        .map(|entry| entry.content.clone())
        .ok_or_else(|| {
            let names = entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("unknown guide topic {topic:?} — valid topics: {names}")
        })
}
