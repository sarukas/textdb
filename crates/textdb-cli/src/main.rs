//! `textdb`: read, search and edit a textdb store from the command line.
//!
//! One binary for SQLite files and Postgres databases, built for agents as much as for
//! people: every command can answer in JSON, a failed write exits with a status that says
//! why, and a conflict carries the current text of the contested lines so the caller can
//! retry without reading the file again. See `docs/cli.md`.

mod config;
mod store;

use std::collections::{BTreeMap, HashMap};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
use textdb_sqlite::db::parent_of;
use textdb_sqlite::normalize_path;

use config::StoreUrl;
use store::{Change, Commit, Entry, ImportStats, PathEvent, Store, StoreError, Written};

type Result<T> = std::result::Result<T, StoreError>;

mod assets;
mod git;
mod links;
mod lock;
mod meta;
mod outline;
mod portable;
mod search;
mod sql_query;
mod sync;

#[derive(Parser)]
#[command(
    name = "textdb",
    version,
    about = "Read, search and edit a versioned text corpus stored in textdb (SQLite or Postgres)",
    after_help = "Exit status: 0 ok, 1 error, 2 usage, 3 conflict (TX001), 4 contention (TX002), \
                  5 not found (TX003), 6 invalid edit (TX004)."
)]
struct Cli {
    /// Store: a SQLite file (`kb.db`, `sqlite:kb.db`) or a Postgres URL (`postgres://user@host/db`).
    #[arg(long, short = 's', global = true, env = "TEXTDB_STORE", default_value = "kb.db")]
    store: String,
    /// Name recorded as the author of writes.
    #[arg(long, short = 'a', global = true, env = "TEXTDB_AUTHOR", default_value = "cli")]
    author: String,
    /// Answer in JSON (errors too, on stdout).
    #[arg(long, global = true)]
    json: bool,
    /// Record renames, moves and deletes in the history of what they touch: `on` or `off` for
    /// this command. Without it the store's `path_history` setting decides, which is on unless
    /// changed with `textdb setting path_history off`.
    #[arg(long, global = true, env = "TEXTDB_PATH_HISTORY", value_name = "on|off", value_parser = switch)]
    path_history: Option<bool>,
    #[command(subcommand)]
    cmd: Cmd,
}

/// A path inside the store, as given on the command line.
///
/// Git Bash rewrites an argument that starts with `/` into a Windows path under its install
/// directory (`/guides/a.md` becomes `C:/Program Files/Git/guides/a.md`) before the program
/// sees it. A drive-letter path can never name something in the store, so it is refused with
/// the two ways around the rewrite rather than reported later as "not found".
fn store_path(s: &str) -> std::result::Result<String, String> {
    let b = s.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\') {
        return Err(format!(
            "'{s}' is a Windows path, not a path in the store. If your shell rewrote a path starting \
             with '/' (Git Bash does), leave out the leading slash or set MSYS_NO_PATHCONV=1"
        ));
    }
    Ok(s.to_string())
}

