//! Running the whole matrix as a delegated account instead of as the store's owner (#14 item 2).
//!
//! Every number the suite has produced so far is the **owner's**: no backend presented a bearer,
//! so nothing measured a filtered read, a translated listing or a projected document. That was
//! the right thing to measure first — "the access model costs nothing to callers who do not use
//! it" is the claim most likely to go wrong quietly, and it did, in three places — but it leaves
//! the other half unmeasured, and optimising one branch of a two-namespace view without a number
//! for the other is optimising half a structure.
//!
//! ## Why a single-root account
//!
//! The account is **single-root** over one folder ([`SHARE`]): its root *is* that folder, so the
//! path it writes is the path the suite writes. `/notes/a.md` goes in as `/notes/a.md` and comes
//! back as `/notes/a.md`, while the store holds `/bench/notes/a.md` and every read, listing and
//! write crosses [`View::to_store`] and [`View::to_view`] on the way. **No path is translated in
//! the harness**, which is the point: a suite that rewrote paths would be measuring its own
//! string work alongside the store's, and every oracle would have to be taught the difference.
//!
//! An aliased account — the other namespace, where the root lists aliases and every path gains a
//! `/<alias>` prefix — would need exactly that harness-side rewriting. It is also barely a
//! different measurement: the two namespaces differ by one `format!` inside `to_store`, and
//! everything expensive (the visibility predicate, the per-row projection, the two-branch view)
//! runs the same either way. So this measures the single-root shape and says so.
//!
//! A share also has to be a folder that already exists, and it has to contain whatever the suite
//! creates *later* — NS-02 invents a namespace 1000 deep as it goes. One share over one folder
//! covers everything written under it from then on, which per-top-level-folder grants would not.
//!
//! ## What it does not measure
//!
//! **Row-level security is bypassed here.** `kb.node`'s policy is deliberately not `FORCE`d, and
//! Postgres exempts a table's owner and any superuser from a policy; the harness connects as the
//! role that owns the extension. So a delegated Postgres run measures the `kb.*` view and
//! function translation and **not** the RLS layer underneath it. Measuring that needs a role with
//! no privilege on the base tables, which is #14 item 5 and is not a read-only change: writing
//! under such a role needs `SECURITY DEFINER` on the functions that do the bookkeeping.

use crate::backend::BackendError;

/// The folder the account's whole namespace is. Anything the suite writes lands under it.
pub const SHARE: &str = "/bench";

/// The account the delegated backends authenticate as. One per store, created at construction.
pub const ACCOUNT: &str = "bench-agent";

/// The suffix that turns a backend id into its delegated twin: `textdb-pg@account`.
pub const SUFFIX: &str = "@account";

/// Split a backend id into the engine and whether it runs delegated.
///
/// `textdb-pg` is the owner, `textdb-pg@account` the same engine holding a bearer. Two ids rather
/// than two runs, so both columns come out of **one** invocation on one host — a delegated run
/// captured separately would be comparing two moments of this container, and the reference
/// backends put that drift at up to 10x on a single cell.
pub fn split(id: &str) -> (&str, bool) {
    match id.strip_suffix(SUFFIX) {
        Some(engine) => (engine, true),
        None => (id, false),
    }
}

/// The store path a view path names, for the instrumentation that reaches past the view.
///
/// Almost nothing needs this. The suite's paths are the account's paths and the store translates
/// them; only the hooks that read the node table directly — `extra_stats`, `leaf_hashes` on
/// SQLite — see store paths, and they are not part of any timed span.
pub fn store_path(delegated: bool, view_path: &str) -> String {
    if !delegated || view_path.is_empty() {
        return view_path.to_string();
    }
    match view_path {
        "/" => SHARE.to_string(),
        p => format!("{SHARE}{p}"),
    }
}

/// Why the sidecar surfaces have no delegated number.
///
/// `links`, `backlinks`, `frontmatter` and `sections` are the four operations both backends read
/// from the sidecar **tables** — `kb.link`, `kb.frontmatter`, `kb.section` joined to `kb.node` —
/// rather than through a view or a table function, because neither engine exposes one for them:
/// SQLite has no `textdb_links`, and the Postgres surface for these is the tables themselves.
///
/// Reading a table is reading store paths, and it sees link targets as recorded rather than as an
/// account is shown them — link **projection** (#12 §5) is exactly what those rows do not carry.
/// Translating the paths in the harness would make the oracles pass and publish a number for a
/// query no account can actually run, which is the same mistake as reporting the owner's numbers
/// in the account's column. So the twin records N/A with this reason, and *that* is the result:
/// there is no view-aware SQL surface for the structure sidecar on either engine.
/// `BackendError::NotSupported` prints its own `N/A:`, so this is the reason alone.
pub const NO_SIDECAR_VIEW: &str =
    "the harness reads the sidecar tables, which bypass the account's view; no view-aware SQL surface for links, front matter or sections";

/// A setup step that failed is never silent: a delegated backend that quietly stayed the owner
/// would publish the owner's numbers under the account's name, which is worse than no numbers.
pub fn setup_err(what: &str, e: impl std::fmt::Display) -> BackendError {
    BackendError::Other(format!("delegated setup: {what}: {e}"))
}
