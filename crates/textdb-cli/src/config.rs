//! Where the CLI's settings come from.
//!
//! Today a setting is a command-line flag, else an environment variable (`TEXTDB_STORE`,
//! `TEXTDB_AUTHOR`), else a built-in default, and `textdb config` prints each value with its
//! source. The layers planned for later — named profiles in a config file, and credential
//! providers for Postgres — are described in `docs/cli.md`; they slot in between the
//! environment and the defaults without changing any command.

/// A store location, by backend.
pub enum StoreUrl {
    /// A file path.
    Sqlite(String),
    /// A libpq-style URL, passed to the driver as given.
    Postgres(String),
}

/// Read `--store`: `postgres://…` / `postgresql://…` is Postgres; `sqlite:…` or a bare path
/// is a SQLite file.
///
/// `sqlite:///kb.db` is the relative path `kb.db` and `sqlite:////srv/kb.db` the absolute
/// `/srv/kb.db`, as in SQLAlchemy and the Python library, so `sqlite:///C:/kb.db` is `C:/kb.db`.
pub fn parse_store(s: &str) -> StoreUrl {
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        return StoreUrl::Postgres(s.to_string());
    }
    let path = match s.strip_prefix("sqlite:") {
        Some(rest) => match rest.strip_prefix("//") {
            Some(r) => r.strip_prefix('/').unwrap_or(r),
            None => rest,
        },
        None => s,
    };
    StoreUrl::Sqlite(path.to_string())
}

/// The store location with any password hidden, for printing.
pub fn redact(store: &str) -> String {
    let Some((scheme, rest)) = store.split_once("://") else {
        return store.to_string();
    };
    let Some((userinfo, host)) = rest.split_once('@') else {
        return store.to_string();
    };
    match userinfo.split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:***@{host}"),
        None => store.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite(s: &str) -> String {
        match parse_store(s) {
            StoreUrl::Sqlite(p) => p,
            StoreUrl::Postgres(_) => panic!("{s} parsed as postgres"),
        }
    }

    #[test]
    fn store_urls() {
        assert_eq!(sqlite("kb.db"), "kb.db");
        assert_eq!(sqlite("C:\\data\\kb.db"), "C:\\data\\kb.db");
        assert_eq!(sqlite("sqlite:kb.db"), "kb.db");
        assert_eq!(sqlite("sqlite://kb.db"), "kb.db");
        assert_eq!(sqlite("sqlite:///kb.db"), "kb.db");
        assert_eq!(sqlite("sqlite:////srv/kb.db"), "/srv/kb.db");
        assert_eq!(sqlite("sqlite:///C:/kb.db"), "C:/kb.db");
        assert!(matches!(parse_store("postgresql://u@h/db"), StoreUrl::Postgres(_)));
        assert!(matches!(parse_store("POSTGRES://u@h/db"), StoreUrl::Postgres(_)));
    }

    #[test]
    fn passwords_are_hidden() {
        assert_eq!(redact("postgres://me:secret@db:5432/kb"), "postgres://me:***@db:5432/kb");
        assert_eq!(redact("postgres://me@db/kb"), "postgres://me@db/kb");
        assert_eq!(redact("kb.db"), "kb.db");
    }
}