/// An on/off value: `on`, `off`, and also `true`/`false`, `yes`/`no`, `1`/`0`.
fn switch(s: &str) -> std::result::Result<bool, String> {
    textdb_core::parse_switch(s).ok_or_else(|| format!("'{s}' is not on or off"))
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the store if it does not exist, or upgrade one an older build wrote.
    Init,
    /// Show each setting and where it came from.
    Config,
    /// Load every matching file under a directory; unchanged files make no new version.
    Import {
        dir: PathBuf,
        /// Folder in the store to load into.
        #[arg(long, default_value = "/", value_parser = store_path)]
        prefix: String,
        /// File extensions to load, comma separated (`*` for all).
        #[arg(long, default_value = "md,markdown,mdx,txt")]
        ext: String,
        /// Files per transaction.
        #[arg(long, default_value_t = 500)]
        batch: usize,
    },
    /// Write the files under a folder to a local directory: only new and changed ones (compared
    /// byte for byte), and nothing on disk is deleted, so a git checkout shows exactly what changed
    /// in the store. Stops before writing when names cannot coexist on this computer.
    Export {
        #[arg(value_parser = store_path)]
        prefix: String,
        dir: PathBuf,
        /// List what would be written, and any name problems, without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Reconcile a store folder with a directory, both ways: changes on either side since the
    /// last sync are copied across, deletes included; edits on both sides are merged, and where
    /// they overlap the file on disk gets conflict markers. In a git checkout, changes that came
    /// from git are attributed to their git authors, and --commit commits what sync wrote.
    Sync {
        #[arg(value_parser = store_path)]
        prefix: String,
        dir: PathBuf,
        /// Files found only on disk to take in, by extension (`*` for all). Files already synced
        /// are followed whatever their type.
        #[arg(long, default_value = "md,markdown,mdx,txt")]
        ext: String,
        /// First sync only: the git commit the store's content came from, so changes made since
        /// on either side are merged instead of conflicting.
        #[arg(long, value_name = "REV")]
        base: Option<String>,
        /// Show what would change on each side without writing.
        #[arg(long)]
        dry_run: bool,
        /// Commit the files sync changed on disk, with Textdb-* trailers.
        #[arg(long)]
        commit: bool,
        /// Take in the files that changed include rules (extensions, skipped folders,
        /// .textdbignore) add since the last sync; without it such a sync stops and lists them. It
        /// also accepts changed .gitattributes files for pushing assets.
        #[arg(long)]
        accept_rules: bool,
        /// Remove directories on disk that hold no files.
        #[arg(long)]
        prune_empty_dirs: bool,
        /// Also push new and changed assets, as `textdb assets push` does (the store's
        /// asset_sync setting decides when neither --push nor --pull is given).
        #[arg(long)]
        push: bool,
        /// Also pull assets: the ones the notes link to, or all of them when the store's
        /// asset_pull setting is `all`.
        #[arg(long)]
        pull: bool,
        /// Seconds to wait for another sync of the same directory to finish. Two syncs at once
        /// would each compute both sides from the same base and land one edit twice, so the
        /// second waits. On timeout it exits 4 naming the holder.
        #[arg(long, value_name = "SECONDS", default_value_t = 10)]
        lock_timeout: u64,
        /// Do not wait for another sync of the same directory: exit 4 at once if one is running.
        #[arg(long, conflicts_with = "lock_timeout")]
        no_wait: bool,
    },
    /// When a store folder was last synced, what changed in it since, and how it compares with a
    /// git commit (by git blob id).
    GitStatus {
        #[arg(value_parser = store_path)]
        prefix: String,
        dir: PathBuf,
        #[arg(long, default_value = "HEAD")]
        rev: String,
        /// Which of the commit's files count as missing from the store.
        #[arg(long, default_value = "md,markdown,mdx,txt")]
        ext: String,
    },
    /// Binaries (images, PDFs, office files …) kept in an asset store rather than in git, each with
    /// a pointer document NAME.tdbasset where it belongs, versioned like any document. See
    /// docs/assets.md.
    Assets {
        #[command(subcommand)]
        op: AssetsOp,
    },
    /// Run one SQL statement against the store and print its rows. Besides `kb` and the textdb
    /// functions, the views files, folders, frontmatter, properties, sections, links, commits and authors
    /// describe the live store by path. Read-only unless --write.
    Sql {
        /// The statement; read from stdin when omitted or `-`.
        query: Option<String>,
        /// A value for the next placeholder (`?` in SQLite, `$1`, `$2`, … in Postgres), as text.
        /// In SQLite, `:author` takes the --author name.
        #[arg(long = "param", short = 'p', value_name = "VALUE", allow_hyphen_values = true)]
        params: Vec<String>,
        /// Allow statements that change the store, through `kb` and the textdb functions, which
        /// record versions like any other edit. The internal tables stay off limits.
        #[arg(long)]
        write: bool,
        /// Print whole values instead of cutting them at 60 characters.
        #[arg(long)]
        full: bool,
        /// With --write: run the statement, print each file's diff and the moves and deletes it
        /// made, then undo all of it.
        #[arg(long, requires = "write")]
        dry_run: bool,
        /// Read the statement from this file.
        #[arg(long, short = 'f', conflicts_with = "query")]
        file: Option<PathBuf>,
        /// How to print rows: table, tsv, lines (one value per line) or json (as --json).
        #[arg(long, value_enum, default_value_t = sql_query::SqlFormat::Table)]
        format: sql_query::SqlFormat,
    },
    /// Undo what one `sql --write` run changed, by the batch id it printed (also in the `commits`
    /// view): changed files get their earlier content back as a new version, files it created
    /// are deleted, moves are undone and what it deleted is created again. Refuses, changing
    /// nothing, when something in the batch changed since, unless --skip-changed. SQLite stores.
    RevertBatch {
        batch: String,
        /// Revert what can be, and list what changed since and was left alone.
        #[arg(long)]
        skip_changed: bool,
        /// Say what reverting would do, without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// List one folder.
    Ls {
        #[arg(default_value = "/", value_parser = store_path)]
        path: String,
        /// A table: size, lines, words, versions, last update, and a file's authors or a
        /// folder's contents. A folder's figures are totals over everything below it.
        #[arg(long, short = 'l')]
        long: bool,
        /// Only the paths, one per line: for scripts.
        #[arg(long, short = '1', conflicts_with = "long")]
        paths: bool,
        /// Order by this; folders stay before files.
        #[arg(long, short = 'S', value_enum, default_value_t = SortKey::Name)]
        sort: SortKey,
        /// Reverse the order.
        #[arg(long, short = 'r')]
        reverse: bool,
        /// Everything below the folder, listed by path.
        #[arg(long, short = 'R')]
        recursive: bool,
    },
    /// Show the folder tree.
    Tree {
        #[arg(default_value = "/", value_parser = store_path)]
        path: String,
        /// Levels to show (all when omitted).
        #[arg(long, short = 'L')]
        depth: Option<usize>,
        /// Folders only.
        #[arg(long, short = 'd')]
        dirs: bool,
    },
    /// Version, size and last author of a file or folder.
    Stat {
        #[arg(value_parser = store_path)]
        path: String,
    },
    /// Print a file or part of it.
    Cat {
        #[arg(value_parser = store_path)]
        path: String,
        /// Number the lines, under a header naming the version — pass that version as
        /// --base-version when editing by line number.
        #[arg(long, short = 'n')]
        number: bool,
        /// Lines `A:B` (1-based, inclusive); `A:` to the end, `:B` from the start, `A` alone.
        #[arg(long, short = 'l')]
        lines: Option<String>,
        /// A past version instead of the current one.
        #[arg(long = "version", short = 'v', value_name = "VERSION")]
        at: Option<i64>,
        /// Only the section under this markdown heading (`Heading` or `Parent / Heading`).
        #[arg(long, conflicts_with = "at")]
        section: Option<String>,
    },
    /// Full-text search: every word must occur in the document (AND), "quoted phrases" and
    /// prefix* work, case and accents do not matter. Lists each line holding a word, checked
    /// against the text. For regular expressions or exact case, use `grep`.
    Search {
        /// The words; several arguments are one query.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(long, short = 'p', default_value = "/", value_parser = store_path)]
        prefix: String,
        /// Rows to return at most — matching lines, as on `grep`.
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Matching lines to list per document; the rest are reported as `more`.
        #[arg(long, default_value_t = 10)]
        per_file: usize,
        /// List only the paths of documents that matched.
        #[arg(long, short = 'l', conflicts_with = "count")]
        files_with_matches: bool,
        /// List each matching document and how many of its lines matched.
        #[arg(long, short = 'c')]
        count: bool,
    },
    /// Lines matching a regular expression in every file under a folder; case-sensitive
    /// unless -i. Reads the files, so it is slower than `search` on a large folder.
    Grep {
        pattern: String,
        #[arg(long, short = 'p', default_value = "/", value_parser = store_path)]
        prefix: String,
        #[arg(long, short = 'i')]
        ignore_case: bool,
        /// Match the pattern as plain text.
        #[arg(long, short = 'F')]
        fixed_strings: bool,
        /// List only the paths of files with a match.
        #[arg(long, short = 'l', conflicts_with = "count")]
        files_with_matches: bool,
        /// List each matching file and how many of its lines matched.
        #[arg(long, short = 'c')]
        count: bool,
        /// Rows to return at most — matching lines, as on `search`.
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Matching lines to list per file; the rest are reported as `more`.
        #[arg(long, default_value_t = 10)]
        per_file: usize,
    },
    /// Create a file or replace its content, from --file or stdin.
    Write {
        #[arg(value_parser = store_path)]
        path: String,
        /// The version the new content was derived from. Commits that landed since are
        /// rebased under it; a change to the same lines is a conflict.
        #[arg(long, short = 'b')]
        base_version: Option<i64>,
        #[arg(long, short = 'f')]
        file: Option<PathBuf>,
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Allow writing empty content (otherwise refused, as it is usually a missing pipe).
        #[arg(long)]
        allow_empty: bool,
        /// Only create: fail if the file exists.
        #[arg(long, conflicts_with = "base_version")]
        create: bool,
    },
    /// Replace the one occurrence of the old text with the new text.
    Edit {
        #[arg(value_parser = store_path)]
        path: String,
        // `allow_hyphen_values`: markdown list items start with "- ", which clap would
        // otherwise take for a flag.
        #[arg(long, allow_hyphen_values = true, conflicts_with_all = ["old_file", "stdin_json"])]
        old: Option<String>,
        #[arg(long, allow_hyphen_values = true, conflicts_with_all = ["new_file", "stdin_json"])]
        new: Option<String>,
        #[arg(long, conflicts_with = "stdin_json")]
        old_file: Option<PathBuf>,
        #[arg(long, conflicts_with = "stdin_json")]
        new_file: Option<PathBuf>,
        /// Read `{"old": "…", "new": "…"}` from stdin: no shell quoting for multi-line text.
        #[arg(long)]
        stdin_json: bool,
        /// Commit message (default `edit`).
        #[arg(long, short = 'm')]
        message: Option<String>,
    },
    /// Replace lines FROM..TO (1-based, inclusive) with --text, --file or stdin. TO = FROM-1 inserts before FROM.
    ReplaceLines {
        #[arg(value_parser = store_path)]
        path: String,
        #[arg(required_unless_present = "stdin_json")]
        from: Option<i64>,
        #[arg(required_unless_present = "stdin_json")]
        to: Option<i64>,
        /// The version the line numbers refer to (see `cat -n`).
        #[arg(long, short = 'b')]
        base_version: Option<i64>,
        #[arg(long, short = 't', allow_hyphen_values = true, conflicts_with = "file")]
        text: Option<String>,
        #[arg(long, short = 'f')]
        file: Option<PathBuf>,
        /// Several ranges in one commit, read from stdin as
        /// `[{"from": N, "to": N, "text": "…"}, …]`, all numbered as in the same version.
        #[arg(long, conflicts_with_all = ["from", "to", "text", "file"])]
        stdin_json: bool,
        /// Commit message (default `replace-lines`).
        #[arg(long, short = 'm')]
        message: Option<String>,
    },
    /// The links written in a file, or in every file below a folder, with what each points to:
    /// ok, ambiguous (several files match; the nearest is shown), anchor-missing, broken,
    /// not-in-store (PDFs, images, other files a text store does not hold) or external.
    Links {
        #[arg(value_parser = store_path, default_value = "/")]
        path: String,
        /// Only links that do not resolve: broken, anchor-missing and not-in-store.
        #[arg(long)]
        broken: bool,
        /// The directory the store is synced with: links to files the store does not hold are
        /// looked for there, and only listed when missing.
        #[arg(long, requires = "broken")]
        dir: Option<PathBuf>,
    },
    /// The links in any file that point to a file, or to a file below a folder.
    Backlinks {
        #[arg(value_parser = store_path)]
        path: String,
    },
    /// List markdown headings: one file's outline, or every heading under a folder or the
    /// whole store, with each document's own size and last change alongside.
    Outline {
        /// A file, a folder, or `/` for everything. Default `/`.
        #[arg(value_parser = store_path, default_value = "/")]
        path: String,
        /// Only headings matching this, ignoring case.
        #[arg(long)]
        heading: Option<String>,
        /// How `--heading` matches: the whole heading, its start, or anywhere in it.
        #[arg(long, value_parser = ["exact", "prefix", "contains"], default_value = "exact")]
        match_: String,
        /// Only headings this deep or shallower (`1` is `#`, `2` is `##`).
        #[arg(long)]
        level: Option<i64>,
        /// Distinct headings in use with their counts, rather than the headings themselves.
        #[arg(long)]
        names: bool,
        #[arg(long, default_value_t = 1000)]
        limit: i64,
    },
    /// Read or change front matter, one top-level key at a time; the other lines of the file are
    /// left exactly as they are.
    Meta {
        #[command(subcommand)]
        op: MetaOp,
    },
    /// Append text (the argument, or stdin) to the end of a file; never conflicts.
    Append {
        #[arg(value_parser = store_path)]
        path: String,
        #[arg(allow_hyphen_values = true)]
        text: Option<String>,
        /// Commit message (default `append`).
        #[arg(long, short = 'm')]
        message: Option<String>,
    },
    /// Versions of a file, oldest first.
    History {
        #[arg(value_parser = store_path)]
        path: String,
        /// Versions only, without the renames, moves and deletes that touched the file.
        #[arg(long)]
        versions_only: bool,
    },
    /// Unified diff between two versions; V2 defaults to the current version.
    Diff {
        #[arg(value_parser = store_path)]
        path: String,
        v1: i64,
        v2: Option<i64>,
    },
    /// Line hunks between two versions; defaults to the latest commit.
    Hunks {
        #[arg(value_parser = store_path)]
        path: String,
        v1: Option<i64>,
        v2: Option<i64>,
    },
    /// The content-defined chunks a file is stored as.
    Chunks {
        #[arg(value_parser = store_path)]
        path: String,
        #[arg(long = "version", short = 'v', value_name = "VERSION")]
        at: Option<i64>,
    },
    /// Move or rename a file or folder. Folders the move leaves empty are removed.
    Mv {
        #[arg(value_parser = store_path)]
        from: String,
        #[arg(value_parser = store_path)]
        to: String,
        /// Message recorded in the change log.
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Keep folders the move leaves empty.
        #[arg(long)]
        keep_empty_folders: bool,
        /// Rewrite the links that pointed at what moved (default: the store's `link_updates`
        /// setting, which lists them unless set to `rewrite` or `off`).
        #[arg(long, conflicts_with = "no_update_links")]
        update_links: bool,
        /// Leave links alone, without listing them.
        #[arg(long)]
        no_update_links: bool,
    },
    /// Delete a file or folder; its history stays readable. Folders the delete leaves empty are
    /// removed.
    Rm {
        #[arg(value_parser = store_path)]
        path: String,
        /// Message recorded in the change log.
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Keep folders the delete leaves empty.
        #[arg(long)]
        keep_empty_folders: bool,
    },
    /// Show or change a store setting: `setting`, `setting path_history`,
    /// `setting path_history off` (`on`, or `default` to clear it).
    Setting {
        key: Option<String>,
        value: Option<String>,
    },
    /// Changes after a sequence number, oldest first.
    Log {
        #[arg(long, default_value_t = 0)]
        since: i64,
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// Follow changes as they happen, one line each (JSON lines with --json).
    Watch {
        /// Start after this sequence number instead of now.
        #[arg(long)]
        since: Option<i64>,
        /// Only changes under this folder.
        #[arg(long, short = 'p', default_value = "/", value_parser = store_path)]
        prefix: String,
    },
}

fn main() {
    // clap builds the whole command tree in one go and `run` is one large `match`; in a debug
    // build their frames outgrow the 1 MiB main-thread stack Windows gives a program, so the CLI
    // runs on a thread with room to spare.
    let status = std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(cli_main)
        .expect("start the CLI thread")
        .join()
        .unwrap_or(101);
    std::process::exit(status);
}

fn cli_main() -> i32 {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let json = cli.json;
    let status = match run(cli, &matches) {
        Ok(()) => 0,
        Err(e) => {
            report(&e, json);
            e.exit_code()
        }
    };
    let _ = std::io::stdout().flush();
    status
}

fn run(cli: Cli, matches: &ArgMatches) -> Result<()> {
    let json = cli.json;
    if let Cmd::Config = cli.cmd {
        return show_config(&cli, matches);
    }
    let mut store = store::open(&cli.store)?;
    let st = store.as_mut();
    if cli.path_history.is_some() {
        st.set_session_path_history(cli.path_history)?;
    }
    let author = Some(cli.author.as_str());
    match cli.cmd {
        Cmd::Config => unreachable!("handled above"),
        Cmd::Init => {
            st.init()?;
            let seq = st.last_seq()?;
            let shown = config::redact(&cli.store);
            if json {
                emit_json(&json!({ "store": shown, "backend": st.backend(), "last_seq": seq }))
            } else {
                line(format!("{} store ready: {shown} (last change #{seq})", st.backend()))
            }
        }
        Cmd::Import { dir, prefix, ext, batch } => import(st, &dir, &prefix, &ext, batch, author, json),
        Cmd::Export { prefix, dir, dry_run } => export(st, &prefix, &dir, dry_run, json),
        Cmd::Sync {
            prefix,
            dir,
            ext,
            base,
            dry_run,
            commit,
            accept_rules,
            prune_empty_dirs,
            push,
            pull,
            lock_timeout,
            no_wait,
        } => sync::sync(
            st,
            sync::Options {
                prefix,
                dir,
                exts: sync::parse_exts(&ext),
                base_rev: base,
                dry_run,
                commit,
                author: cli.author.clone(),
                store: config::redact(&cli.store),
                accept_rules,
                prune_empty_dirs,
                lock_wait: if no_wait { Duration::ZERO } else { Duration::from_secs(lock_timeout) },
                assets: match (push, pull) {
                    (true, true) => Some("both".to_string()),
                    (true, false) => Some("push".to_string()),
                    (false, true) => Some("pull".to_string()),
                    (false, false) => None,
                },
            },
            json,
        ),
        Cmd::Sql {
            query,
            params,
            write,
            full,
            dry_run,
            file,
            format,
        } => {
            let statement = match (file, query.as_deref()) {
                (Some(file), _) => String::from_utf8(read_file(&file)?)
                    .map_err(|_| StoreError::invalid(format!("{} is not UTF-8 text", file.display())))?,
                (None, None | Some("-")) => {
                    let mut text = String::new();
                    std::io::stdin().read_to_string(&mut text)?;
                    text
                }
                (None, Some(text)) => text.to_string(),
            };
            let format = if json { sql_query::SqlFormat::Json } else { format };
            let options = sql_query::SqlOptions { write, dry_run, full, format };
            sql_query::run(st, &statement, &params, author, options)
        }
        Cmd::RevertBatch {
            batch,
            skip_changed,
            dry_run,
        } => {
            let r = st.revert_batch(&batch, author, skip_changed, dry_run)?;
            if json {
                return emit_json(&r);
            }
            out(sql_query::revert_text(&r).as_bytes())
        }
        Cmd::GitStatus { prefix, dir, rev, ext } => sync::git_status(st, &prefix, &dir, &rev, &sync::parse_exts(&ext), json),
        Cmd::Ls {
            path,
            long,
            paths,
            sort,
            reverse,
            recursive,
        } => {
            // `ls FILE` lists that one file, as Unix does; it used to print nothing at all.
            let mut entries = match st.stat(&path) {
                Ok(e) if e.kind == "file" => vec![e],
                _ => st.ls(&path, recursive)?,
            };
            sort_entries(&mut entries, sort, reverse, recursive);
            if json {
                return emit_json(&entries);
            }
            if paths {
                // The trailing `/` marks a folder here too: without it a script could not tell
                // `/guides` the folder from `/guides` a file with no extension.
                let mark = |e: &Entry| if e.kind == "folder" { format!("{}/\n", e.path) } else { format!("{}\n", e.path) };
                return out(entries.iter().map(mark).collect::<String>().as_bytes());
            }
            out(ls_text(&entries, long, recursive).as_bytes())
        }
        Cmd::Tree { path, depth, dirs } => tree(st, &path, depth, dirs, json),
        Cmd::Stat { path } => {
            let e = st.stat(&path)?;
            if json {
                return emit_json(&e);
            }
            // One key per line: `stat` is the "what is this" command, and an agent reaching
            // for it first should see everything the store knows rather than seven fields.
            line(stat_text(&e))
        }
        Cmd::Cat { path, number, lines, at, section } => cat(st, &path, number, lines.as_deref(), at, section.as_deref(), json),
        Cmd::Search {
            query,
            prefix,
            limit,
            per_file,
            files_with_matches,
            count,
        } => search::search(
            st,
            &query.join(" "),
            &prefix,
            search::Options {
                mode: mode_of(files_with_matches, count),
                limit,
                per_file,
                ignore_case: false,
                fixed: false,
            },
            json,
        ),
        Cmd::Grep {
            pattern,
            prefix,
            ignore_case,
            fixed_strings,
            files_with_matches,
            count,
            limit,
            per_file,
        } => search::grep(
            st,
            &pattern,
            &prefix,
            search::Options {
                mode: mode_of(files_with_matches, count),
                limit,
                per_file,
                ignore_case,
                fixed: fixed_strings,
            },
            json,
        ),
        Cmd::Write {
            path,
            base_version,
            file,
            message,
            allow_empty,
            create,
        } => {
            let content = input(file.as_deref(), "write")?;
            if content.is_empty() && !allow_empty {
                return Err(StoreError::invalid(
                    "refusing to write empty content (nothing on stdin?); pass --allow-empty to mean it",
                ));
            }
            if create {
                match st.stat(&path) {
                    Ok(s) => {
                        return Err(StoreError::invalid(format!(
                            "{} exists already (v{}); --create only makes new files",
                            s.path, s.version.unwrap_or(0)
                        )))
                    }
                    Err(e) if e.code == "TX003" => {}
                    Err(e) => return Err(e),
                }
            }
            let w = st.write(&path, &content, base_version, author, message.as_deref())?;
            emit_written(&path, &w, json)
        }
        Cmd::Edit {
            path,
            old,
            new,
            old_file,
            new_file,
            stdin_json,
            message,
        } => {
            let (old, new) = if stdin_json {
                #[derive(Deserialize)]
                struct Pair {
                    old: String,
                    new: String,
                }
                let raw = input(None, "edit --stdin-json")?;
                let pair: Pair = serde_json::from_slice(&raw)
                    .map_err(|e| StoreError::invalid(format!("stdin must be {{\"old\": …, \"new\": …}}: {e}")))?;
                (pair.old.into_bytes(), pair.new.into_bytes())
            } else {
                (text_or_file(old, old_file, "old")?, text_or_file(new, new_file, "new")?)
            };
            let w = st.edit(&path, &old, &new, author, message.as_deref())?;
            emit_written(&path, &w, json)
        }
        Cmd::Links { path, broken, dir } => links::links(st, &path, broken, dir.as_deref(), json),
        Cmd::Backlinks { path } => links::backlinks(st, &path, json),
        Cmd::Assets { op } => match op {
            AssetsOp::Stores { add, driver, root, remove, bind } => {
                assets::stores(st, assets::StoresOptions { add, driver, root, remove, bind }, json)
            }
            AssetsOp::Status { path, dir } => assets::status(st, path.as_deref(), dir.as_deref(), json),
            AssetsOp::Push { paths, dir, to, message, dry_run, force } => assets::push(
                st,
                &paths,
                dir.as_deref(),
                assets::PushOptions { to: to.as_deref(), message: message.as_deref(), author, dry_run, force },
                json,
            ),
            AssetsOp::Pull { paths, dir, linked_from, dry_run } => assets::pull(st, &paths, dir.as_deref(), linked_from.as_deref(), dry_run, json),
            AssetsOp::Verify { path, dir } => assets::verify(st, path.as_deref(), dir.as_deref(), json),
            AssetsOp::Gitignore { path, dir, dry_run } => assets::gitignore(st, path.as_deref(), dir.as_deref(), dry_run, json),
            AssetsOp::MigrateFromGit { path, dir, to, message, dry_run } => assets::migrate::migrate_from_git(
                st,
                path.as_deref(),
                dir.as_deref(),
                assets::migrate::MigrateOptions { to: to.as_deref(), message: message.as_deref(), author, dry_run },
                json,
            ),
        },
        Cmd::Outline {
            path,
            heading,
            match_,
            level,
            names,
            limit,
        } => outline::run(st, &path, heading.as_deref(), &match_, level, names, limit, json),
        Cmd::Meta { op } => match op {
            MetaOp::Get { path, key } => meta::get(st, &path, key.as_deref(), json),
            MetaOp::Keys { prefix, limit } => meta::keys(st, prefix.as_deref().unwrap_or(""), limit, json),
            MetaOp::Values { key, prefix, limit } => meta::values(st, &key, prefix.as_deref().unwrap_or(""), limit, json),
            MetaOp::Find {
                query,
                folder,
                limit,
                show,
            } => meta::find(st, query.as_deref().unwrap_or(""), &folder, limit, show.as_deref(), json),
            MetaOp::Set {
                path,
                key,
                values,
                list,
                raw,
                message,
            } => {
                let value = match (values.len(), list, raw) {
                    (_, true, _) => meta::NewValue::List(values),
                    (0, false, _) => {
                        return Err(StoreError::invalid(
                            "meta set: give a value (several values, or --list, make a list; --list alone an empty one)",
                        ))
                    }
                    (_, false, true) => meta::NewValue::Raw(values.join(" ")),
                    (1, false, false) => meta::NewValue::Text(values.into_iter().next().unwrap_or_default()),
                    (_, false, false) => meta::NewValue::List(values),
                };
                meta::set(st, &path, &key, Some(value), message.as_deref(), author, json)
            }
            MetaOp::Unset { path, key, message } => meta::set(st, &path, &key, None, message.as_deref(), author, json),
        },
        Cmd::ReplaceLines {
            path,
            from,
            to,
            base_version,
            text,
            file,
            stdin_json,
            message,
        } => {
            if stdin_json {
                let body = input(None, "replace-lines --stdin-json")?;
                let ranges: Vec<store::LineRange> = serde_json::from_slice(&body).map_err(|e| {
                    StoreError::invalid(format!("--stdin-json wants [{{\"from\": N, \"to\": N, \"text\": \"…\"}}, …]: {e}"))
                })?;
                let w = st.replace_ranges(&path, &ranges, base_version, author, message.as_deref())?;
                return emit_written(&path, &w, json);
            }
            let (Some(from), Some(to)) = (from, to) else {
                return Err(StoreError::invalid("pass FROM and TO, or --stdin-json"));
            };
            if from < 1 || to < from - 1 {
                return Err(StoreError::invalid(format!(
                    "invalid line range {from}..{to}: FROM starts at 1, and TO is at least FROM-1 (which inserts)"
                )));
            }
            let body = match (text, file) {
                (Some(t), _) => t.into_bytes(),
                (None, file) => {
                    let body = input(file.as_deref(), "replace-lines")?;
                    if body.is_empty() && file.is_none() {
                        return Err(StoreError::invalid(
                            "no replacement text on stdin; to delete the lines pass --text ''",
                        ));
                    }
                    body
                }
            };
            let w = st.replace_lines(&path, from, to, &body, base_version, author, message.as_deref())?;
            emit_written(&path, &w, json)
        }
        Cmd::Append { path, text, message } => {
            let tail = match text {
                // An argument is a line of text; stdin is taken as it comes.
                Some(t) if t.ends_with('\n') => t.into_bytes(),
                Some(t) => format!("{t}\n").into_bytes(),
                None => input(None, "append")?,
            };
            let w = st.append(&path, &tail, author, message.as_deref())?;
            emit_written(&path, &w, json)
        }
        Cmd::History { path, versions_only } => {
            let commits = st.history(&path)?;
            // `--versions-only` filters the rows; it does not drop the `type` tag that tells
            // a version from a path event, which used to make one command emit two shapes.
            let events = if versions_only { Vec::new() } else { st.path_history(&path)? };
            let mut items: Vec<HistoryItem> =
                commits.iter().map(HistoryItem::Version).chain(events.iter().map(HistoryItem::Path)).collect();
            // Stable, so a version and a path event at the same instant keep the version first.
            items.sort_by(|a, b| a.ts().cmp(b.ts()));
            if json {
                return emit_json(&items);
            }
            out(history_text(&items).as_bytes())
        }
        Cmd::Diff { path, v1, v2 } => {
            let v2 = match v2 {
                Some(v) => v,
                // A folder has no version, and neither command accepts one, so this is a file.
                None => st.stat(&path)?.version.unwrap_or(0),
            };
            let diff = st.diff(&path, v1, v2)?;
            if json {
                emit_json(&json!({ "path": normalize_path(&path)?, "from": v1, "to": v2, "diff": diff }))
            } else {
                out(diff.as_bytes())
            }
        }
        Cmd::Hunks { path, v1, v2 } => {
            let v2 = match v2 {
                Some(v) => v,
                // A folder has no version, and neither command accepts one, so this is a file.
                None => st.stat(&path)?.version.unwrap_or(0),
            };
            let v1 = v1.unwrap_or(v2 - 1).max(0);
            let hunks = st.hunks(&path, v1, v2)?;
            if json {
                return emit_json(&json!({ "path": normalize_path(&path)?, "from": v1, "to": v2, "hunks": hunks }));
            }
            let mut s = String::new();
            for h in &hunks {
                s.push_str(&format!("@@ -{},{} +{},{} @@\n", h.old_from, h.old_count, h.new_from, h.new_count));
                for (sign, text) in [('-', &h.old_text), ('+', &h.new_text)] {
                    for l in text.split_inclusive('\n') {
                        s.push(sign);
                        s.push_str(l);
                        if !l.ends_with('\n') {
                            s.push('\n');
                        }
                    }
                }
            }
            out(s.as_bytes())
        }
        Cmd::Chunks { path, at } => {
            let chunks = st.chunks(&path, at)?;
            if json {
                return emit_json(&chunks);
            }
            let s: String = chunks
                .iter()
                .map(|c| {
                    format!(
                        "{:>5}  lines {:>6}-{:<6} {:>8}  {}\n",
                        c.ord,
                        c.line_from,
                        c.line_from + c.nlines.max(1) - 1,
                        human_bytes(c.nbytes),
                        &c.hash[..16.min(c.hash.len())]
                    )
                })
                .collect();
            out(s.as_bytes())
        }
        Cmd::Mv {
            from,
            to,
            message,
            keep_empty_folders,
            update_links,
            no_update_links,
        } => {
            let update = if update_links { Some(true) } else if no_update_links { Some(false) } else { None };
            // An asset is named by its own path: its pointer moves, and the next sync moves the file.
            let from = store_or_pointer(st, &from);
            let to = if assets::pointer::is_asset_pointer(&from) { pointer_destination(st, &from, &to)? } else { document_destination(st, &to)? };
            let untracked = untracked_on_disk(st, &from);
            let moved_links = st.mv_links(&from, &to, author, message.as_deref(), update)?;
            let removed = if keep_empty_folders { Vec::new() } else { prune_empty_folders(st, &from, author)? };
            if json {
                return emit_json(&json!({
                    "moved": from, "to": to, "removed_empty_folders": removed, "links": moved_links,
                    "untracked_on_disk": untracked_json(&untracked),
                }));
            }
            let mut s = format!("moved {from} -> {to}\n");
            for folder in &removed {
                s.push_str(&format!("removed empty folder {folder}\n"));
            }
            s.push_str(&links::moved_text(&from, &moved_links));
            for (disk, n, kinds) in &untracked {
                s.push_str(&format!(
                    "note: {disk} also holds {n} {} textdb does not track ({kinds}); the next sync moves them along with the folder\n",
                    if *n == 1 { "file" } else { "files" }
                ));
            }
            out(s.as_bytes())
        }
        Cmd::Rm {
            path,
            message,
            keep_empty_folders,
        } => {
            let path = store_or_pointer(st, &path);
            let untracked = untracked_on_disk(st, &path);
            // Links from elsewhere into what is deleted, unless the store turned link reports off.
            let broken: Vec<store::LinkRow> = match st.setting(textdb_sqlite::links::LINK_UPDATES_SETTING) {
                Ok(mode) if mode.as_deref() != Some("off") => {
                    let inside = format!("{path}/");
                    st.backlinks(&path).unwrap_or_default().into_iter().filter(|l| l.path != path && !l.path.starts_with(&inside)).collect()
                }
                _ => Vec::new(),
            };
            st.rm(&path, author, message.as_deref())?;
            let removed = if keep_empty_folders { Vec::new() } else { prune_empty_folders(st, &path, author)? };
            if json {
                return emit_json(&json!({
                    "deleted": path, "removed_empty_folders": removed, "broken_links": broken,
                    "untracked_on_disk": untracked_json(&untracked),
                }));
            }
            let mut s = format!("deleted {path}\n");
            for folder in &removed {
                s.push_str(&format!("removed empty folder {folder}\n"));
            }
            s.push_str(&links::deleted_text(&broken));
            for (disk, n, kinds) in &untracked {
                s.push_str(&format!(
                    "note: {disk} also holds {n} {} textdb does not track ({kinds}); they stay on disk after the next sync\n",
                    if *n == 1 { "file" } else { "files" }
                ));
            }
            out(s.as_bytes())
        }
        Cmd::Setting { key, value } => {
            let key = key.unwrap_or_else(|| textdb_core::PATH_HISTORY_SETTING.to_string());
            if let Some(v) = &value {
                st.set_setting(&key, if v == "default" { None } else { Some(v.as_str()) })?;
            }
            let stored = st.setting(&key)?;
            if key == textdb_sqlite::links::LINK_UPDATES_SETTING {
                let effective = stored.clone().unwrap_or_else(|| textdb_sqlite::links::LinkUpdates::DEFAULT.as_str().to_string());
                if json {
                    return emit_json(&json!({ key.as_str(): { "value": stored, "effective": effective } }));
                }
                return line(format!("{key}  {}", stored.unwrap_or_else(|| format!("{effective} (default)"))));
            }
            let asset_default = match key.as_str() {
                "asset_sync" => Some("off"),
                "asset_pull" => Some("linked"),
                _ => None,
            };
            if let Some(default) = asset_default {
                let effective = stored.clone().unwrap_or_else(|| default.to_string());
                if json {
                    return emit_json(&json!({ key.as_str(): { "value": stored, "effective": effective } }));
                }
                return line(format!("{key}  {}", stored.unwrap_or_else(|| format!("{default} (default)"))));
            }
            let effective = st.path_history_enabled()?;
            if json {
                return emit_json(&json!({ key.as_str(): { "value": stored, "effective": effective } }));
            }
            let default = if textdb_core::PATH_HISTORY_DEFAULT { "on" } else { "off" };
            let shown = stored.map_or_else(|| format!("{default} (default)"), |v| v);
            let session = match cli.path_history {
                Some(on) => format!("; {} for this command (--path-history / TEXTDB_PATH_HISTORY)", if on { "on" } else { "off" }),
                None => String::new(),
            };
            line(format!("{key}  {shown}{session}"))
        }
        Cmd::Log { since, limit } => {
            let changes = st.feed(since, limit)?;
            if json {
                return emit_json(&changes);
            }
            let s: String = changes.iter().map(|c| change_line(c) + "\n").collect();
            out(s.as_bytes())
        }
        Cmd::Watch { since, prefix } => watch(st, since, &prefix, json),
    }
}

/// One entry of a file's history: a version, or a rename, move or delete that touched it.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum HistoryItem<'a> {
    Version(&'a Commit),
    Path(&'a PathEvent),
}

impl HistoryItem<'_> {
    fn ts(&self) -> &str {
        match self {
            HistoryItem::Version(c) => &c.ts,
            HistoryItem::Path(e) => &e.ts,
        }
    }
}

fn history_text(items: &[HistoryItem]) -> String {
    let mut s = String::new();
    for item in items {
        match item {
            HistoryItem::Version(c) => {
                let mut how = c.kind.clone().unwrap_or_default();
                if let Some(b) = c.base_version.filter(|b| *b != c.version - 1) {
                    how.push_str(&format!(" from v{b}"));
                }
                s.push_str(&format!(
                    "v{:<5} {}  {:<14} {:<16} {}\n",
                    c.version,
                    c.ts,
                    c.author.as_deref().unwrap_or("-"),
                    how,
                    c.message.as_deref().unwrap_or("")
                ));
            }
            HistoryItem::Path(e) => {
                let what = match e.op.as_str() {
                    "rename" => "renamed",
                    "move" => "moved",
                    "delete" => "deleted",
                    other => other,
                };
                let mut detail = match &e.new_path {
                    Some(to) => format!("{} -> {to}", e.old_path),
                    None => e.old_path.clone(),
                };
                if let Some(via) = &e.via {
                    detail.push_str(&format!("  (with {via})"));
                }
                s.push_str(&format!("{:<6} {}  {:<14} {:<16} {}\n", "", e.ts, e.author.as_deref().unwrap_or("-"), what, detail));
            }
        }
    }
    s
}

// ---------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------

/// Write to stdout. A reader that went away (`textdb cat big.md | head`) ends the program
/// quietly instead of as an error.
fn out(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(bytes).and_then(|_| stdout.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        r => r.map_err(StoreError::from),
    }
}

fn line(s: impl Into<String>) -> Result<()> {
    let mut s = s.into();
    s.push('\n');
    out(s.as_bytes())
}

fn emit_json<T: Serialize + ?Sized>(v: &T) -> Result<()> {
    line(serde_json::to_string(v).map_err(StoreError::other)?)
}

fn emit_written(path: &str, w: &Written, json: bool) -> Result<()> {
    if json {
        emit_json(&json!({ "path": normalize_path(path)?, "version": w.version, "kind": w.kind }))
    } else if w.kind == "noop" {
        line(format!("{path}: unchanged, still v{}", w.version))
    } else {
        line(format!("{path}: v{} ({})", w.version, w.kind))
    }
}

fn report(e: &StoreError, json: bool) {
    if json {
        let _ = emit_json(&json!({ "error": e }));
        return;
    }
    eprintln!("{}: {}", e.code, e.message);
    if let Some(c) = &e.conflict {
        let text = |k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        eprintln!(
            "the same lines changed since your base version (lines {}-{}, now v{})",
            c["region_line_from"], c["region_line_to"], c["current_version"]
        );
        eprintln!("--- base\n{}--- theirs (current text)\n{}--- ours\n{}", text("base"), text("theirs"), text("ours"));
        eprintln!("Rebuild the change on 'theirs' and write again with --base-version {}.", c["current_version"]);
    }
}

#[derive(Serialize, Default)]
struct ExportReport {
    prefix: String,
    dir: String,
    dry_run: bool,
    new: Vec<String>,
    changed: Vec<String>,
    unchanged: usize,
    skipped: Vec<ExportSkip>,
    problems: Vec<portable::Problem>,
    /// Blocking problems: nothing was written.
    stopped: bool,
    written: usize,
    bytes: u64,
}

#[derive(Serialize)]
struct ExportSkip {
    path: String,
    reason: String,
}

/// The names in `dir`, keyed as the file system on `here` compares them; `None` when unreadable.
fn names_in(dir: &Path, here: portable::Platform) -> Option<HashMap<String, String>> {
    let entries = std::fs::read_dir(dir).ok()?;
    Some(
        entries
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                (portable::fold(&name, here), name)
            })
            .collect(),
    )
}

/// `textdb export`: plan against what is in `dir`, then write only new and changed files.
fn export(st: &mut dyn Store, prefix: &str, dir: &Path, dry_run: bool, json: bool) -> Result<()> {
    let prefix = normalize_path(prefix)?;
    let here = portable::Platform::current();
    let files: Vec<Entry> = st.nodes(&prefix)?.into_iter().filter(|e| e.kind == "file").collect();
    let base = if prefix == "/" { 0 } else { prefix.len() };
    let rels: Vec<String> = files
        .iter()
        .map(|f| if f.path == prefix { f.name.clone() } else { f.path[base..].trim_start_matches('/').to_string() })
        .collect();
    let mut problems = portable::Problems::new(here);
    portable::check_names(&rels, &mut problems);
    let mut report = ExportReport {
        prefix: prefix.clone(),
        dir: dir.display().to_string(),
        dry_run,
        ..Default::default()
    };

    let mut listings: HashMap<PathBuf, Option<HashMap<String, String>>> = HashMap::new();
    let mut to_write: Vec<usize> = Vec::new();
    'files: for (i, f) in files.iter().enumerate() {
        let rel = &rels[i];
        let segs: Vec<&str> = rel.split('/').collect();
        let mut target = dir.to_path_buf();
        for (k, seg) in segs.iter().enumerate() {
            let last = k + 1 == segs.len();
            let shown = if last { rel.clone() } else { format!("{}/", segs[..=k].join("/")) };
            // Where case does not count, `README.md` would overwrite a `Readme.md` already there.
            if here.case_insensitive() {
                let names = listings.entry(target.clone()).or_insert_with(|| names_in(&target, here));
                if let Some(on_disk) = names.as_ref().and_then(|n| n.get(&portable::fold(seg, here))) {
                    if on_disk != seg {
                        problems.add(shown, "disk-case", format!("exists on disk as “{on_disk}”"), &[here]);
                        continue 'files;
                    }
                }
            }
            target.push(seg);
            if !last && target.exists() && !target.is_dir() {
                problems.add(shown, "disk-kind", "is a file on disk; the export needs a folder here".into(), &portable::ALL);
                continue 'files;
            }
        }
        match std::fs::symlink_metadata(&target) {
            Err(_) => {
                report.new.push(rel.clone());
                to_write.push(i);
            }
            Ok(meta) if meta.is_dir() => {
                problems.add(rel.clone(), "disk-kind", "is a folder on disk; the export needs a file here".into(), &portable::ALL)
            }
            Ok(meta) => {
                let disk_len = std::fs::metadata(&target).map(|m| m.len()).ok();
                let same = disk_len.is_some()
                    && disk_len == Some(f.nbytes as u64)
                    && std::fs::read(&target).ok() == Some(st.read(&f.path, None)?.0);
                if same {
                    report.unchanged += 1;
                } else if meta.file_type().is_symlink() {
                    report.skipped.push(ExportSkip {
                        path: rel.clone(),
                        reason: "a symbolic link on disk, left as it is".into(),
                    });
                } else {
                    report.changed.push(rel.clone());
                    to_write.push(i);
                }
            }
        }
    }
    report.problems = problems.into_vec();
    let blocking = report.problems.iter().filter(|p| p.blocking).count();
    report.stopped = blocking > 0;

    if !report.stopped && !dry_run {
        for &i in &to_write {
            let (body, _) = st.read(&files[i].path, None)?;
            let target = dir.join(&rels[i]);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // An existing file is truncated in place rather than replaced, so it keeps its
            // permissions: an executable script stays executable.
            let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&target)?;
            file.write_all(&body)?;
            report.written += 1;
            report.bytes += body.len() as u64;
        }
    }

    if json {
        emit_json(&report)?;
        if report.stopped {
            std::process::exit(6);
        }
        return Ok(());
    }
    let mut s = String::new();
    if dry_run {
        for r in &report.new {
            s.push_str(&format!("new      {r}\n"));
        }
        for r in &report.changed {
            s.push_str(&format!("changed  {r}\n"));
        }
    }
    for k in &report.skipped {
        s.push_str(&format!("skipped  {}: {}\n", k.path, k.reason));
    }
    for p in &report.problems {
        if p.blocking {
            s.push_str(&format!("problem  {}: {}\n", p.path, p.detail));
        } else {
            let on: Vec<&str> = p.platforms.iter().map(|x| x.name()).collect();
            s.push_str(&format!("warning  {}: {} (on {})\n", p.path, p.detail, on.join(", ")));
        }
    }
    let counts = format!("{} new, {} changed, {} unchanged", report.new.len(), report.changed.len(), report.unchanged);
    if report.stopped {
        s.push_str(&format!("nothing written: {counts}\n"));
    } else if dry_run {
        s.push_str(&format!("dry run: {counts}; nothing written\n"));
    } else {
        s.push_str(&format!(
            "exported {prefix} to {}: {counts}; wrote {} files, {}\n",
            dir.display(),
            report.written,
            human_bytes(report.bytes as i64)
        ));
    }
    out(s.as_bytes())?;
    if report.stopped {
        return Err(StoreError::invalid(format!(
            "export stopped: {blocking} {} cannot be written on this computer; rename {} in the store",
            if blocking == 1 { "name" } else { "names" },
            if blocking == 1 { "it" } else { "them" }
        )));
    }
    Ok(())
}

