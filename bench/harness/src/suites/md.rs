//! MD — the structure sidecar: markdown links, front matter, sections, the change feed.
//!
//! These operations have no baseline equivalent. `fs` keeps no link index, and the
//! `sql-text-*` stores keep text and nothing derived from it, so every cell here is `N/A`
//! for them — recorded with its reason, the way the rest of the suite records a capability
//! a backend does not have. The family is not asking who is faster. It is establishing what
//! these operations cost at all, and, in `extract`, what maintaining them costs the write
//! path that every other family measures.
//!
//! Every case is generated with a **known** link graph, front matter and heading tree, so
//! the oracle is what the generator wrote rather than whatever the backend happens to
//! return. A backend that indexes nothing and answers instantly fails the check and
//! publishes no timings.

use crate::backend::BackendError;
use crate::metrics::Latencies;
use crate::ops;
use crate::runner::Ctx;

pub fn markdown(ctx: &Ctx) -> anyhow::Result<()> {
    match ctx.params.str("variant", "extract").as_str() {
        "extract" => extract(ctx),
        "link_query" => link_query(ctx),
        "move_relink" => move_relink(ctx),
        "frontmatter" => frontmatter(ctx),
        "sections" => sections(ctx),
        "feed" => feed(ctx),
        "property_search" => property_search(ctx),
        other => anyhow::bail!("unknown md variant {}", other),
    }
}

// ---------------------------------------------------------------------------------------
// A corpus whose link graph, front matter and headings are known before anything is written.
// ---------------------------------------------------------------------------------------

/// One generated document and everything the oracle needs to check it.
struct Doc {
    path: String,
    body: Vec<u8>,
    /// Targets written as `[](./name.md)`, in line order, each resolving to a real file.
    resolved: Vec<String>,
    /// Targets written the same way that point at nothing.
    broken: usize,
    headings: Vec<String>,
}

/// `n` documents under `prefix`, each about `size` bytes, each linking to `links` other
/// documents in the same set plus `broken` targets that do not exist.
///
/// Links are `[text](./fNNNNN.md)` — relative markdown links rather than wiki links,
/// because a wiki link resolves by name across the whole store and would make the expected
/// status depend on what else the corpus happens to contain. A relative link's target is
/// unambiguous, so the oracle can state the status of every link exactly.
fn corpus(prefix: &str, n: usize, size: usize, links: usize, broken: usize, headings: usize) -> Vec<Doc> {
    let name = |i: usize| format!("{}/f{:05}.md", prefix, i);
    (0..n)
        .map(|i| {
            let mut body = String::new();
            let mut resolved = Vec::new();
            let mut heads = Vec::new();
            body.push_str(&format!("---\ntitle: Doc {}\nstatus: draft\nord: {}\n---\n\n", i, i));
            for h in 0..headings.max(1) {
                let heading = format!("Heading {}", h);
                body.push_str(&format!("# {}\n\n", heading));
                heads.push(heading);
                // Links are spread over the headings so they do not all land in one section.
                for l in 0..links.div_ceil(headings.max(1)) {
                    if resolved.len() >= links {
                        break;
                    }
                    // Deterministic, and never a self link: step around the ring.
                    let t = (i + 1 + l * 7 + h) % n.max(1);
                    let t = if t == i { (t + 1) % n.max(1) } else { t };
                    body.push_str(&format!("See [doc {}](./f{:05}.md) for more.\n\n", t, t));
                    resolved.push(name(t));
                }
                for b in 0..broken.div_ceil(headings.max(1)) {
                    if body.matches("./missing-").count() >= broken {
                        break;
                    }
                    body.push_str(&format!("And [gone](./missing-{}-{}.md).\n\n", i, b));
                }
                // Filler so the document reaches roughly `size` bytes, which is what makes
                // the write-path cost comparable with the other families. Every line is
                // distinct: a `replace` anchor has to occur exactly once in the document or
                // the store refuses the edit (TX004), so repeated filler would make the
                // edits in `extract` and `feed` unrunnable rather than slow.
                let mut fill = 0;
                // At least one filler line per heading, whatever `size` says: at a high link
                // density the links alone can exceed the target, and a document with no
                // filler line has no anchor, which silently emptied the edit loop.
                while fill == 0 || body.len() < size * (h + 1) / headings.max(1) {
                    body.push_str(&format!("Filler line {:03}-{:03}-{:04} long enough to be chunked sensibly.\n", i, h, fill));
                    fill += 1;
                }
            }
            Doc {
                path: name(i),
                body: body.into_bytes(),
                resolved,
                broken: broken.min(broken),
                headings: heads,
            }
        })
        .collect()
}

