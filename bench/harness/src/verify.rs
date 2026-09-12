//! Untimed accuracy checks that bracket every cell.
//!
//! Nothing here runs inside a timed span, and nothing here is reported as a latency: the
//! point is to establish that the numbers a cell publishes describe real work. A backend
//! that silently does nothing, or that starts a rep on the previous rep's data, passes the
//! timing harness easily — it is these checks that catch it.
//!
//! `precheck` runs before the suite: the store must be empty, and a canary document must
//! survive a create → read → delete round-trip. `postcheck` runs after: everything the
//! backend lists must be readable and self-consistent, and where the suite registered a
//! [`Reference`] oracle, every document must match byte for byte.
//!
//! A failed check marks the cell, and a marked cell publishes no timings at all
//! (see [`crate::metrics::Cell::finish`]).

use crate::backend::BackendError;
use crate::reference::Reference;
use crate::runner::Ctx;

/// Namespace the write canary uses. Removed again before the suite starts.
const CANARY_DIR: &str = "/_bench_canary";
const CANARY_PATH: &str = "/_bench_canary/canary.md";
const CANARY_BODY: &[u8] = b"# canary\nthe quick brown fox\n";

/// Families whose measurement is storage footprint, where even a deleted canary would
/// leave a tombstone or a commit behind and bias the result. They get the read-only
/// emptiness check only, and the skip is recorded rather than left implicit.
fn canary_allowed(family: &str) -> bool {
    !matches!(family, "FP" | "DU")
}

/// Record one check. `ok == false` marks the cell, which voids its timings.
fn check(ctx: &Ctx, case: &str, name: &str, ok: bool, detail: &str) {
    if ok {
        ctx.cell.metric(case, name, 1.0);
    } else {
        ctx.cell.fail(case, name, detail);
    }
}

/// Pre-run checks. Returns false when the cell is already unusable.
pub fn precheck(ctx: &Ctx) -> bool {
    let case = "verify_pre";

    // 1. The store must be empty. This is what would have caught the leaked-connection
    //    bug: a rep starting on the previous rep's database is not a clean measurement.
    match ctx.backend.list("/") {
        Ok(entries) => {
            let files: Vec<&str> = entries.iter().filter(|e| !e.is_dir).map(|e| e.path.as_str()).collect();
            check(
                ctx,
                case,
                "clean_start",
                files.is_empty(),
                &format!("{} documents present before the suite ran, e.g. {:?}", files.len(), &files[..files.len().min(3)]),
            );
            if !files.is_empty() {
                return false;
            }
        }
        Err(BackendError::NotSupported(_)) => ctx.cell.note(case, "clean_start", "N/A: list not supported"),
        Err(e) => {
            check(ctx, case, "clean_start", false, &format!("list failed: {}", e));
            return false;
        }
    }

    if !canary_allowed(&ctx.cell.family) {
        ctx.cell.note(case, "canary_roundtrip", "skipped: canary would bias a storage-footprint measurement");
        return true;
    }

    // 2. A document written now must come back byte-identical, and must be gone after a
    //    delete. A backend that accepts writes and returns stale or empty reads fails here
    //    rather than posting excellent latencies.
    if let Err(e) = ctx.backend.create(CANARY_PATH, CANARY_BODY) {
        check(ctx, case, "canary_roundtrip", false, &format!("create failed: {}", e));
        return false;
    }
    match ctx.backend.read(CANARY_PATH) {
        Ok(got) if got == CANARY_BODY => {}
        Ok(got) => {
            check(
                ctx,
                case,
                "canary_roundtrip",
                false,
                &format!("read back {} bytes, expected {}", got.len(), CANARY_BODY.len()),
            );
            return false;
        }
        Err(e) => {
            check(ctx, case, "canary_roundtrip", false, &format!("read failed: {}", e));
            return false;
        }
    }
    if let Err(e) = ctx.backend.delete(CANARY_PATH) {
        check(ctx, case, "canary_roundtrip", false, &format!("delete failed: {}", e));
        return false;
    }
    if ctx.backend.read(CANARY_PATH).is_ok() {
        check(ctx, case, "canary_roundtrip", false, "document still readable after delete");
        return false;
    }
    check(ctx, case, "canary_roundtrip", true, "");
    true
}

/// Post-run checks. `reference`, when the suite registered one, is the oracle.
pub fn postcheck(ctx: &Ctx, reference: Option<&Reference>) {
    let case = "verify_post";

    // 1. Everything the backend claims to hold must actually be readable, and any size it
    //    reports must match the bytes it returns.
    let listed = match ctx.backend.list("/") {
        Ok(e) => e,
        Err(BackendError::NotSupported(_)) => {
            ctx.cell.note(case, "listed_readable", "N/A: list not supported");
            Vec::new()
        }
        Err(e) => {
            check(ctx, case, "listed_readable", false, &format!("list failed: {}", e));
            return;
        }
    };
    let files: Vec<_> = listed.iter().filter(|e| !e.is_dir && e.path != CANARY_PATH && !e.path.starts_with(CANARY_DIR)).collect();
    let mut unreadable = Vec::new();
    let mut size_mismatch = Vec::new();
    for e in &files {
        match ctx.backend.read(&e.path) {
            Ok(body) => {
                if let Some(n) = e.nbytes {
                    if n != body.len() as u64 {
                        size_mismatch.push(format!("{} listed {}B, read {}B", e.path, n, body.len()));
                    }
                }
            }
            Err(err) => unreadable.push(format!("{}: {}", e.path, err)),
        }
    }
    check(
        ctx,
        case,
        "listed_readable",
        unreadable.is_empty(),
        &format!("{} of {} listed documents unreadable: {:?}", unreadable.len(), files.len(), &unreadable[..unreadable.len().min(3)]),
    );
    check(
        ctx,
        case,
        "size_consistent",
        size_mismatch.is_empty(),
        &format!("{} size mismatches: {:?}", size_mismatch.len(), &size_mismatch[..size_mismatch.len().min(3)]),
    );
    ctx.cell.metric(case, "documents", files.len() as f64);

    // 2. Storage must have grown if anything was stored. Catches a backend whose writes
    //    never reach the medium.
    if !files.is_empty() {
        match ctx.backend.storage_bytes() {
            Ok(n) => check(ctx, case, "storage_nonzero", n > 0, "backend holds documents but reports 0 bytes of storage"),
            Err(BackendError::NotSupported(_)) => ctx.cell.note(case, "storage_nonzero", "N/A: storage_bytes not supported"),
            Err(e) => check(ctx, case, "storage_nonzero", false, &format!("storage_bytes failed: {}", e)),
        }
    }

    // 3. The oracle comparison, where the suite kept one.
    let Some(r) = reference else {
        ctx.cell.note(case, "oracle_match", "no reference model registered by this suite");
        return;
    };
    let mut mismatches = Vec::new();
    for (path, want) in &r.docs {
        match ctx.backend.read(path) {
            Ok(got) if &got == want => {}
            Ok(got) => mismatches.push(format!("{}: {}B != oracle {}B", path, got.len(), want.len())),
            Err(e) => mismatches.push(format!("{}: {}", path, e)),
        }
    }
    check(
        ctx,
        case,
        "oracle_match",
        mismatches.is_empty(),
        &format!("{} of {} documents differ from the oracle: {:?}", mismatches.len(), r.docs.len(), &mismatches[..mismatches.len().min(3)]),
    );
    ctx.cell.metric(case, "oracle_documents", r.docs.len() as f64);
}