/// What `ls --sort` orders by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum SortKey {
    Name,
    /// The file extension.
    Type,
    Size,
    Lines,
    Words,
    Versions,
    Created,
    Updated,
    /// The number of authors.
    Authors,
}

/// Order a listing by `key`, ties by name (by path when recursive). Folders come first except
/// in a recursive listing, where each folder stays in front of what is inside it by path.
fn sort_entries(entries: &mut [Entry], key: SortKey, reverse: bool, recursive: bool) {
    let ext = |e: &Entry| -> String {
        match e.name.rsplit_once('.') {
            Some((_, x)) if e.kind == "file" => x.to_ascii_lowercase(),
            _ => String::new(),
        }
    };
    entries.sort_by(|a, b| {
        let by = match key {
            SortKey::Name => std::cmp::Ordering::Equal,
            SortKey::Type => ext(a).cmp(&ext(b)),
            SortKey::Size => a.nbytes.cmp(&b.nbytes),
            SortKey::Lines => a.nlines.cmp(&b.nlines),
            SortKey::Words => a.nwords.cmp(&b.nwords),
            SortKey::Versions => a.versions.cmp(&b.versions),
            SortKey::Created => a.created_at.cmp(&b.created_at),
            SortKey::Updated => a.updated_at.cmp(&b.updated_at),
            SortKey::Authors => a.authors.len().cmp(&b.authors.len()),
        };
        let by = if recursive { by.then_with(|| a.path.cmp(&b.path)) } else { by.then_with(|| a.name.cmp(&b.name)) };
        let by = if reverse { by.reverse() } else { by };
        if recursive {
            by
        } else {
            (a.kind != "folder").cmp(&(b.kind != "folder")).then(by)
        }
    });
}

