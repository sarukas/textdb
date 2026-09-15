//! The operation vocabulary every suite times against.
//!
//! One name per `Backend` method that a suite can call in a timed span. Suites pass these
//! constants to [`crate::runner::Ctx::op`], which attributes the latency both to the
//! suite's own per-case bucket and to a run-wide per-operation bucket. Keeping the names
//! in one place is what makes `op_*` rows comparable across families and backends —
//! before this, each suite spelled its own label and the report could not aggregate them.

pub const CREATE: &str = "create";
pub const READ: &str = "read";
pub const READ_LINES: &str = "read_lines";
pub const READ_VERSION: &str = "read_version";
pub const READ_VERSIONED: &str = "read_versioned";
pub const OVERWRITE: &str = "overwrite";
pub const REPLACE: &str = "replace";
pub const APPEND: &str = "append";
pub const DELETE: &str = "delete";
pub const RENAME: &str = "rename";
pub const LIST: &str = "list";
pub const SEARCH: &str = "search";
pub const HISTORY: &str = "history";
pub const MAINTENANCE: &str = "maintenance";
// Structure sidecar: textdb-only, so these rows are present for the textdb bindings and
// absent for the baselines rather than being compared across them.
pub const LINKS: &str = "links";
pub const BACKLINKS: &str = "backlinks";
pub const FRONTMATTER: &str = "frontmatter";
pub const SET_META: &str = "set_meta";
pub const SECTIONS: &str = "sections";
pub const SECTION: &str = "section";
pub const CHANGES_SINCE: &str = "changes_since";
pub const SYNC: &str = "sync";
pub const PROP_KEYS: &str = "prop_keys";
pub const PROP_VALUES: &str = "prop_values";
pub const PROP_FIND: &str = "prop_find";

/// Every operation name, in the order the report should present them.
pub const ALL: &[&str] = &[
    CREATE,
    READ,
    READ_LINES,
    READ_VERSION,
    READ_VERSIONED,
    OVERWRITE,
    REPLACE,
    APPEND,
    DELETE,
    RENAME,
    LIST,
    SEARCH,
    HISTORY,
    MAINTENANCE,
    LINKS,
    BACKLINKS,
    FRONTMATTER,
    SET_META,
    SECTIONS,
    SECTION,
    CHANGES_SINCE,
    SYNC,
    PROP_KEYS,
    PROP_VALUES,
    PROP_FIND,
];

/// True when `name` is part of the vocabulary. Used by the report to separate genuine
/// operation rows from the free-form per-case labels suites also emit.
pub fn is_op(name: &str) -> bool {
    ALL.contains(&name)
}
