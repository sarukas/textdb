//! The `Storage` trait bindings implement (spec §4) plus an in-memory implementation.

use std::collections::HashMap;

use crate::hash::Hash;
use crate::node::Node;
use crate::TextdbError;

pub type Result<T> = std::result::Result<T, TextdbError>;

/// Persistence abstraction. All chunk/node writes are append-only and idempotent; the
/// only mutation is `cas_root`.
pub trait Storage {
    fn get_chunk(&self, h: &Hash) -> Result<Option<Vec<u8>>>;
    fn put_chunk(&mut self, h: &Hash, b: &[u8]) -> Result<()>;
    fn get_node(&self, h: &Hash) -> Result<Option<Node>>;
    fn put_node(&mut self, h: &Hash, n: &Node) -> Result<()>;
    /// Current `(root, version)` of a file, if the file exists.
    fn get_root(&self, file_id: u64) -> Result<Option<(Hash, u64)>>;
    /// Compare-and-swap the root. `expect == None` means "file has no root yet".
    /// Returns `true` on success. On success the version is incremented by one.
    fn cas_root(&mut self, file_id: u64, expect: Option<&Hash>, new: &Hash) -> Result<bool>;

    /// Convenience: fetch a node or fail.
    fn node(&self, h: &Hash) -> Result<Node> {
        self.get_node(h)?.ok_or(TextdbError::MissingNode(*h))
    }

    /// Convenience: fetch a chunk or fail.
    fn chunk(&self, h: &Hash) -> Result<Vec<u8>> {
        self.get_chunk(h)?.ok_or(TextdbError::MissingChunk(*h))
    }

    /// Fetch a chunk as a shared buffer.
    ///
    /// Materialising a document copies every chunk into the output, so a binding that
    /// already holds the bytes — from a cache, say — should not have to hand out a fresh
    /// `Vec` only for the caller to copy out of it and drop it. The default allocates, as
    /// `chunk` does.
    fn chunk_shared(&self, h: &Hash) -> Result<std::sync::Arc<Vec<u8>>> {
        Ok(std::sync::Arc::new(self.chunk(h)?))
    }
}

/// In-memory storage used for tests and as the Stage 0 reference implementation.
#[derive(Default, Debug, Clone)]
pub struct MemStorage {
    pub chunks: HashMap<Hash, Vec<u8>>,
    pub nodes: HashMap<Hash, Node>,
    pub roots: HashMap<u64, (Hash, u64)>,
    /// Number of chunk bytes written (counting only new chunks).
    pub chunk_bytes_written: u64,
    pub node_writes: u64,
}

impl MemStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for MemStorage {
    fn get_chunk(&self, h: &Hash) -> Result<Option<Vec<u8>>> {
        Ok(self.chunks.get(h).cloned())
    }
    fn put_chunk(&mut self, h: &Hash, b: &[u8]) -> Result<()> {
        if !self.chunks.contains_key(h) {
            self.chunk_bytes_written += b.len() as u64;
            self.chunks.insert(*h, b.to_vec());
        }
        Ok(())
    }
    fn get_node(&self, h: &Hash) -> Result<Option<Node>> {
        Ok(self.nodes.get(h).cloned())
    }
    fn put_node(&mut self, h: &Hash, n: &Node) -> Result<()> {
        if !self.nodes.contains_key(h) {
            self.node_writes += 1;
            self.nodes.insert(*h, n.clone());
        }
        Ok(())
    }
    fn get_root(&self, file_id: u64) -> Result<Option<(Hash, u64)>> {
        Ok(self.roots.get(&file_id).copied())
    }
    fn cas_root(&mut self, file_id: u64, expect: Option<&Hash>, new: &Hash) -> Result<bool> {
        let cur = self.roots.get(&file_id).map(|(h, _)| *h);
        if cur.as_ref() != expect {
            return Ok(false);
        }
        let v = self.roots.get(&file_id).map(|(_, v)| *v).unwrap_or(0);
        self.roots.insert(file_id, (*new, v + 1));
        Ok(true)
    }
}