/// `stat` as one key per line: the full `Entry`, with the values that do not apply omitted
/// from the *text* (the JSON still carries every key as `null`).
fn stat_text(e: &Entry) -> String {
    let mut out = Vec::new();
    let mut add = |k: &str, v: String| out.push(format!("{k:<14} {v}"));
    add("path", e.path.clone());
    add("name", e.name.clone());
    add("kind", e.kind.clone());
    if let Some(v) = e.version {
        add("version", format!("v{v}"));
    }
    add("nbytes", format!("{} ({})", e.nbytes, human_bytes(e.nbytes)));
    add("nlines", e.nlines.to_string());
    add("nwords", e.nwords.to_string());
    add("updated_at", e.updated_at.clone());
    if let Some(by) = &e.updated_by {
        add("updated_by", by.clone());
    }
    add("created_at", e.created_at.clone());
    if let Some(d) = &e.dir {
        add("dir", d.clone());
    }
    add("depth", e.depth.to_string());
    if let Some(x) = &e.ext {
        add("ext", x.clone());
    }
    if let Some(t) = &e.title {
        add("title", t.clone());
    }
    add("nsections", e.nsections.to_string());
    add("nprops", e.nprops.to_string());
    add("nlinks", format!("{} ({} broken)", e.nlinks, e.nlinks_broken));
    add("versions", e.versions.to_string());
    if e.kind == "folder" {
        add("files", e.files.unwrap_or(0).to_string());
        add("folders", e.folders.unwrap_or(0).to_string());
    }
    add("nauthors", e.nauthors.to_string());
    if !e.authors.is_empty() {
        let who: Vec<String> =
            e.authors.iter().map(|a| format!("{} ({})", a.author.as_deref().unwrap_or("-"), a.commits)).collect();
        add("authors", who.join(", "));
    }
    add("id", e.id.to_string());
    out.join("\n")
}

