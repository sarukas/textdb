//! Tree nodes and their canonical encoding (spec §5.1).

use crate::hash::{hash_node, Hash};

/// One entry of an internal node: a child subtree (or a leaf chunk) with its totals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Child {
    pub hash: Hash,
    pub nbytes: u64,
    pub nlines: u64,
    pub is_leaf: bool,
}

/// Internal node of the prolly tree. Leaves (chunks) are not nodes.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Node {
    pub children: Vec<Child>,
}

pub const CHILD_ENC_LEN: usize = 32 + 8 + 8 + 1;

impl Node {
    pub fn new(children: Vec<Child>) -> Self {
        Node { children }
    }

    pub fn nbytes(&self) -> u64 {
        self.children.iter().map(|c| c.nbytes).sum()
    }

    pub fn nlines(&self) -> u64 {
        self.children.iter().map(|c| c.nlines).sum()
    }

    /// Canonical encoding: concatenation of `hash || nbytes_le || nlines_le || is_leaf`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.children.len() * CHILD_ENC_LEN);
        for c in &self.children {
            out.extend_from_slice(&c.hash);
            out.extend_from_slice(&c.nbytes.to_le_bytes());
            out.extend_from_slice(&c.nlines.to_le_bytes());
            out.push(c.is_leaf as u8);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Node> {
        if bytes.len() % CHILD_ENC_LEN != 0 {
            return None;
        }
        let mut children = Vec::with_capacity(bytes.len() / CHILD_ENC_LEN);
        for ch in bytes.chunks_exact(CHILD_ENC_LEN) {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&ch[..32]);
            let nbytes = u64::from_le_bytes(ch[32..40].try_into().unwrap());
            let nlines = u64::from_le_bytes(ch[40..48].try_into().unwrap());
            let is_leaf = match ch[48] {
                0 => false,
                1 => true,
                _ => return None,
            };
            children.push(Child {
                hash,
                nbytes,
                nlines,
                is_leaf,
            });
        }
        Some(Node { children })
    }

    pub fn hash(&self) -> Hash {
        hash_node(&self.encode())
    }

    /// The entry this node contributes to its parent.
    pub fn as_child(&self) -> Child {
        Child {
            hash: self.hash(),
            nbytes: self.nbytes(),
            nlines: self.nlines(),
            is_leaf: false,
        }
    }
}
