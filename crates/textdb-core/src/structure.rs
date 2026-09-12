//! Structure extractor plugin trait (spec §6.8). Core is format-agnostic.

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Section {
    /// Heading path, e.g. `"Intro / Goals"`.
    pub heading_path: String,
    pub level: u32,
    /// 1-based inclusive line span.
    pub line_from: u64,
    pub line_to: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Link {
    pub target_path: String,
    /// 1-based line.
    pub line: u64,
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