/// `-l` and `-c` pick the same two modes on both commands.
fn mode_of(files_only: bool, count: bool) -> search::Mode {
    match (files_only, count) {
        (true, _) => search::Mode::Files,
        (_, true) => search::Mode::Count,
        _ => search::Mode::Lines,
    }
}

/// The short listing is the minimal tier: size, lines, the version, and the name.
///
/// The version is there because every line-numbered edit needs one, and without it a short
/// listing was not enough to start one.
fn ls_text(entries: &[Entry], long: bool, recursive: bool) -> String {
    let name = |e: &Entry| {
        let n = if recursive { &e.path } else { &e.name };
        if e.kind == "folder" {
            format!("{n}/")
        } else {
            n.clone()
        }
    };
    let mut s = String::new();
    if !long {
        for e in entries {
            let version = e.version.map_or(String::new(), |v| format!("v{v}"));
            s.push_str(&format!(
                "{:>9}  {:>7}  {:>4}  {}\n",
                human_bytes(e.nbytes),
                format!("{}L", e.nlines),
                version,
                name(e)
            ));
        }
        return s;
    }
    // The full tier. `LINKS` is total/broken; `UPDATED` keeps the seconds and the `Z` rather
    // than a 16-character cut that read as local time to anyone not in UTC.
    s.push_str(&format!(
        "{:>9} {:>7} {:>8} {:>5} {:>6} {:>7} {:>5}  {:<24}  {:<28}  {}\n",
        "SIZE", "LINES", "WORDS", "SECT", "PROPS", "LINKS", "VERS", "UPDATED", "BY / CONTAINS", "NAME"
    ));
    for e in entries {
        let who = if e.kind == "folder" {
            format!("{}, {}", plural(e.files.unwrap_or(0) as usize, "file"), plural(e.folders.unwrap_or(0) as usize, "folder"))
        } else {
            let mut names: Vec<String> =
                e.authors.iter().take(2).map(|a| format!("{} ({})", a.author.as_deref().unwrap_or("-"), a.commits)).collect();
            if e.authors.len() > 2 {
                names.push(format!("+{}", e.authors.len() - 2));
            }
            names.join(", ")
        };
        let links = if e.nlinks_broken > 0 {
            format!("{}/{}", e.nlinks, e.nlinks_broken)
        } else {
            e.nlinks.to_string()
        };
        s.push_str(&format!(
            "{:>9} {:>7} {:>8} {:>5} {:>6} {:>7} {:>5}  {:<24}  {:<28}  {}\n",
            human_bytes(e.nbytes),
            e.nlines,
            e.nwords,
            e.nsections,
            e.nprops,
            links,
            e.versions,
            e.updated_at,
            who,
            name(e)
        ));
    }
    s
}

