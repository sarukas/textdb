//! Content-defined chunker: FastCDC-style gear hash with normalized chunking and a
//! forward newline snap (spec §6.1).
//!
//! Determinism requirement D1: boundaries are a pure function of the bytes from the
//! start of the current chunk, so identical content always yields identical chunks.

/// Chunker parameters. All sizes in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkParams {
    pub min: usize,
    pub avg: usize,
    pub max: usize,
    /// Forward search distance for a `\n` after a CDC boundary.
    pub snap: usize,
}

impl ChunkParams {
    pub const DEFAULT: ChunkParams = ChunkParams {
        min: 512,
        avg: 1024,
        max: 4096,
        snap: 256,
    };

    /// Number of bytes that must be available past a chunk start for a boundary
    /// decision to be final when more input may follow.
    pub const fn lookahead(&self) -> usize {
        self.max + self.snap
    }

    const fn bits(&self) -> u32 {
        self.avg.trailing_zeros()
    }

    /// Mask used before `avg` (more bits set: harder to hit).
    pub const fn mask_s(&self) -> u64 {
        spread_mask(self.bits() + 1)
    }

    /// Mask used after `avg` (fewer bits set: easier to hit).
    pub const fn mask_l(&self) -> u64 {
        spread_mask(self.bits() - 1)
    }
}

impl Default for ChunkParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `k` mask bits spread over the upper half of the word, where the gear hash mixes best.
const fn spread_mask(k: u32) -> u64 {
    let mut m = 0u64;
    let mut j = 0;
    while j < k {
        m |= 1u64 << (16 + 2 * j);
        j += 1;
    }
    m
}

const fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

const fn gear_table() -> [u64; 256] {
    let mut t = [0u64; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = splitmix64(0x7465787464622d67 ^ (i as u64));
        i += 1;
    }
    t
}

/// Fixed random table; part of the on-disk format (changing it changes every root).
pub static GEAR: [u64; 256] = gear_table();

/// Decide the length of the chunk starting at `buf[0]`.
///
/// Returns `None` when the decision cannot be final yet: `eof` is false and fewer than
/// `params.lookahead()` bytes are available. When `eof` is true the whole buffer is
/// the tail of the document and a decision is always returned (possibly `buf.len()`).
pub fn cut(params: &ChunkParams, buf: &[u8], eof: bool) -> Option<usize> {
    let n = buf.len();
    if !eof && n < params.lookahead() {
        return None;
    }
    if n <= params.min {
        return if eof { Some(n) } else { None };
    }
    let limit = n.min(params.max);
    let mask_s = params.mask_s();
    let mask_l = params.mask_l();
    let mut h: u64 = 0;
    let mut boundary = limit; // default: cut at max (or at end)
    let mut i = params.min;
    let avg = params.avg.min(limit);
    while i < avg {
        h = (h << 1).wrapping_add(GEAR[buf[i] as usize]);
        if h & mask_s == 0 {
            boundary = i + 1;
            return Some(snap(params, buf, boundary));
        }
        i += 1;
    }
    while i < limit {
        h = (h << 1).wrapping_add(GEAR[buf[i] as usize]);
        if h & mask_l == 0 {
            boundary = i + 1;
            return Some(snap(params, buf, boundary));
        }
        i += 1;
    }
    if boundary == n && eof {
        // Reached the end of the document before max: the rest is one chunk.
        return Some(n);
    }
    Some(snap(params, buf, boundary))
}

/// Move a boundary forward to just after the next `\n` if one occurs within `snap` bytes.
fn snap(params: &ChunkParams, buf: &[u8], boundary: usize) -> usize {
    let end = (boundary + params.snap).min(buf.len());
    if boundary >= end {
        return boundary;
    }
    match buf[boundary..end].iter().position(|&b| b == b'\n') {
        Some(p) => boundary + p + 1,
        None => boundary,
    }
}

/// Chunk a complete buffer into `(start, len)` pairs.
pub fn chunk_all(params: &ChunkParams, bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(bytes.len() / params.avg + 1);
    let mut pos = 0;
    while pos < bytes.len() {
        let len = cut(params, &bytes[pos..], true).expect("eof cut is always decided");
        debug_assert!(len > 0);
        out.push((pos, len));
        pos += len;
    }
    out
}

pub fn count_newlines(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|&&b| b == b'\n').count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_input_and_respect_bounds() {
        let p = ChunkParams::DEFAULT;
        let mut data = Vec::new();
        let mut x = 1u64;
        for _ in 0..200_000 {
            x = splitmix64(x);
            data.push((x & 0x7f) as u8);
            if x % 90 == 0 {
                data.push(b'\n');
            }
        }
        let chunks = chunk_all(&p, &data);
        let mut pos = 0;
        for (s, l) in &chunks {
            assert_eq!(*s, pos);
            assert!(*l <= p.max + p.snap, "chunk too large: {}", l);
            pos += l;
        }
        assert_eq!(pos, data.len());
        for (_, l) in &chunks[..chunks.len() - 1] {
            assert!(*l >= p.min);
        }
        let mean = data.len() / chunks.len();
        assert!(mean > 600 && mean < 2500, "mean chunk {}", mean);
    }

    #[test]
    fn incremental_decisions_match_eof_decisions() {
        let p = ChunkParams::DEFAULT;
        let mut data = Vec::new();
        let mut x = 7u64;
        for _ in 0..50_000 {
            x = splitmix64(x);
            data.push((x & 0xff) as u8);
        }
        let full = chunk_all(&p, &data);
        let mut pos = 0;
        for (s, l) in full {
            assert_eq!(s, pos);
            let avail = (data.len() - pos).min(p.lookahead());
            let decided = cut(&p, &data[pos..pos + avail], pos + avail == data.len());
            assert_eq!(decided, Some(l));
            pos += l;
        }
    }
}
