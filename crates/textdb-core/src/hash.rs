//! Hashing. BLAKE3-256 behind a type alias so it can be swapped (spec §5.1).

/// 32-byte content hash.
pub type Hash = [u8; 32];

const NODE_DOMAIN: &[u8] = b"textdb-node-v1\0";

/// Hash of a chunk = BLAKE3 of its bytes.
pub fn hash_chunk(bytes: &[u8]) -> Hash {
    *blake3::hash(bytes).as_bytes()
}

/// Hash of a tree node = BLAKE3 of a domain tag followed by the canonical child encoding.
pub fn hash_node(encoded: &[u8]) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(NODE_DOMAIN);
    h.update(encoded);
    *h.finalize().as_bytes()
}

pub fn hex(h: &Hash) -> String {
    let mut s = String::with_capacity(64);
    for b in h {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

pub fn from_hex(s: &str) -> Option<Hash> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}