/// A byte count for a human to read.
///
/// One formatter for the whole CLI. There were two, and they disagreed above a gigabyte: this
/// one stopped at `GB` and the assets one went to `TB`, so the same number printed differently
/// depending on which command showed it.
fn human_bytes(n: i64) -> String {
    assets::size_text(n.max(0) as u64)
}

fn change_line(c: &Change) -> String {
    let mut s = format!("#{} {} {:<6} {}", c.seq, c.ts, c.op, c.path);
    if let Some(old) = &c.old_path {
        s.push_str(&format!(" (from {old})"));
    }
    if let Some(v) = c.version {
        s.push_str(&format!(" v{v}"));
        if let Some(kind) = c.commit_kind.as_deref().filter(|k| *k != "direct") {
            s.push_str(&format!(" {kind}"));
        }
        if let Some(b) = c.base_version.filter(|b| *b != v - 1) {
            s.push_str(&format!(" from v{b}"));
        }
    }
    if let Some(a) = &c.author {
        s.push_str(&format!(" by {a}"));
    }
    if let Some(m) = &c.message {
        s.push_str(&format!(" · {m}"));
    }
    s
}

// ---------------------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------------------

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| StoreError::other(format!("{}: {e}", path.display())))
}

/// Content from `file`, else stdin — unless stdin is a terminal, where waiting for input
/// would look like a hang.
fn input(file: Option<&Path>, what: &str) -> Result<Vec<u8>> {
    if let Some(path) = file {
        return read_file(path);
    }
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(StoreError::invalid(format!("{what}: pass --file or pipe the text on stdin")));
    }
    let mut buf = Vec::new();
    stdin.read_to_end(&mut buf)?;
    Ok(buf)
}

fn text_or_file(text: Option<String>, file: Option<PathBuf>, name: &str) -> Result<Vec<u8>> {
    match (text, file) {
        (Some(t), _) => Ok(t.into_bytes()),
        (None, Some(f)) => read_file(&f),
        (None, None) => Err(StoreError::invalid(format!("pass --{name}, --{name}-file or --stdin-json"))),
    }
}

/// `A:B`, `A:`, `:B`, `A-B` or `A`.
fn parse_range(s: &str) -> Result<(Option<i64>, Option<i64>)> {
    let bad = || StoreError::invalid(format!("line range '{s}' should look like 10:20, 10:, :20 or 10"));
    let num = |t: &str| -> Result<Option<i64>> {
        let t = t.trim();
        if t.is_empty() {
            Ok(None)
        } else {
            t.parse().map(Some).map_err(|_| bad())
        }
    };
    match s.split_once(':').or_else(|| s.split_once('-')) {
        Some((a, b)) => Ok((num(a)?, num(b)?)),
        None => {
            let n = num(s)?.ok_or_else(bad)?;
            Ok((Some(n), Some(n)))
        }
    }
}

// ---------------------------------------------------------------------------------------
// Commands with more than a few lines of logic
// ---------------------------------------------------------------------------------------

fn cat(
    st: &mut dyn Store,
    path: &str,
    number: bool,
    lines: Option<&str>,
    at: Option<i64>,
    section: Option<&str>,
    json: bool,
) -> Result<()> {
    let (content, version) = st.read(path, at)?;
    let all: Vec<&[u8]> = content.split_inclusive(|&c| c == b'\n').collect();
    let total = all.len() as i64;
    let (mut from, mut to) = (1, total);
    if let Some(heading) = section {
        let body = st
            .section(path, heading)?
            .ok_or_else(|| StoreError::not_found(format!("no section '{heading}' in {path}")))?;
        let pos = content
            .windows(body.len().max(1))
            .position(|w| w == body.as_slice())
            .ok_or_else(|| StoreError::other(format!("{path} changed while reading; try again")))?;
        from = content[..pos].iter().filter(|&&c| c == b'\n').count() as i64 + 1;
        to = from + body.split_inclusive(|&c| c == b'\n').count() as i64 - 1;
    }
    if let Some(range) = lines {
        let (a, b) = parse_range(range)?;
        from = from.max(a.unwrap_or(from));
        to = to.min(b.unwrap_or(to));
    }
    let from = from.max(1);
    let to = to.min(total);
    let selected: &[&[u8]] = if from <= to { &all[(from - 1) as usize..to as usize] } else { &[] };
    // Echo the path the store knows, not the one the user typed: `cat guides/x.md` used to
    // answer `"path":"guides/x.md"` while every listing said `/guides/x.md`, so a caller
    // keying on `path` saw two spellings of one file.
    let path = &normalize_path(path)?;
    if json {
        return emit_json(&json!({
            "path": path,
            "version": version,
            "nlines": total,
            "from": from,
            "to": to,
            "content": String::from_utf8_lossy(&selected.concat()),
        }));
    }
    if !number {
        return out(&selected.concat());
    }
    let mut s = format!("{path} v{version} · lines {from}-{to} of {total}\n");
    for (i, l) in selected.iter().enumerate() {
        let text = String::from_utf8_lossy(l);
        s.push_str(&format!("{:>6}\t{text}", from + i as i64));
        if !text.ends_with('\n') {
            s.push('\n');
        }
    }
    out(s.as_bytes())
}

/// Let the store get ready after a bulk load, and say so if it could not.
///
/// A store that will not settle is slower to query, not broken, so this never fails the
/// command that just succeeded in writing everything.
pub fn settle(st: &mut dyn Store) {
    if let Err(e) = st.settle() {
        eprintln!("note: the store could not refresh its query statistics ({}); queries may plan badly until it does", e.message);
    }
}