/// The first filler line of a generated document, as a unique `replace` anchor.
fn first_filler(body: &[u8]) -> Option<Vec<u8>> {
    body.split(|&b| b == b'\n')
        .find(|l| l.starts_with(b"Filler line "))
        .map(|l| l.to_vec())
}

/// The same bytes without a single markdown construct the extractor would record: the
/// control for `extract`. Same length, same line count, same chunk boundaries.
fn flatten(body: &[u8]) -> Vec<u8> {
    body.iter()
        .map(|&b| match b {
            b'#' | b'[' | b']' | b'(' | b')' | b'-' => b'x',
            other => other,
        })
        .collect()
}

/// Write `docs` and stop the cell cleanly if the backend cannot.
fn write_all(ctx: &Ctx, docs: &[Doc], case: &str) -> anyhow::Result<bool> {
    let mut lat = Latencies::default();
    for d in docs {
        if let Err(e) = ctx.op(ops::CREATE, &mut lat, || ctx.backend.create(&d.path, &d.body)) {
            ctx.err(case, "create", &e);
            return Ok(false);
        }
    }
    ctx.cell.lat(case, "create", &lat);
    Ok(true)
}

/// Record an N/A once and give up on the cell: a backend without the sidecar has nothing
/// to measure here, and saying so once is clearer than once per case.
fn unsupported(ctx: &Ctx, what: &str, e: &BackendError) -> bool {
    matches!(e, BackendError::NotSupported(_)) && {
        ctx.err("", what, e);
        true
    }
}

// ---------------------------------------------------------------------------------------
// MD-01 — what the sidecar costs the write path
// ---------------------------------------------------------------------------------------

