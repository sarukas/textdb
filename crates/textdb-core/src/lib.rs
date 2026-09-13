//! textdb-core: engine-agnostic storage and query algorithms for versioned text.
//!
//! See `docs/spec.md` in the repository for the design. This crate owns every
//! algorithm (chunker, tree, hashing, materialize, locate, edit, rebase, diff) and has
//! no database dependency; bindings implement [`Storage`] and own persistence.

pub mod chunker;
pub mod commit;
pub mod diff;
pub mod edit;
pub mod hash;
pub mod myers;
pub mod node;
pub mod storage;
pub mod structure;
pub mod tree;

pub use chunker::ChunkParams;
pub use commit::{commit, commit_append, CommitKind, Committed, ConflictInfo, DEFAULT_RETRIES};
pub use diff::{changed_runs, line_hunks, unified_diff, ChangedRun, LineHunk};
pub use edit::{apply_edits, Edit, EditResult};
pub use hash::{hash_chunk, hash_node, Hash};
pub use node::{Child, Node};
pub use storage::{MemStorage, Storage};
pub use structure::{Link, NoStructure, Section, Structure, StructureExtractor};
pub use tree::{build, build_with_chunks, leaves, lines, locate_byte, locate_line, materialize, materialize_range, totals, LeafRef};

#[derive(Debug, thiserror::Error)]
pub enum TextdbError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("missing chunk {}", hash::hex(.0))]
    MissingChunk(Hash),
    #[error("missing node {}", hash::hex(.0))]
    MissingNode(Hash),
    #[error("invalid edit: {0}")]
    InvalidEdit(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict in {} lines {}-{}", .0.path, .0.region_line_from, .0.region_line_to)]
    Conflict(Box<ConflictInfo>),
    #[error("contention: retry budget exhausted")]
    Contention,
    #[error("{0}")]
    Other(String),
}

impl TextdbError {
    /// SQLSTATE-style code used by the engine bindings (spec §7.2).
    pub fn code(&self) -> &'static str {
        match self {
            TextdbError::Conflict(_) => "TX001",
            TextdbError::Contention => "TX002",
            TextdbError::NotFound(_) => "TX003",
            TextdbError::InvalidEdit(_) => "TX004",
            _ => "TX000",
        }
    }
}