fn import(
    st: &mut dyn Store,
    dir: &Path,
    prefix: &str,
    ext: &str,
    batch: usize,
    author: Option<&str>,
    json: bool,
) -> Result<()> {
    let exts: Vec<String> = ext
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    let files = collect_files(dir, &exts)?;
    let total = files.len();
    let prefix = normalize_path(prefix)?;
    let base = if prefix == "/" { "" } else { prefix.as_str() };
    let interactive = std::io::stderr().is_terminal() && !json;
    let started = Instant::now();
    let mut contents = files.into_iter().filter_map(|(rel, local)| match std::fs::read(&local) {
        // A binary file (a NUL byte in its first 8000 bytes, as git tells) is not text for the store.
        Ok(body) if body.iter().take(8000).any(|&c| c == 0) => {
            eprintln!("skipped {}: a binary file, not imported (narrow --ext, or keep it in an asset store)", local.display());
            None
        }
        Ok(body) => Some((format!("{base}/{rel}"), body)),
        Err(e) => {
            eprintln!("skipped {}: {e}", local.display());
            None
        }
    });
    let stats = st.import(
        &mut contents,
        author,
        batch.max(1),
        &mut |s: &ImportStats| {
            if interactive {
                eprint!("\r{} / {total} files", s.files);
            }
        },
        &mut |path: &str, e: &StoreError| {
            eprintln!("{}failed {path}: {} {}", if interactive { "\n" } else { "" }, e.code, e.message)
        },
    )?;
    let seconds = started.elapsed().as_secs_f64();
    if interactive {
        eprintln!();
    }
    settle(st);
    if json {
        return emit_json(&json!({ "dir": dir.display().to_string(), "prefix": prefix, "stats": stats, "seconds": seconds }));
    }
    line(format!(
        "{} files ({}) in {seconds:.1}s: {} created, {} updated, {} unchanged, {} failed",
        stats.files,
        human_bytes(stats.bytes as i64),
        stats.created,
        stats.updated,
        stats.unchanged,
        stats.failed
    ))
}

/// Files under `root` with one of `exts` (or any, for `*`), as `(relative/path, local path)`
/// sorted by the relative path. `.git`, `.textdb`, `.trash` and `node_modules` are skipped;
/// other hidden directories are read.
fn collect_files(root: &Path, exts: &[String]) -> Result<Vec<(String, PathBuf)>> {
    let any = exts.iter().any(|e| e == "*");
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| StoreError::other(format!("{}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if !sync::SKIP_DIRS.contains(&name.as_str()) {
                    dirs.push(entry.path());
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let wanted = any
                || Path::new(&name)
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
                    .is_some_and(|e| exts.contains(&e));
            if wanted {
                let path = entry.path();
                let rel = path
                    .strip_prefix(root)
                    .expect("walked from root")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                found.push((rel, path));
            }
        }
    }
    found.sort();
    Ok(found)
}

