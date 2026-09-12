//! Deterministic content generator (test spec §6).

use rand::distributions::Distribution;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::LogNormal;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Charset {
    Ascii,
    Mixed,
    RandomBytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineEnding {
    Lf,
    CrLf,
    /// `\n` but no newline at EOF.
    NoTrailing,
}

#[derive(Clone, Copy, Debug, serde::Deserialize)]
pub struct GenOpts {
    pub charset: Charset,
    pub line_ending: LineEnding,
    /// Headings per KB.
    pub heading_density: f64,
    pub structure: bool,
    pub frontmatter: bool,
}

impl Default for GenOpts {
    fn default() -> Self {
        GenOpts {
            charset: Charset::Mixed,
            line_ending: LineEnding::Lf,
            heading_density: 0.5,
            structure: true,
            frontmatter: true,
        }
    }
}

const SYLLABLES: &[&str] = &[
    "ka", "to", "ri", "ma", "ne", "lu", "so", "vi", "da", "pe", "ro", "ti", "ga", "mu", "le", "no", "sa", "ve", "di", "ba",
    "ren", "tal", "mir", "kos", "lun", "der", "vok", "sim", "bel", "tar",
];
const UNICODE_WORDS: &[&str] = &[
    "ąžuolas", "šviesa", "žemė", "ėglė", "ūkas", "čiurlys", "įrašas", "客户", "数据", "平台", "😀", "🚀", "naïve", "façade",
    "Ελλάδα", "Москва",
];

/// Vocabulary of `n` deterministic words (ASCII).
pub fn vocabulary(n: usize) -> Vec<String> {
    let mut rng = StdRng::seed_from_u64(0x5eed_1000);
    (0..n)
        .map(|_| {
            let k = rng.gen_range(1..=3);
            (0..k).map(|_| SYLLABLES[rng.gen_range(0..SYLLABLES.len())]).collect::<String>()
        })
        .collect()
}

pub struct Generator {
    rng: StdRng,
    vocab: Vec<String>,
    line_len: LogNormal<f64>,
}

impl Generator {
    pub fn new(seed: u64) -> Self {
        // median 80 B, p99 ≈ 400 B  →  σ = ln(5)/2.326
        let sigma = (400.0f64 / 80.0).ln() / 2.326;
        Generator {
            rng: StdRng::seed_from_u64(seed),
            vocab: vocabulary(2000),
            line_len: LogNormal::new(80.0f64.ln(), sigma).unwrap(),
        }
    }

    fn word(&mut self, cs: Charset) -> String {
        match cs {
            Charset::Mixed if self.rng.gen_bool(0.12) => UNICODE_WORDS[self.rng.gen_range(0..UNICODE_WORDS.len())].to_string(),
            _ => {
                // Zipf-ish: favour low indices.
                let i = ((self.rng.gen::<f64>().powf(2.5)) * self.vocab.len() as f64) as usize;
                self.vocab[i.min(self.vocab.len() - 1)].clone()
            }
        }
    }

    fn text_line(&mut self, target: usize, cs: Charset) -> String {
        let mut s = String::new();
        while s.len() < target {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(&self.word(cs));
        }
        s
    }

    /// Markdown document of approximately `bytes` bytes.
    pub fn markdown(&mut self, bytes: usize, o: &GenOpts) -> Vec<u8> {
        if o.charset == Charset::RandomBytes {
            let mut v = vec![0u8; bytes];
            self.rng.fill(&mut v[..]);
            return v;
        }
        let nl: &str = match o.line_ending {
            LineEnding::CrLf => "\r\n",
            _ => "\n",
        };
        let mut out = String::with_capacity(bytes + 512);
        if o.frontmatter && o.structure && bytes > 200 {
            out.push_str(&format!(
                "---{nl}title: {}{nl}tags: [{}, {}]{nl}draft: false{nl}---{nl}",
                self.text_line(20, o.charset),
                self.word(o.charset),
                self.word(o.charset),
                nl = nl
            ));
        }
        let heading_every = if o.heading_density > 0.0 { (1024.0 / o.heading_density) as usize } else { usize::MAX };
        let mut since_heading = heading_every; // start with a heading
        let mut level = 1u32;
        while out.len() < bytes {
            if o.structure && since_heading >= heading_every {
                level = if level >= 3 { 1 } else { self.rng.gen_range(1..=3) };
                out.push_str(&format!("{} {}{nl}{nl}", "#".repeat(level as usize), self.text_line(24, o.charset), nl = nl));
                since_heading = 0;
                continue;
            }
            let before = out.len();
            let kind = if o.structure { self.rng.gen_range(0..10) } else { 0 };
            match kind {
                0..=5 => {
                    let n = self.rng.gen_range(1..=6);
                    for _ in 0..n {
                        let len = self.line_len.sample(&mut self.rng).clamp(1.0, 2000.0) as usize;
                        out.push_str(&self.text_line(len, o.charset));
                        out.push_str(nl);
                    }
                    out.push_str(nl);
                }
                6 => {
                    let n = self.rng.gen_range(2..=7);
                    for _ in 0..n {
                        out.push_str(&format!("- {}{nl}", self.text_line(40, o.charset), nl = nl));
                    }
                    out.push_str(nl);
                }
                7 => {
                    out.push_str(&format!("```{nl}", nl = nl));
                    let n = self.rng.gen_range(2..=8);
                    for i in 0..n {
                        out.push_str(&format!("let v{} = {}({});{nl}", i, self.word(Charset::Ascii), self.rng.gen::<u16>(), nl = nl));
                    }
                    out.push_str(&format!("```{nl}{nl}", nl = nl));
                }
                8 => {
                    out.push_str(&format!("| key | value |{nl}|---|---|{nl}", nl = nl));
                    let n = self.rng.gen_range(2..=5);
                    for _ in 0..n {
                        out.push_str(&format!("| {} | {} |{nl}", self.word(o.charset), self.rng.gen::<u32>(), nl = nl));
                    }
                    out.push_str(nl);
                }
                _ => {
                    out.push_str(&format!(
                        "See [[{}]] and [{}](./{}.md).{nl}{nl}",
                        self.text_line(12, o.charset),
                        self.word(o.charset),
                        self.word(Charset::Ascii),
                        nl = nl
                    ));
                }
            }
            since_heading += out.len() - before;
        }
        // Trim to the exact size on a character boundary, then fix the ending.
        let mut cut = bytes.min(out.len());
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        let mut v = out.into_bytes();
        match o.line_ending {
            LineEnding::Lf => {
                if !v.is_empty() && v.last() != Some(&b'\n') && v.len() == bytes {
                    // Replace the last byte(s) to end on a newline, keeping the size exact.
                    let mut end = v.len() - 1;
                    while end > 0 && !std::str::from_utf8(&v[..end]).is_ok() {
                        end -= 1;
                    }
                    v.truncate(end);
                    while v.len() < bytes {
                        v.push(b'\n');
                    }
                }
            }
            LineEnding::CrLf => {
                if v.len() >= 2 && &v[v.len() - 2..] != b"\r\n" && v.len() == bytes {
                    let mut end = v.len() - 2;
                    while end > 0 && !std::str::from_utf8(&v[..end]).is_ok() {
                        end -= 1;
                    }
                    v.truncate(end);
                    while v.len() + 2 < bytes {
                        v.push(b' ');
                    }
                    v.extend_from_slice(b"\r\n");
                    v.truncate(bytes);
                }
            }
            LineEnding::NoTrailing => {
                while v.last() == Some(&b'\n') || v.last() == Some(&b'\r') {
                    v.pop();
                }
            }
        }
        v
    }

    /// One line of `bytes` ASCII words, no newline.
    pub fn single_line(&mut self, bytes: usize, cs: Charset) -> Vec<u8> {
        let mut s = self.text_line(bytes, cs);
        let mut cut = bytes.min(s.len());
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.into_bytes()
    }

    /// Minified-JSON-like content: no newlines, high entropy.
    pub fn json_like(&mut self, bytes: usize) -> Vec<u8> {
        let mut out = String::with_capacity(bytes + 64);
        out.push('[');
        let mut first = true;
        while out.len() < bytes {
            if !first {
                out.push(',');
            }
            first = false;
            let vlen = self.rng.gen_range(5..40);
            let id = self.rng.gen::<u64>();
            let k = self.rng.gen::<u32>();
            out.push_str(&format!(
                "{{\"id\":\"{:016x}\",\"k\":{},\"v\":\"{}\"}}",
                id,
                k,
                self.text_line(vlen, Charset::Ascii)
            ));
        }
        out.truncate(bytes.min(out.len()));
        out.into_bytes()
    }

    /// `n` lines of exactly `len` bytes each (including the newline).
    pub fn fixed_lines(&mut self, n: usize, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n * len);
        for i in 0..n {
            let prefix = format!("L{:07} ", i);
            let mut l = prefix.into_bytes();
            let body = self.text_line(len - l.len() - 1, Charset::Ascii);
            l.extend_from_slice(body.as_bytes());
            l.truncate(len - 1);
            while l.len() < len - 1 {
                l.push(b' ');
            }
            l.push(b'\n');
            out.extend_from_slice(&l);
        }
        out
    }

    pub fn rng(&mut self) -> &mut StdRng {
        &mut self.rng
    }

    pub fn vocab(&self) -> &[String] {
        &self.vocab
    }
}

/// Line start offsets (0-based) of a byte string.
pub fn line_starts(b: &[u8]) -> Vec<usize> {
    let mut v = vec![0];
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' && i + 1 < b.len() {
            v.push(i + 1);
        }
    }
    v
}
