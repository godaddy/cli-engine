use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

/// Searchable document contributed by commands, guides, or applications.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchDocument {
    /// Stable document id.
    pub id: String,
    /// Document kind, such as `command` or `guide`.
    pub kind: String,
    /// Search result title.
    pub title: String,
    /// Short snippet shown in search results.
    pub summary: String,
    /// Full searchable content.
    pub content: String,
}

impl SearchDocument {
    /// Creates a searchable document with title text as the default content.
    #[must_use]
    pub fn new(id: impl Into<String>, kind: impl Into<String>, title: impl Into<String>) -> Self {
        let title = title.into();
        Self {
            id: id.into(),
            kind: kind.into(),
            summary: title.clone(),
            content: title.clone(),
            title,
        }
    }

    /// Sets the short snippet shown in search results.
    #[must_use]
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    /// Sets the full searchable content.
    #[must_use]
    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = content.into();
        self
    }
}

/// Ranked search hit rendered by `--search`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchResult {
    /// Command path or guide title.
    pub command: String,
    /// Short search snippet.
    pub snippet: String,
    /// Cosine-similarity confidence score.
    pub confidence: f64,
}

/// Small TF-IDF search index for command and guide discovery.
#[derive(Clone, Debug)]
pub struct SearchIndex {
    docs: Vec<SearchDocument>,
    tfidf: Vec<BTreeMap<String, f64>>,
    idf: BTreeMap<String, f64>,
}

impl SearchIndex {
    /// Builds a search index from documents.
    #[must_use]
    pub fn new(docs: Vec<SearchDocument>) -> Self {
        let mut doc_terms = Vec::with_capacity(docs.len());
        let mut df = BTreeMap::<String, usize>::new();
        for doc in &docs {
            let tokens = tokenize(&doc.content);
            let mut tf = BTreeMap::<String, f64>::new();
            for token in &tokens {
                *tf.entry(token.clone()).or_default() += 1.0;
            }
            let len = tokens.len() as f64;
            if len > 0.0 {
                for value in tf.values_mut() {
                    *value /= len;
                }
            }
            let mut seen = BTreeSet::new();
            for token in tokens {
                if seen.insert(token.clone()) {
                    *df.entry(token).or_default() += 1;
                }
            }
            doc_terms.push(tf);
        }
        let doc_count = docs.len() as f64;
        let idf = df
            .into_iter()
            .map(|(term, count)| (term, (1.0 + doc_count / count as f64).ln()))
            .collect::<BTreeMap<_, _>>();
        let tfidf = doc_terms
            .into_iter()
            .map(|tf| {
                tf.into_iter()
                    .map(|(term, freq)| {
                        let weighted = freq * idf.get(&term).copied().unwrap_or_default();
                        (term, weighted)
                    })
                    .collect()
            })
            .collect();
        Self { docs, tfidf, idf }
    }

    /// Searches the index and returns up to `top_k` ranked hits.
    #[must_use]
    pub fn search(&self, query: &str, top_k: usize) -> Vec<SearchResult> {
        let tokens = tokenize(query);
        if tokens.is_empty() {
            return Vec::new();
        }
        let mut qtf = BTreeMap::<String, f64>::new();
        for token in &tokens {
            *qtf.entry(token.clone()).or_default() += 1.0;
        }
        let len = tokens.len() as f64;
        let qvec = qtf
            .into_iter()
            .filter_map(|(term, freq)| self.idf.get(&term).map(|idf| (term, (freq / len) * idf)))
            .collect::<BTreeMap<_, _>>();
        if qvec.is_empty() {
            return Vec::new();
        }
        let mut results = self
            .tfidf
            .iter()
            .enumerate()
            .filter_map(|(index, dvec)| {
                let score = cosine(&qvec, dvec);
                (score > 0.0).then(|| SearchResult {
                    command: self.docs[index].title.clone(),
                    snippet: self.docs[index].summary.clone(),
                    confidence: score,
                })
            })
            .collect::<Vec<_>>();
        results.sort_by(|left, right| {
            right
                .confidence
                .partial_cmp(&left.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if top_k > 0 && results.len() > top_k {
            results.truncate(top_k);
        }
        results
    }
}

/// Tokenizes text with the same stemming and stop-word behavior used by search.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| word.len() >= 2 && !is_stop_word(word))
        .map(stem)
        .collect()
}

fn cosine(a: &BTreeMap<String, f64>, b: &BTreeMap<String, f64>) -> f64 {
    let dot = a
        .iter()
        .filter_map(|(key, a_value)| b.get(key).map(|b_value| a_value * b_value))
        .sum::<f64>();
    let norm_a = a.values().map(|value| value * value).sum::<f64>();
    let norm_b = b.values().map(|value| value * value).sum::<f64>();
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

fn stem(word: &str) -> String {
    for suffix in [
        "tion", "sion", "ment", "ness", "ing", "ous", "ive", "ful", "ed", "ly", "er", "es",
    ] {
        if word.len() > suffix.len() + 2 && word.ends_with(suffix) {
            return word[..word.len() - suffix.len()].to_owned();
        }
    }
    if word.len() > 3 && word.ends_with('s') {
        return word[..word.len() - 1].to_owned();
    }
    word.to_owned()
}

fn is_stop_word(word: &str) -> bool {
    matches!(
        word,
        "the"
            | "be"
            | "to"
            | "of"
            | "and"
            | "in"
            | "that"
            | "have"
            | "it"
            | "for"
            | "not"
            | "on"
            | "with"
            | "he"
            | "as"
            | "you"
            | "do"
            | "at"
            | "this"
            | "but"
            | "his"
            | "by"
            | "from"
            | "they"
            | "we"
            | "her"
            | "she"
            | "or"
            | "an"
            | "will"
            | "my"
            | "one"
            | "all"
            | "would"
            | "there"
            | "their"
            | "what"
            | "so"
            | "up"
            | "if"
            | "about"
            | "who"
            | "which"
            | "them"
            | "then"
            | "its"
            | "also"
            | "into"
            | "could"
            | "than"
            | "other"
            | "how"
            | "has"
            | "more"
            | "these"
            | "was"
            | "are"
            | "is"
            | "am"
            | "been"
    )
}
