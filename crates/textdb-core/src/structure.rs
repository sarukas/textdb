//! Structure extractor plugin trait (spec §6.8). Core is format-agnostic.

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Section {
    /// Heading path, e.g. `"Intro / Goals"`.
    pub heading_path: String,
    pub level: u32,
    /// 1-based inclusive line span, heading line included.
    pub line_from: u64,
    pub line_to: u64,
    /// The last component of the heading path, as written: `"Goals"`.
    pub heading: String,
    /// Words in this section's own lines, heading line included.
    ///
    /// A word never spans a line and sections partition a document by line, so this is exact
    /// and the two figures compose: `nwords_total` is this plus every nested section's own.
    pub nwords: u64,
    /// Words in this section and everything nested under it.
    pub nwords_total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Link {
    /// The target as written, without `#anchor`, `|alias` or query; markdown links decoded.
    pub target_path: String,
    /// 1-based line.
    pub line: u64,
    /// `wiki`, `embed`, `md` or `image`.
    pub kind: String,
    /// `heading` or `^block` after `#`.
    pub anchor: Option<String>,
    pub alias: Option<String>,
    /// A URL, email address, query or numbered reference rather than a document.
    pub external: bool,
    /// The bytes of the target *as written* inside the document, when it is written in place.
    ///
    /// `None` for a reference link or an autolink, whose target is not where the link is. This
    /// is what lets a rewrite — a move, or the link projection of #12 — change a target without
    /// re-parsing free text, so code spans, fenced blocks, escaped brackets and URLs are left
    /// alone by construction rather than by a second, weaker parser agreeing with the first.
    pub span: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Structure {
    pub sections: Vec<Section>,
    pub links: Vec<Link>,
    pub frontmatter: Option<serde_json::Value>,
}

pub trait StructureExtractor: Send + Sync {
    fn extract(&self, bytes: &[u8]) -> Structure;
}

/// Extractor that finds nothing; used when no format plugin is installed.
pub struct NoStructure;

impl StructureExtractor for NoStructure {
    fn extract(&self, _bytes: &[u8]) -> Structure {
        Structure::default()
    }
}