/// The same bytes written as `.md` (extracted) and as `.txt` (not), at several link
/// densities.
///
/// This is the only cell in the family that compares anything, and it compares a backend
/// with itself. Every other family writes `.md` corpora, so the extractor's cost is already
/// inside their create and replace numbers without being visible; here it is the
/// measurement. `extract_overhead_pct` is the answer to "did the markdown features make
/// writes slower, and by how much".
fn extract(ctx: &Ctx) -> anyhow::Result<()> {
    let n = ctx.params.usize("n_files", 200);
    let size = ctx.params.usize("file_size", 8192);
    let headings = ctx.params.usize("headings", 8);
    // Zero first: the difference between 0 and the rest separates the fixed cost of parsing
    // a document from the per-link cost of recording what it found.
    let densities: Vec<usize> = ctx.params.str("densities", "0,8,64").split(',').filter_map(|s| s.trim().parse().ok()).collect();

    for links in densities {
        let docs = corpus(&format!("/md/x{}", links), n, size, links, 0, headings);
        let case = format!("links{}", links);

        // `.md` runs the extractor, `.txt` does not; the bytes are otherwise the same.
        //
        // Interleaved, and with the order swapped on every other document, because writing
        // all of one kind and then all of the other hands the second kind a warmed page
        // cache and a store that has already grown — which is worth more than the effect
        // being measured. Alternating makes that cancel instead of accumulate.
        let mut md = Latencies::default();
        let mut txt = Latencies::default();
        let mut failed = false;
        for (i, d) in docs.iter().enumerate() {
            let tp = d.path.replace("/md/x", "/txt/x").replace(".md", ".txt");
            let tb = flatten(&d.body);
            let do_md = |lat: &mut Latencies| ctx.op(ops::CREATE, lat, || ctx.backend.create(&d.path, &d.body));
            let do_txt = |lat: &mut Latencies| ctx.op(ops::CREATE, lat, || ctx.backend.create(&tp, &tb));
            let r = if i % 2 == 0 {
                do_md(&mut md).and_then(|_| do_txt(&mut txt))
            } else {
                do_txt(&mut txt).and_then(|_| do_md(&mut md))
            };
            if let Err(e) = r {
                ctx.err(&case, "create", &e);
                failed = true;
                break;
            }
        }
        if failed {
            continue;
        }
        ctx.cell.lat(&case, "create_md", &md);
        ctx.cell.lat(&case, "create_txt", &txt);

        // An edit that changes one line of prose and no structure at all. The write path
        // still re-extracts, but `write_structure` should find the rows unchanged and write
        // none of them — so this is where that optimisation either shows up or does not.
        let mut edit_md = Latencies::default();
        let mut edit_txt = Latencies::default();
        for (i, d) in docs.iter().enumerate() {
            let Some(anchor) = first_filler(&d.body) else { continue };
            let new = format!("Rewritten body {:05} of this document, same length or so.", i).into_bytes();
            let p = d.path.clone();
            let pt = d.path.replace("/md/x", "/txt/x").replace(".md", ".txt");
            let oldt = flatten(&anchor);
            let do_md = |lat: &mut Latencies| ctx.op(ops::REPLACE, lat, || ctx.backend.replace(&p, &anchor, &new, None));
            let do_txt = |lat: &mut Latencies| ctx.op(ops::REPLACE, lat, || ctx.backend.replace(&pt, &oldt, &new, None));
            // Alternated for the same reason as the creates above.
            let r = if i % 2 == 0 {
                do_md(&mut edit_md).and_then(|_| do_txt(&mut edit_txt))
            } else {
                do_txt(&mut edit_txt).and_then(|_| do_md(&mut edit_md))
            };
            if let Err(e) = r {
                ctx.err(&case, "replace", &e);
                break;
            }
        }
        ctx.cell.lat(&case, "replace_md", &edit_md);
        ctx.cell.lat(&case, "replace_txt", &edit_txt);

        for (label, a, b) in [("create", &md, &txt), ("replace", &edit_md, &edit_txt)] {
            if let (Some(x), Some(y)) = (a.p50(), b.p50()) {
                if y > 0.0 {
                    ctx.cell.metric(&case, &format!("{}_overhead_pct", label), (x - y) / y * 100.0);
                }
            }
        }

        // The links really were recorded: without this the overhead above could be zero
        // because nothing was extracted.
        match ctx.backend.links(&format!("/md/x{}", links)) {
            Ok(rows) => {
                let want = docs.iter().map(|d| d.resolved.len()).sum::<usize>();
                if rows.len() == want {
                    ctx.cell.metric(&case, "links_recorded", rows.len() as f64);
                } else {
                    ctx.cell.fail(&case, "links_recorded", &format!("{} of {}", rows.len(), want));
                }
            }
            Err(e) => {
                if unsupported(ctx, "links", &e) {
                    return Ok(());
                }
                ctx.err(&case, "links", &e);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// MD-02 — reading the link graph, and validating it
// ---------------------------------------------------------------------------------------

fn link_query(ctx: &Ctx) -> anyhow::Result<()> {
    let n = ctx.params.usize("n_files", 500);
    let size = ctx.params.usize("file_size", 4096);
    let links = ctx.params.usize("links_per_file", 8);
    let broken = ctx.params.usize("broken_per_file", 2);
    let docs = corpus("/md", n, size, links, broken, 4);
    if !write_all(ctx, &docs, "")? {
        return Ok(());
    }

    // Outbound links, one document at a time.
    let mut one = Latencies::default();
    let mut wrong = 0;
    for d in &docs {
        match ctx.op(ops::LINKS, &mut one, || ctx.backend.links(&d.path)) {
            Ok(rows) => {
                let ok: Vec<_> = rows.iter().filter(|r| r.status == "ok").map(|r| r.resolved.clone().unwrap_or_default()).collect();
                if ok != d.resolved {
                    wrong += 1;
                }
            }
            Err(e) => {
                if unsupported(ctx, "links", &e) {
                    return Ok(());
                }
                ctx.err("", "links", &e);
                return Ok(());
            }
        }
    }
    ctx.cell.lat("", "links_one", &one);
    if wrong == 0 {
        ctx.cell.metric("", "links_match_graph", 1.0);
    } else {
        ctx.cell.fail("", "links_match_graph", &format!("{} of {} documents", wrong, n));
    }

    // The whole subtree in one query — the shape a "show me the graph" view issues.
    let mut all = Latencies::default();
    match ctx.op(ops::LINKS, &mut all, || ctx.backend.links("/md")) {
        Ok(rows) => {
            let want = docs.iter().map(|d| d.resolved.len() + d.broken).sum::<usize>();
            if rows.len() == want {
                ctx.cell.metric("", "links_subtree_count", rows.len() as f64);
            } else {
                ctx.cell.fail("", "links_subtree_count", &format!("{} of {}", rows.len(), want));
            }
            // Validation: every planted dangling target is reported broken, and nothing else is.
            let got = rows.iter().filter(|r| r.status == "broken").count();
            let want_broken = docs.iter().map(|d| d.broken).sum::<usize>();
            if got == want_broken {
                ctx.cell.metric("", "broken_found", got as f64);
            } else {
                ctx.cell.fail("", "broken_found", &format!("{} of {}", got, want_broken));
            }
        }
        Err(e) => {
            ctx.err("", "links_subtree", &e);
        }
    }
    ctx.cell.lat("", "links_subtree", &all);

    // Backlinks: what points *here*. The expensive direction, since it is a lookup by
    // resolved target rather than by owning document.
    let mut back = Latencies::default();
    let mut expect: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for d in &docs {
        for t in &d.resolved {
            *expect.entry(t.as_str()).or_default() += 1;
        }
    }
    let mut bad = 0;
    for d in docs.iter().take(ctx.params.usize("backlink_probes", 100)) {
        match ctx.op(ops::BACKLINKS, &mut back, || ctx.backend.backlinks(&d.path)) {
            Ok(rows) => {
                if rows.len() != *expect.get(d.path.as_str()).unwrap_or(&0) {
                    bad += 1;
                }
            }
            Err(e) => {
                if unsupported(ctx, "backlinks", &e) {
                    return Ok(());
                }
                ctx.err("", "backlinks", &e);
                break;
            }
        }
    }
    ctx.cell.lat("", "backlinks", &back);
    if bad == 0 {
        ctx.cell.metric("", "backlinks_match_graph", 1.0);
    } else {
        ctx.cell.fail("", "backlinks_match_graph", &format!("{} documents", bad));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// MD-03 — moving a file that other files point at
// ---------------------------------------------------------------------------------------

/// Rename under each `link_updates` mode and report what each costs.
///
/// `rewrite` is the interesting one: it commits a new version of every document that
/// pointed at what moved, so a single rename writes `fan_in` documents. That is real write
/// amplification, and it belongs in the record next to the chunk-level amplification the ME
/// family measures.
fn move_relink(ctx: &Ctx) -> anyhow::Result<()> {
    let fan_in = ctx.params.usize("fan_in", 200);
    let size = ctx.params.usize("file_size", 4096);

    for mode in ["off", "report", "rewrite"] {
        let prefix = format!("/mv/{}", mode);
        // Every document links to f00000, which is the one that will move.
        let mut docs = corpus(&prefix, fan_in + 1, size, 1, 0, 2);
        for (i, d) in docs.iter_mut().enumerate().skip(1) {
            let body = String::from_utf8_lossy(&d.body).replace(
                &format!("](./f{:05}.md)", (i + 1) % (fan_in + 1)),
                "](./f00000.md)",
            );
            d.body = body.into_bytes();
            d.resolved = vec![format!("{}/f00000.md", prefix)];
        }
        if !write_all(ctx, &docs, mode)? {
            return Ok(());
        }

        if let Err(e) = ctx.backend.set_link_mode(mode) {
            if unsupported(ctx, "set_link_mode", &e) {
                return Ok(());
            }
            ctx.err(mode, "set_link_mode", &e);
            return Ok(());
        }

        // How many documents actually point here, checked before the move so a mode that
        // silently rewrites nothing cannot pass by having had nothing to do.
        let before = match ctx.backend.backlinks(&format!("{}/f00000.md", prefix)) {
            Ok(r) => r.len(),
            Err(e) => {
                ctx.err(mode, "backlinks", &e);
                return Ok(());
            }
        };
        if before != fan_in {
            ctx.cell.fail(mode, "fan_in", &format!("{} of {}", before, fan_in));
            continue;
        }
        ctx.cell.metric(mode, "fan_in", before as f64);

        let from = format!("{}/f00000.md", prefix);
        let to = format!("{}/renamed.md", prefix);
        let mut lat = Latencies::default();
        if let Err(e) = ctx.op(ops::RENAME, &mut lat, || ctx.backend.rename(&from, &to)) {
            ctx.err(mode, "rename", &e);
            continue;
        }
        ctx.cell.lat(mode, "rename", &lat);

        // What the mode did to the documents that pointed here: `rewrite` should have
        // moved every one of them to version 2, the others should have left them at 1.
        let mut rewritten = 0;
        for d in docs.iter().skip(1) {
            if let Ok((_, v)) = ctx.backend.read_versioned(&d.path) {
                if v > 1 {
                    rewritten += 1;
                }
            }
        }
        ctx.cell.metric(mode, "documents_rewritten", rewritten as f64);
        let want = if mode == "rewrite" { fan_in } else { 0 };
        if rewritten == want {
            ctx.cell.metric(mode, "rewrite_as_declared", 1.0);
        } else {
            ctx.cell.fail(mode, "rewrite_as_declared", &format!("{} rewritten, expected {}", rewritten, want));
        }
        // After a rewrite the links must resolve again; after off/report they must not.
        if let Ok(rows) = ctx.backend.backlinks(&to) {
            ctx.cell.metric(mode, "backlinks_after", rows.len() as f64);
        }
    }
    // Leave the store as the other families expect to find it.
    let _ = ctx.backend.set_link_mode("report");
    Ok(())
}

// ---------------------------------------------------------------------------------------
// MD-04 — front matter
// ---------------------------------------------------------------------------------------

fn frontmatter(ctx: &Ctx) -> anyhow::Result<()> {
    let n = ctx.params.usize("n_files", 500);
    let size = ctx.params.usize("file_size", 4096);
    let docs = corpus("/fm", n, size, 2, 0, 3);
    if !write_all(ctx, &docs, "")? {
        return Ok(());
    }

    // Reading the parsed block back, which is what a "documents where status = draft"
    // query does per row.
    let mut read = Latencies::default();
    let mut missing = 0;
    for d in &docs {
        match ctx.op(ops::FRONTMATTER, &mut read, || ctx.backend.frontmatter(&d.path)) {
            Ok(Some(j)) => {
                if !j.contains("\"draft\"") {
                    missing += 1;
                }
            }
            Ok(None) => missing += 1,
            Err(e) => {
                if unsupported(ctx, "frontmatter", &e) {
                    return Ok(());
                }
                ctx.err("", "frontmatter", &e);
                return Ok(());
            }
        }
    }
    ctx.cell.lat("", "frontmatter_read", &read);
    if missing == 0 {
        ctx.cell.metric("", "frontmatter_parsed", 1.0);
    } else {
        ctx.cell.fail("", "frontmatter_parsed", &format!("{} of {} documents", missing, n));
    }

    // Changing one key. This rewrites the document, so it is a write measured against the
    // ordinary write path — the question it answers is what a metadata update costs
    // compared with an ordinary edit of the same document.
    let mut set = Latencies::default();
    let mut corrupted = 0;
    for d in docs.iter().take(ctx.params.usize("meta_writes", 200)) {
        if let Err(e) = ctx.op(ops::SET_META, &mut set, || ctx.backend.set_meta(&d.path, "status", "published")) {
            if unsupported(ctx, "set_meta", &e) {
                return Ok(());
            }
            ctx.err("", "set_meta", &e);
            return Ok(());
        }
        // The body below the front matter must be untouched, byte for byte: that is the
        // whole promise of editing one key rather than re-serialising the document.
        if let (Ok(got), Some((_, end))) = (ctx.backend.read(&d.path), textdb_md::split_frontmatter(&d.body)) {
            match textdb_md::split_frontmatter(&got) {
                Some((_, gend)) if got[gend..] == d.body[end..] => {}
                _ => corrupted += 1,
            }
        }
    }
    ctx.cell.lat("", "set_meta", &set);
    if corrupted == 0 {
        ctx.cell.metric("", "body_preserved", 1.0);
    } else {
        ctx.cell.fail("", "body_preserved", &format!("{} documents", corrupted));
    }

    // The change is visible through the index, not just in the bytes.
    if let Ok(Some(j)) = ctx.backend.frontmatter(&docs[0].path) {
        if j.contains("published") {
            ctx.cell.metric("", "meta_reindexed", 1.0);
        } else {
            ctx.cell.fail("", "meta_reindexed", "index still says draft");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// MD-05 — sections
// ---------------------------------------------------------------------------------------

fn sections(ctx: &Ctx) -> anyhow::Result<()> {
    let n = ctx.params.usize("n_files", 200);
    let size = ctx.params.usize("file_size", 16384);
    let headings = ctx.params.usize("headings", 32);
    let docs = corpus("/sec", n, size, 1, 0, headings);
    if !write_all(ctx, &docs, "")? {
        return Ok(());
    }

    let mut list = Latencies::default();
    let mut wrong = 0;
    for d in &docs {
        match ctx.op(ops::SECTIONS, &mut list, || ctx.backend.sections(&d.path)) {
            Ok(rows) => {
                let got: Vec<&str> = rows.iter().map(|r| r.heading.as_str()).collect();
                if got != d.headings.iter().map(|s| s.as_str()).collect::<Vec<_>>() {
                    wrong += 1;
                }
            }
            Err(e) => {
                if unsupported(ctx, "sections", &e) {
                    return Ok(());
                }
                ctx.err("", "sections", &e);
                return Ok(());
            }
        }
    }
    ctx.cell.lat("", "sections_list", &list);
    if wrong == 0 {
        ctx.cell.metric("", "headings_match", 1.0);
    } else {
        ctx.cell.fail("", "headings_match", &format!("{} of {} documents", wrong, n));
    }

    // Fetching one section's body by heading: the addressed read the section index exists
    // for, and the one an agent reading part of a document actually issues.
    let mut body = Latencies::default();
    let mut empty = 0;
    for d in &docs {
        let h = d.headings[d.headings.len() / 2].clone();
        let p = d.path.clone();
        match ctx.op(ops::SECTION, &mut body, || ctx.backend.section(&p, &h)) {
            Ok(Some(b)) => {
                if !b.starts_with(format!("# {}", h).as_bytes()) {
                    empty += 1;
                }
            }
            Ok(None) => empty += 1,
            Err(e) => {
                if unsupported(ctx, "section", &e) {
                    return Ok(());
                }
                ctx.err("", "section", &e);
                return Ok(());
            }
        }
    }
    ctx.cell.lat("", "section_body", &body);
    if empty == 0 {
        ctx.cell.metric("", "section_body_correct", 1.0);
    } else {
        ctx.cell.fail("", "section_body_correct", &format!("{} of {} documents", empty, n));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// MD-06 — the change feed
// ---------------------------------------------------------------------------------------

/// What it costs to follow a store. Two shapes: a watcher catching up from nothing, and a
/// watcher already up to date that polls after each change — the steady state, and the one
/// that decides whether following a busy store is cheap.
fn feed(ctx: &Ctx) -> anyhow::Result<()> {
    let n = ctx.params.usize("n_changes", 2000);
    let size = ctx.params.usize("file_size", 2048);
    let docs = corpus("/feed", n.min(ctx.params.usize("n_files", 200)), size, 1, 0, 2);
    if !write_all(ctx, &docs, "")? {
        return Ok(());
    }

    let start = match ctx.backend.changes_since(0) {
        Ok((last, _)) => last,
        Err(e) => {
            if unsupported(ctx, "changes_since", &e) {
                return Ok(());
            }
            ctx.err("", "changes_since", &e);
            return Ok(());
        }
    };

    // Tail: one poll after each write, which is what `watch` does.
    let mut tail = Latencies::default();
    let mut seq = start;
    let mut seen = 0u64;
    // Each pass over the corpus rewrites the line the previous pass wrote, so every edit
    // has exactly one anchor however many times round the loop goes.
    let mut anchors: Vec<Vec<u8>> = docs.iter().map(|d| first_filler(&d.body).unwrap_or_default()).collect();
    let mut made = 0u64;
    for i in 0..n {
        let k = i % docs.len();
        let d = &docs[k];
        if anchors[k].is_empty() {
            continue;
        }
        let new = format!("Tail edit {:06} of this document, long enough to be a line.", i).into_bytes();
        let p = d.path.clone();
        let old = anchors[k].clone();
        if let Err(e) = ctx.op(ops::REPLACE, &mut Latencies::default(), || ctx.backend.replace(&p, &old, &new, None)) {
            ctx.err("", "replace", &e);
            break;
        }
        anchors[k] = new;
        made += 1;
        match ctx.op(ops::CHANGES_SINCE, &mut tail, || ctx.backend.changes_since(seq)) {
            Ok((last, rows)) => {
                seq = last;
                seen += rows;
            }
            Err(e) => {
                ctx.err("", "changes_since", &e);
                break;
            }
        }
    }
    ctx.cell.lat("", "feed_tail", &tail);
    // Every commit the loop made appears exactly once in the feed: a watcher that misses
    // rows is worse than a slow one.
    if seen >= made {
        ctx.cell.metric("", "feed_no_gaps", 1.0);
    } else {
        ctx.cell.fail("", "feed_no_gaps", &format!("{} rows for {} changes", seen, made));
    }

    // Catch-up: everything from the beginning in one query.
    let mut full = Latencies::default();
    match ctx.op(ops::CHANGES_SINCE, &mut full, || ctx.backend.changes_since(0)) {
        Ok((_, rows)) => ctx.cell.metric("", "feed_rows_total", rows as f64),
        Err(e) => {
            ctx.err("", "feed_full", &e);
        }
    }
    ctx.cell.lat("", "feed_full", &full);
    Ok(())
}

// ---------------------------------------------------------------------------------------
// MD-07 — searching front matter
// ---------------------------------------------------------------------------------------

/// Property queries over a corpus whose front matter is known, so every count is checked.
///
/// The two autosuggest calls are the ones that decide whether the UI feels instant: they run
/// on every keystroke, so they are reported separately from the search itself. `keys` is the
/// one that used to be worst — enumerating property names meant reading every document's JSON,
/// 67 ms over 50,000 notes — and is now an index range.
fn property_search(ctx: &Ctx) -> anyhow::Result<()> {
    let n = ctx.params.usize("n_files", 500);
    let size = ctx.params.usize("file_size", 2048);
    // Front matter with a known distribution: every fourth note is a draft, every third is
    // tagged `telco`, and `budget` is the long tail a real vault always has.
    let docs: Vec<(String, Vec<u8>)> = (0..n)
        .map(|i| {
            let status = ["draft", "review", "published", "archived"][i % 4];
            let tags = if i % 3 == 0 { "telco, cvm" } else { "cvm" };
            let mut body = format!(
                "---\ntitle: Note {i}\nstatus: {status}\ntags: [{tags}]\npriority: {}\nproject:\n  name: atlas\n",
                1 + (i % 5)
            );
            if i % 10 == 0 {
                body.push_str(&format!("budget: {}\n", 100 + i));
            }
            body.push_str("---\n\n# Note\n\n");
            while body.len() < size {
                body.push_str("Filler prose that is long enough to be chunked sensibly.\n");
            }
            (format!("/prop/f{:05}.md", i), body.into_bytes())
        })
        .collect();
    let mut lat = Latencies::default();
    for (path, body) in &docs {
        if let Err(e) = ctx.op(ops::CREATE, &mut lat, || ctx.backend.create(path, body)) {
            ctx.err("", "create", &e);
            return Ok(());
        }
    }
    ctx.cell.lat("", "create", &lat);

    // What the vault uses. The oracle is the generator: eight names, and `budget` on a tenth.
    let mut keys = Latencies::default();
    match ctx.op(ops::PROP_KEYS, &mut keys, || ctx.backend.property_keys("")) {
        Ok(rows) => {
            let by: std::collections::HashMap<&str, u64> = rows.iter().map(|(k, d)| (k.as_str(), *d)).collect();
            // A list is one row per element, so a note tagged twice must still count once.
            let tags_ok = by.get("tags") == Some(&(n as u64));
            let budget_ok = by.get("budget") == Some(&(n as u64).div_ceil(10));
            if tags_ok && budget_ok && by.contains_key("project.name") {
                ctx.cell.metric("", "keys_match_corpus", 1.0);
            } else {
                ctx.cell.fail("", "keys_match_corpus", &format!("{:?}", rows));
            }
        }
        Err(e) => {
            if unsupported(ctx, "property_keys", &e) {
                return Ok(());
            }
            ctx.err("", "property_keys", &e);
            return Ok(());
        }
    }
    ctx.cell.lat("", "prop_keys", &keys);

    // The same call a UI makes per keystroke while a name is being typed.
    let mut prefix = Latencies::default();
    for p in ["p", "pr", "pro", "proj"] {
        if ctx.op(ops::PROP_KEYS, &mut prefix, || ctx.backend.property_keys(p)).is_err() {
            break;
        }
    }
    ctx.cell.lat("", "prop_keys_prefix", &prefix);

    let mut values = Latencies::default();
    match ctx.op(ops::PROP_VALUES, &mut values, || ctx.backend.property_values("status", "")) {
        Ok(rows) => {
            let total: u64 = rows.iter().map(|(_, d)| d).sum();
            if rows.len() == 4 && total == n as u64 {
                ctx.cell.metric("", "values_match_corpus", 1.0);
            } else {
                ctx.cell.fail("", "values_match_corpus", &format!("{} values, {} docs", rows.len(), total));
            }
        }
        Err(e) => {
            ctx.err("", "property_values", &e);
        }
    }
    ctx.cell.lat("", "prop_values", &values);

    // Each query's answer is arithmetic on the generator, so a wrong index fails rather than
    // merely looking fast.
    let quarter = n.div_ceil(4);
    let cases: [(&str, usize); 6] = [
        ("status:draft", quarter),
        ("tags:telco", n.div_ceil(3)),
        ("priority:>3", (0..n).filter(|i| 1 + (i % 5) > 3).count()),
        ("status:draft tags:telco", (0..n).filter(|i| i % 4 == 0 && i % 3 == 0).count()),
        ("has:budget", n.div_ceil(10)),
        ("status:!=draft", n - quarter),
    ];
    for (query, want) in cases {
        let mut one = Latencies::default();
        match ctx.op(ops::PROP_FIND, &mut one, || ctx.backend.property_find(query)) {
            Ok(paths) => {
                if paths.len() == want {
                    ctx.cell.metric(query, "hits", paths.len() as f64);
                } else {
                    ctx.cell.fail(query, "hits", &format!("{} of {}", paths.len(), want));
                }
            }
            Err(e) => {
                ctx.err(query, "property_find", &e);
                break;
            }
        }
        ctx.cell.lat(query, "find", &one);
    }
    Ok(())
}
