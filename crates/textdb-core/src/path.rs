//! Path history: renames, moves and deletes, recorded next to a file's versions.
//!
//! A version is a change of content. A path event is a change of where a file or folder is,
//! or of whether it is there at all; it does not create a version, so version `v` is still
//! content `v - 1` plus one commit. Backends record one event per node an operation touched —
//! the node it named, and every node below a folder — so a file's history stays complete
//! when only its folder was renamed, moved or deleted.
//!
//! Recording is a store setting, [`PATH_HISTORY_SETTING`], on unless turned off.

/// What happened to a node's path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathOp {
    /// A new name in the same folder.
    Rename,
    /// Into another folder, with or without a new name.
    Move,
    /// Deleted (to the trash, where backends keep one).
    Delete,
}

impl PathOp {
    /// `Rename` when `from` and `to` are in the same folder, `Move` otherwise.
    pub fn classify(from: &str, to: &str) -> PathOp {
        if parent(from) == parent(to) {
            PathOp::Rename
        } else {
            PathOp::Move
        }
    }

    /// Lower-case name, as recorded in path event rows.
    pub fn as_str(self) -> &'static str {
        match self {
            PathOp::Rename => "rename",
            PathOp::Move => "move",
            PathOp::Delete => "delete",
        }
    }

    pub fn parse(s: &str) -> Option<PathOp> {
        match s {
            "rename" => Some(PathOp::Rename),
            "move" => Some(PathOp::Move),
            "delete" => Some(PathOp::Delete),
            _ => None,
        }
    }
}

fn parent(path: &str) -> &str {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

/// The store setting that turns path history on or off.
pub const PATH_HISTORY_SETTING: &str = "path_history";

/// Path history is recorded unless a store or session turns it off.
pub const PATH_HISTORY_DEFAULT: bool = true;

/// An on/off setting value: `on`, `true`, `yes`, `1` and `off`, `false`, `no`, `0`, in any case.
pub fn parse_switch(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_name_in_the_same_folder_is_a_rename() {
        assert_eq!(PathOp::classify("/a/b.md", "/a/c.md"), PathOp::Rename);
        assert_eq!(PathOp::classify("/b.md", "/c.md"), PathOp::Rename);
        assert_eq!(PathOp::classify("/a", "/b"), PathOp::Rename);
        assert_eq!(PathOp::classify("/a/b.md", "/x/b.md"), PathOp::Move);
        assert_eq!(PathOp::classify("/a/b.md", "/b.md"), PathOp::Move);
        assert_eq!(PathOp::classify("/a/b/c.md", "/a/c.md"), PathOp::Move);
    }

    #[test]
    fn names_round_trip_and_switches_parse() {
        for op in [PathOp::Rename, PathOp::Move, PathOp::Delete] {
            assert_eq!(PathOp::parse(op.as_str()), Some(op));
        }
        assert_eq!(PathOp::parse("purge"), None);
        assert_eq!(parse_switch(" ON "), Some(true));
        assert_eq!(parse_switch("0"), Some(false));
        assert_eq!(parse_switch("maybe"), None);
    }
}