#[derive(Subcommand)]
enum AssetsOp {
    /// The asset stores declared in the store and how this computer reaches them; declare,
    /// remove or bind one.
    Stores {
        /// Declare an asset store with this name (or change the one of that name).
        #[arg(long, value_name = "NAME", conflicts_with = "remove")]
        add: Option<String>,
        /// Its driver: local (a folder) or rclone.
        #[arg(long, default_value = "local")]
        driver: String,
        /// Where it keeps files: a folder, or an rclone remote path (`teamdrive:textdb`).
        #[arg(long)]
        root: Option<String>,
        /// Remove the declaration of this asset store (its files stay where they are).
        #[arg(long, value_name = "NAME")]
        remove: Option<String>,
        /// Where this computer reaches an asset store: NAME=FOLDER (NAME= removes it). Kept in
        /// the config directory; TEXTDB_ASSET_STORE_<NAME> overrides it.
        #[arg(long, value_name = "NAME=LOCATION")]
        bind: Option<String>,
    },
    /// Each asset's state: ok, new (no pointer yet), modified (other bytes than its pointer
    /// names) or not-pulled (a pointer and no file).
    Status {
        /// A store folder or asset; the directory is the one it was last synced with.
        #[arg(value_parser = store_path)]
        path: Option<String>,
        /// The directory (a vault) to look at.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Upload new and changed assets to their asset store, check what arrived, then commit their
    /// pointers to the store and write them next to the files.
    Push {
        #[arg(value_parser = store_path)]
        paths: Vec<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
        /// The asset store for new assets (needed when several are declared).
        #[arg(long, value_name = "NAME")]
        to: Option<String>,
        /// Message of the pointer commits (default `assets push`).
        #[arg(long, short = 'm')]
        message: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Push conflicting files too: their bytes replace the asset store's copy (which goes to
        /// its trash).
        #[arg(long)]
        force: bool,
    },
    /// Download the assets whose pointers are here and whose files are not, checking each hash
    /// before putting the file in place.
    Pull {
        #[arg(value_parser = store_path)]
        paths: Vec<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Only the assets the notes at or below this path link to.
        #[arg(long, value_name = "PATH", value_parser = store_path)]
        linked_from: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Hash each asset here and check its asset store holds the bytes its pointer names.
    Verify {
        #[arg(value_parser = store_path)]
        path: Option<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Write the managed block of .gitignore patterns that keeps assets out of git and their
    /// pointers in.
    Gitignore {
        #[arg(value_parser = store_path)]
        path: Option<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Move the binaries git tracks to the asset store: push them, remove them from git's index
    /// (the files stay on disk), write the .gitignore block, and commit their pointers and
    /// .gitignore in one commit. Nothing may be staged beforehand. Git's history keeps the old
    /// blobs.
    MigrateFromGit {
        #[arg(value_parser = store_path)]
        path: Option<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
        /// The asset store for them (needed when several are declared).
        #[arg(long, value_name = "NAME")]
        to: Option<String>,
        /// Message of the pointer commits and the git commit (default `assets migrate-from-git`).
        #[arg(long, short = 'm')]
        message: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum MetaOp {
    /// A key's value (list items one per line), or the whole front matter without KEY.
    Get {
        #[arg(value_parser = store_path)]
        path: String,
        key: Option<String>,
    },
    /// Set a top-level key: its lines are replaced, or it is added at the end of the front
    /// matter (which is created when the file has none).
    Set {
        #[arg(value_parser = store_path)]
        path: String,
        key: String,
        /// The value. Several values make a list. Put a value starting with `-` after `--`.
        #[arg(num_args = 0.., allow_negative_numbers = true)]
        values: Vec<String>,
        /// Write a list, even of one value (or none).
        #[arg(long)]
        list: bool,
        /// Write the value as YAML, without quoting (`[a, b]`).
        #[arg(long, conflicts_with = "list")]
        raw: bool,
        /// Commit message (default `meta set KEY`).
        #[arg(long, short = 'm')]
        message: Option<String>,
    },
    /// Property names used anywhere in the store, most-used first.
    Keys {
        /// Only names starting with this.
        prefix: Option<String>,
        #[arg(long, default_value_t = 200)]
        limit: i64,
    },
    /// The values one property takes, most-used first.
    Values {
        key: String,
        /// Only values starting with this.
        prefix: Option<String>,
        #[arg(long, default_value_t = 200)]
        limit: i64,
    },
    /// Documents matching a property query: `status:draft tags:telco -priority:>3`.
    ///
    /// `key:value` equals, `has:key` exists, `key:>3` compares, `key:val*` starts with,
    /// `key:~val` contains, `key:!=val` has it but not as that. A space means AND; `OR`,
    /// `NOT` (or a leading `-`) and parentheses work as written. Quote a value with spaces.
    Find {
        /// The query. An empty one lists every document that has front matter.
        ///
        /// `allow_hyphen_values`: `-status:archived` is the documented way to negate a term,
        /// and without this clap reads it as `-s tatus:archived` — the store flag — and
        /// searches an empty store instead of complaining.
        #[arg(allow_hyphen_values = true)]
        query: Option<String>,
        /// Only below this folder.
        #[arg(long, default_value = "/")]
        folder: String,
        #[arg(long, default_value_t = 500)]
        limit: i64,
        /// Show these property columns, comma separated (`status,tags`).
        #[arg(long)]
        show: Option<String>,
    },
    /// Remove a top-level key and its lines.
    Unset {
        #[arg(value_parser = store_path)]
        path: String,
        key: String,
        /// Commit message (default `meta unset KEY`).
        #[arg(long, short = 'm')]
        message: Option<String>,
    },
}

/// `path`, or the pointer of the asset at `path` when the store holds nothing there itself.
fn store_or_pointer(st: &mut dyn Store, path: &str) -> String {
    // `PATH/` names a folder, never an asset.
    if !path.ends_with('/') && st.stat(path).is_err() {
        let pointer = format!("{}{}", path.trim_end_matches('/'), assets::pointer::SUFFIX);
        if st.stat(&pointer).is_ok_and(|s| s.kind == "file") {
            return pointer;
        }
    }
    path.to_string()
}

/// Where the pointer `from` goes when moved to `to`: the pointer of the asset path `to` names, so a
/// pointer stays a pointer. A folder, or a name without an asset's name, is refused.
fn pointer_destination(st: &mut dyn Store, from: &str, to: &str) -> Result<String> {
    use assets::pointer::{asset_path, SUFFIX};
    let asset = asset_path(to);
    let name = asset.rsplit('/').next().unwrap_or(asset);
    if to.ends_with('/') || name.is_empty() || st.stat(asset).is_ok_and(|s| s.kind != "file") {
        let own = asset_path(from).rsplit('/').next().unwrap_or("");
        return Err(StoreError::invalid(format!("{to}: name the asset's new path, as in {}/{own}", asset.trim_end_matches('/'))));
    }
    let pointer = format!("{asset}{SUFFIX}");
    // A pointer renamed in letter case only does not stand in its own way.
    if exists_any_case(st, asset) || (pointer.to_lowercase() != from.to_lowercase() && exists_any_case(st, &pointer)) {
        return Err(StoreError::invalid(format!("{asset} exists already (in some letter case): an asset cannot take another file's name")));
    }
    Ok(pointer)
}

/// `to` for a document or folder, unless that is an asset's path (its pointer exists, in any
/// letter case): the document would stand where the asset's file goes.
fn document_destination(st: &mut dyn Store, to: &str) -> Result<String> {
    use assets::pointer::{is_asset_pointer, SUFFIX};
    let bare = to.trim_end_matches('/');
    if !bare.is_empty() && !is_asset_pointer(bare) && exists_any_case(st, &format!("{bare}{SUFFIX}")) {
        return Err(StoreError::invalid(format!("{bare} is an asset's path ({bare}{SUFFIX} exists): choose another name")));
    }
    Ok(to.to_string())
}

/// Whether the store has `path`, in any letter case along it (folders that differ only in case,
/// such as `/Img` and `/img`, are all looked in).
fn exists_any_case(st: &mut dyn Store, path: &str) -> bool {
    if st.stat(path).is_ok() {
        return true;
    }
    let segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(str::to_lowercase).collect();
    let mut at = vec!["/".to_string()];
    for seg in &segs {
        let mut next = Vec::new();
        for folder in &at {
            let Ok(entries) = st.ls(folder, false) else { continue };
            next.extend(entries.into_iter().filter(|e| e.name.to_lowercase() == *seg).map(|e| e.path));
        }
        if next.is_empty() {
            return false;
        }
        at = next;
    }
    !segs.is_empty()
}

/// Directories synced with the store folder `path` that hold files textdb does not track, which
/// a move or delete in the store leaves where they are until the next sync: `(dir, files, kinds)`.
fn untracked_on_disk(st: &mut dyn Store, path: &str) -> Vec<(String, usize, String)> {
    let Ok(path) = normalize_path(path) else { return Vec::new() };
    let Ok(heads) = st.file_heads(&path) else { return Vec::new() };
    if path == "/" {
        return Vec::new();
    }
    let tracked: std::collections::HashSet<String> = heads.into_iter().map(|h| h.path).collect();
    let mut prefixes = vec!["/".to_string()];
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(seg);
        prefixes.push(acc.clone());
    }
    let mut found = Vec::new();
    for p in prefixes {
        let Ok(bases) = st.sync_bases(&p) else { continue };
        let rel = path[if p == "/" { 1 } else { p.len() }..].trim_start_matches('/');
        for base in bases {
            let root = Path::new(&base.dir).join(rel);
            if !root.is_dir() {
                continue;
            }
            let mut files = Vec::new();
            let mut stack = vec![(root.clone(), String::new())];
            while let Some((dir, rel_dir)) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else { continue };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let r = if rel_dir.is_empty() { name.clone() } else { format!("{rel_dir}/{name}") };
                    match entry.file_type() {
                        Ok(t) if t.is_dir() => {
                            if !sync::SKIP_DIRS.contains(&name.as_str()) {
                                stack.push((entry.path(), r));
                            }
                        }
                        Ok(t) if t.is_file() => {
                            if !tracked.contains(&format!("{path}/{r}")) {
                                files.push(r);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !files.is_empty() {
                found.push((root.display().to_string(), files.len(), sync::kinds(files.iter().map(String::as_str))));
            }
        }
    }
    found
}

fn untracked_json(untracked: &[(String, usize, String)]) -> Vec<serde_json::Value> {
    untracked.iter().map(|(dir, files, kinds)| json!({ "dir": dir, "files": files, "kinds": kinds })).collect()
}

/// Delete the folders a move or delete of `path` left empty, from its parent upwards; never the
/// root. Returns them, deepest first.
fn prune_empty_folders(st: &mut dyn Store, path: &str, author: Option<&str>) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    let mut folder = parent_of(&normalize_path(path)?).to_string();
    while folder != "/" {
        match st.ls(&folder, false) {
            Ok(entries) if entries.is_empty() => {
                st.rm(&folder, author, Some("remove a folder left empty"))?;
                removed.push(folder.clone());
            }
            _ => break,
        }
        folder = parent_of(&folder).to_string();
    }
    Ok(removed)
}

#[derive(Default)]
struct TreeNode {
    entry: Option<Entry>,
    children: BTreeMap<String, TreeNode>,
    files: usize,
    folders: usize,
    bytes: i64,
}

impl TreeNode {
    fn is_folder(&self) -> bool {
        self.entry.as_ref().is_none_or(|e| e.kind != "file")
    }
}

fn tree(st: &mut dyn Store, path: &str, depth: Option<usize>, dirs_only: bool, json: bool) -> Result<()> {
    let root = normalize_path(path)?;
    let base_len = if root == "/" { 0 } else { root.len() };
    // `tree FILE` shows the one entry, as `ls FILE` does. `nodes()` answers about a subtree and
    // gives nothing for a file, which used to leave a header reading `(0 files, 0 B)` and no rows.
    let one = st.stat(&root).ok().filter(|e| e.kind == "file");
    let file = one.is_some();
    let mut entries = match one {
        Some(e) => vec![e],
        None => st.nodes(&root)?,
    };
    if json {
        entries.retain(|e| {
            let rel = e.path[base_len.min(e.path.len())..].trim_start_matches('/');
            let level = if rel.is_empty() { 1 } else { rel.split('/').count() };
            depth.is_none_or(|d| level <= d) && (!dirs_only || e.kind == "folder")
        });
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        return emit_json(&entries);
    }
    // The text tree is built from everything under `root`, so a folder's counts cover its
    // whole subtree however little of it `--depth` lets through.
    let mut top = TreeNode::default();
    for e in entries {
        let rel = e.path[base_len.min(e.path.len())..].trim_start_matches('/').to_string();
        if rel.is_empty() {
            // `path` is itself a file.
            top.children.insert(e.name.clone(), TreeNode { entry: Some(e), ..Default::default() });
            continue;
        }
        let is_file = e.kind == "file";
        let size = if is_file { e.nbytes } else { 0 };
        let mut node = &mut top;
        // Each step counts the entry against the folder it is *in*, so every ancestor of a
        // node holds the totals for its whole subtree and the node itself does not count itself.
        for seg in rel.split('/') {
            if is_file {
                node.files += 1;
                node.bytes += size;
            } else {
                node.folders += 1;
            }
            node = node.children.entry(seg.to_string()).or_default();
        }
        node.entry = Some(e);
    }
    let mut s = if file { String::new() } else { format!("{root}  ({})\n", contains(top.files, top.folders, top.bytes)) };
    render_tree(&top, "", depth, dirs_only, &mut s);
    out(s.as_bytes())
}

/// What a folder holds, all the way down: `2 files, 1 folder, 394 B`.
fn contains(files: usize, folders: usize, bytes: i64) -> String {
    format!("{}, {}, {}", plural(files, "file"), plural(folders, "folder"), human_bytes(bytes))
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

/// Draw `node`'s children, and their children down to `depth` more levels (all when `None`).
fn render_tree(node: &TreeNode, indent: &str, depth: Option<usize>, dirs_only: bool, s: &mut String) {
    if depth == Some(0) {
        return;
    }
    let mut kids: Vec<(&String, &TreeNode)> = node.children.iter().filter(|(_, n)| !dirs_only || n.is_folder()).collect();
    kids.sort_by_key(|(name, n)| (!n.is_folder(), name.to_lowercase()));
    for (i, (name, kid)) in kids.iter().enumerate() {
        let last = i + 1 == kids.len();
        let branch = if last { "└── " } else { "├── " };
        if kid.is_folder() {
            s.push_str(&format!("{indent}{branch}{name}/  ({})\n", contains(kid.files, kid.folders, kid.bytes)));
            let indent = format!("{indent}{}", if last { "    " } else { "│   " });
            render_tree(kid, &indent, depth.map(|d| d - 1), dirs_only, s);
        } else {
            let size = kid.entry.as_ref().map_or(0, |e| e.nbytes);
            s.push_str(&format!("{indent}{branch}{name}  {}\n", human_bytes(size)));
        }
    }
}

fn watch(st: &mut dyn Store, since: Option<i64>, prefix: &str, json: bool) -> Result<()> {
    let prefix = normalize_path(prefix)?;
    // Start listening before reading the position, so nothing committed in between is missed.
    st.wait(Duration::ZERO)?;
    let mut since = match since {
        Some(s) => s,
        None => st.last_seq()?,
    };
    let under = |p: &str| prefix == "/" || p == prefix || p.starts_with(&format!("{prefix}/"));
    const PAGE: i64 = 1000;
    loop {
        let changes = st.feed(since, PAGE)?;
        for c in &changes {
            since = c.seq;
            if under(&c.path) || c.old_path.as_deref().is_some_and(&under) {
                if json {
                    emit_json(c)?;
                } else {
                    line(change_line(c))?;
                }
            }
        }
        if (changes.len() as i64) < PAGE {
            st.wait(Duration::from_secs(5))?;
        }
    }
}

fn show_config(cli: &Cli, matches: &ArgMatches) -> Result<()> {
    let source = |id: &str| match matches.value_source(id) {
        Some(ValueSource::CommandLine) => "flag",
        Some(ValueSource::EnvVariable) => "environment",
        Some(ValueSource::DefaultValue) => "default",
        _ => "unknown",
    };
    let backend = match config::parse_store(&cli.store) {
        StoreUrl::Sqlite(path) => format!("sqlite file {path}"),
        StoreUrl::Postgres(_) => "postgres".to_string(),
    };
    let store = config::redact(&cli.store);
    let (path_history, path_history_source) = match cli.path_history {
        Some(on) => (if on { "on" } else { "off" }, source("path_history")),
        None => ("the store's path_history setting (on unless turned off)", "default"),
    };
    if cli.json {
        return emit_json(&json!({
            "store": { "value": store, "source": source("store"), "backend": backend },
            "author": { "value": cli.author, "source": source("author") },
            "path_history": { "value": cli.path_history, "source": path_history_source },
        }));
    }
    line(format!(
        "store         {store}  [{}] -> {backend}\nauthor        {}  [{}]\npath history  {path_history}  [{path_history_source}]",
        source("store"),
        cli.author,
        source("author")
    ))
}
