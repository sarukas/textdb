//! Test families (test spec §7). Each function runs one test on one backend and records
//! metrics into the cell. Oracles compare against the reference model, never against
//! another backend.

pub mod cr;
pub mod cw;
pub mod du;
pub mod fp;
pub mod ll;
pub mod md;
pub mod me;
pub mod ns;
pub mod rt;
pub mod sr;
pub mod xl;

use crate::gen::{line_starts, Charset, GenOpts, Generator, LineEnding};
use crate::ops;
use crate::runner::Ctx;
use rand::Rng;

pub fn dispatch(kind: &str, ctx: &Ctx) -> anyhow::Result<()> {
    match kind {
        "roundtrip" => rt::roundtrip(ctx),
        "edit_sequence" => me::edit_sequence(ctx),
        "xl" => xl::xl(ctx),
        "long_lines" => ll::long_lines(ctx),
        "concurrent_writes" => cw::concurrent_writes(ctx),
        "concurrent_reads" => cr::concurrent_reads(ctx),
        "search" => sr::search(ctx),
        "namespace" => ns::namespace(ctx),
        "markdown" => md::markdown(ctx),
        "footprint" => fp::footprint(ctx),
        "durability" => du::durability(ctx),
        other => anyhow::bail!("unknown test kind {}", other),
    }
}

pub fn charset(s: &str) -> Charset {
    match s {
        "ascii" => Charset::Ascii,
        "random_bytes" => Charset::RandomBytes,
        _ => Charset::Mixed,
    }
}

pub fn line_ending(s: &str) -> LineEnding {
    match s {
        "crlf" => LineEnding::CrLf,
        "no_trailing" => LineEnding::NoTrailing,
        _ => LineEnding::Lf,
    }
}

pub fn opts(cs: Charset, le: LineEnding) -> GenOpts {
    GenOpts {
        charset: cs,
        line_ending: le,
        ..GenOpts::default()
    }
}

/// Human-readable size label for case names.
pub fn size_label(n: u64) -> String {
    if n >= 1 << 30 && n % (1 << 30) == 0 {
        format!("{}GiB", n >> 30)
    } else if n >= 1 << 20 && n % (1 << 20) == 0 {
        format!("{}MiB", n >> 20)
    } else if n >= 1 << 10 && n % (1 << 10) == 0 {
        format!("{}KiB", n >> 10)
    } else {
        format!("{}B", n)
    }
}

/// A line (without newline) as `(start, end)` byte offsets for 0-based line index.
pub fn line_span(body: &[u8], idx: usize) -> Option<(usize, usize)> {
    let starts = line_starts(body);
    let s = *starts.get(idx)?;
    let e = body[s..].iter().position(|&b| b == b'\n').map(|p| s + p).unwrap_or(body.len());
    Some((s, e))
}

pub fn n_lines(body: &[u8]) -> usize {
    line_starts(body).len()
}

/// Pick `k` consecutive lines starting at `idx` and return `(old_bytes, new_bytes)` where
/// the replacement keeps the line count and carries `tag`.
pub fn line_edit(body: &[u8], idx: usize, k: usize, tag: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let (s, _) = line_span(body, idx)?;
    let (_, e) = line_span(body, idx + k - 1).unwrap_or_else(|| line_span(body, n_lines(body) - 1).unwrap());
    let old = body[s..e].to_vec();
    let mut new = Vec::new();
    for i in 0..k {
        if i > 0 {
            new.push(b'\n');
        }
        new.extend_from_slice(format!("{} line{}", tag, i).as_bytes());
    }
    // Uniqueness: `old` may repeat elsewhere; widen with context when it does.
    if crate::reference::find_unique(body, &old).is_none() {
        return None;
    }
    Some((old, new))
}

/// Zipf(s) sampler over `n` items (rank 1 most popular), inverse-CDF.
pub struct Zipf {
    cdf: Vec<f64>,
}

impl Zipf {
    pub fn new(n: usize, s: f64) -> Self {
        let mut cdf = Vec::with_capacity(n);
        let mut acc = 0.0;
        for k in 1..=n {
            acc += 1.0 / (k as f64).powf(s);
            cdf.push(acc);
        }
        for v in &mut cdf {
            *v /= acc;
        }
        Zipf { cdf }
    }
    pub fn sample<R: Rng>(&self, rng: &mut R) -> usize {
        let u: f64 = rng.gen();
        match self.cdf.binary_search_by(|p| p.partial_cmp(&u).unwrap()) {
            Ok(i) => i,
            Err(i) => i.min(self.cdf.len() - 1),
        }
    }
}

/// Generate `n` markdown files of `size` bytes each into the backend and the reference.
pub fn import_corpus(
    ctx: &Ctx,
    reference: &mut crate::reference::Reference,
    n: usize,
    size: usize,
    prefix: &str,
) -> anyhow::Result<(crate::metrics::Latencies, Vec<String>)> {
    let mut g = Generator::new(ctx.seed);
    let o = GenOpts::default();
    let mut lat = crate::metrics::Latencies::default();
    let mut paths = Vec::with_capacity(n);
    for i in 0..n {
        let path = format!("{}/d{:02}/f{:05}.md", prefix, i % 50, i);
        let body = g.markdown(size, &o);
        ctx.op(ops::CREATE, &mut lat, || ctx.backend.create(&path, &body))?;
        reference.create(&path, &body);
        paths.push(path);
    }
    Ok((lat, paths))
}
