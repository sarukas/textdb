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
use textdb_sqlite::normalize_path;

use config::StoreUrl;
use store::{Change, Commit, Entry, ImportStats, PathEvent, Store, StoreError, Written};

type Result<T> = std::result::Result<T, StoreError>;

mod git;
mod portable;
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
    /// Run one SQL statement against the store and print its rows. Besides `kb` and the textdb
    /// functions, the views files, folders, frontmatter, sections, links, commits and authors
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
    },
    /// List one folder.
    Ls {
        #[arg(default_value = "/", value_parser = store_path)]
        path: String,
        /// A table: size, lines, words, versions, last update, and a file's authors or a
        /// folder's contents. A folder's figures are totals over everything below it.
        #[arg(long, short = 'l')]
        long: bool,
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
    /// Full-text search: terms are ANDed per document, "quoted phrases", prefix*.
    Search {
        query: String,
        #[arg(long, short = 'p', default_value = "/", value_parser = store_path)]
        prefix: String,
        #[arg(long, default_value_t = 50)]
        limit: i64,
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
    },
    /// Replace lines FROM..TO (1-based, inclusive) with --text, --file or stdin. TO = FROM-1 inserts before FROM.
    ReplaceLines {
        #[arg(value_parser = store_path)]
        path: String,
        from: i64,
        to: i64,
        /// The version the line numbers refer to (see `cat -n`).
        #[arg(long, short = 'b')]
        base_version: Option<i64>,
        #[arg(long, short = 't', allow_hyphen_values = true, conflicts_with = "file")]
        text: Option<String>,
        #[arg(long, short = 'f')]
        file: Option<PathBuf>,
    },
    /// Append text (the argument, or stdin) to the end of a file; never conflicts.
    Append {
        #[arg(value_parser = store_path)]
        path: String,
        #[arg(allow_hyphen_values = true)]
        text: Option<String>,
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
    /// Move or rename a file or folder.
    Mv {
        #[arg(value_parser = store_path)]
        from: String,
        #[arg(value_parser = store_path)]
        to: String,
    },
    /// Delete a file or folder; its history stays readable.
    Rm {
        #[arg(value_parser = store_path)]
        path: String,
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
    std::process::exit(status);
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
            },
            json,
        ),
        Cmd::Sql {
            query,
            params,
            write,
            full,
        } => {
            let statement = match query.as_deref() {
                None | Some("-") => {
                    let mut text = String::new();
                    std::io::stdin().read_to_string(&mut text)?;
                    text
                }
                Some(text) => text.to_string(),
            };
            sql_query::run(st, &statement, &params, author, write, full, json)
        }
        Cmd::GitStatus { prefix, dir, rev, ext } => sync::git_status(st, &prefix, &dir, &rev, &sync::parse_exts(&ext), json),
        Cmd::Ls {
            path,
            long,
            sort,
            reverse,
            recursive,
        } => {
            let mut entries = st.ls(&path, recursive)?;
            sort_entries(&mut entries, sort, reverse, recursive);
            if json {
                return emit_json(&entries);
            }
            out(ls_text(&entries, long, recursive).as_bytes())
        }
        Cmd::Tree { path, depth, dirs } => tree(st, &path, depth, dirs, json),
        Cmd::Stat { path } => {
            let s = st.stat(&path)?;
            if json {
                return emit_json(&s);
            }
            let mut text = format!("{} {} v{}", s.path, s.kind, s.version);
            if s.kind == "file" {
                text.push_str(&format!(
                    " · {} · {} lines",
                    human_bytes(s.nbytes.unwrap_or(0)),
                    s.nlines.unwrap_or(0)
                ));
            }
            if let Some(at) = &s.updated_at {
                text.push_str(&format!(" · updated {at}"));
            }
            if let Some(by) = &s.updated_by {
                text.push_str(&format!(" by {by}"));
            }
            line(text)
        }
        Cmd::Cat { path, number, lines, at, section } => cat(st, &path, number, lines.as_deref(), at, section.as_deref(), json),
        Cmd::Search { query, prefix, limit } => {
            let hits = st.search(&query, &prefix, limit)?;
            if json {
                return emit_json(&hits);
            }
            let s: String = hits.iter().map(|h| format!("{}:{}: {}\n", h.path, h.line, h.snippet)).collect();
            out(s.as_bytes())
        }
        Cmd::Write {
            path,
            base_version,
            file,
            message,
            allow_empty,
        } => {
            let content = input(file.as_deref(), "write")?;
            if content.is_empty() && !allow_empty {
                return Err(StoreError::invalid(
                    "refusing to write empty content (nothing on stdin?); pass --allow-empty to mean it",
                ));
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
            let w = st.edit(&path, &old, &new, author)?;
            emit_written(&path, &w, json)
        }
        Cmd::ReplaceLines {
            path,
            from,
            to,
            base_version,
            text,
            file,
        } => {
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
            let w = st.replace_lines(&path, from, to, &body, base_version, author)?;
            emit_written(&path, &w, json)
        }
        Cmd::Append { path, text } => {
            let tail = match text {
                // An argument is a line of text; stdin is taken as it comes.
                Some(t) if t.ends_with('\n') => t.into_bytes(),
                Some(t) => format!("{t}\n").into_bytes(),
                None => input(None, "append")?,
            };
            let w = st.append(&path, &tail, author)?;
            emit_written(&path, &w, json)
        }
        Cmd::History { path, versions_only } => {
            let commits = st.history(&path)?;
            if versions_only && json {
                return emit_json(&commits);
            }
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
                None => st.stat(&path)?.version,
            };
            let diff = st.diff(&path, v1, v2)?;
            if json {
                emit_json(&json!({ "path": path, "from": v1, "to": v2, "diff": diff }))
            } else {
                out(diff.as_bytes())
            }
        }
        Cmd::Hunks { path, v1, v2 } => {
            let v2 = match v2 {
                Some(v) => v,
                None => st.stat(&path)?.version,
            };
            let v1 = v1.unwrap_or(v2 - 1).max(0);
            let hunks = st.hunks(&path, v1, v2)?;
            if json {
                return emit_json(&json!({ "path": path, "from": v1, "to": v2, "hunks": hunks }));
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
        Cmd::Mv { from, to } => {
            st.mv(&from, &to, author)?;
            if json {
                emit_json(&json!({ "moved": from, "to": to }))
            } else {
                line(format!("moved {from} -> {to}"))
            }
        }
        Cmd::Rm { path } => {
            st.rm(&path, author)?;
            if json {
                emit_json(&json!({ "deleted": path }))
            } else {
                line(format!("deleted {path}"))
            }
        }
        Cmd::Setting { key, value } => {
            let key = key.unwrap_or_else(|| textdb_core::PATH_HISTORY_SETTING.to_string());
            if let Some(v) = &value {
                st.set_setting(&key, if v == "default" { None } else { Some(v.as_str()) })?;
            }
            let stored = st.setting(&key)?;
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
        emit_json(&json!({ "path": path, "version": w.version, "kind": w.kind }))
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
                    && disk_len == f.nbytes.map(|n| n as u64)
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
            let (size, lines) = if e.kind == "folder" {
                (String::new(), String::new())
            } else {
                (human_bytes(e.nbytes.unwrap_or(0)), format!("{}L", e.nlines.unwrap_or(0)))
            };
            s.push_str(&format!("{size:>9}  {lines:>7}  {}\n", name(e)));
        }
        return s;
    }
    s.push_str(&format!(
        "{:>9} {:>8} {:>9} {:>5}  {:<16}  {:<28}  {}\n",
        "SIZE", "LINES", "WORDS", "VERS", "UPDATED", "AUTHORS / CONTAINS", "NAME"
    ));
    for e in entries {
        let who = if e.kind == "folder" {
            format!("{} files, {} folders", e.files.unwrap_or(0), e.folders.unwrap_or(0))
        } else {
            let mut names: Vec<String> =
                e.authors.iter().take(2).map(|a| format!("{} ({})", a.author.as_deref().unwrap_or("-"), a.commits)).collect();
            if e.authors.len() > 2 {
                names.push(format!("+{}", e.authors.len() - 2));
            }
            names.join(", ")
        };
        let updated: String = e.updated_at.as_deref().unwrap_or("").replace('T', " ").chars().take(16).collect();
        s.push_str(&format!(
            "{:>9} {:>8} {:>9} {:>5}  {:<16}  {:<28}  {}\n",
            human_bytes(e.nbytes.unwrap_or(0)),
            e.nlines.unwrap_or(0),
            e.nwords.unwrap_or(0),
            e.versions.unwrap_or(0),
            updated,
            who,
            name(e)
        ));
    }
    s
}

fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
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
/// sorted by the relative path. Hidden directories and `node_modules` are skipped.
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
                if !name.starts_with('.') && name != "node_modules" {
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

#[derive(Default)]
struct TreeNode {
    entry: Option<Entry>,
    children: BTreeMap<String, TreeNode>,
    files: usize,
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
    let mut entries = st.nodes(&root)?;
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
        let size = if e.kind == "file" { e.nbytes.unwrap_or(0) } else { 0 };
        let is_file = e.kind == "file";
        let mut node = &mut top;
        for seg in rel.split('/') {
            if is_file {
                node.files += 1;
                node.bytes += size;
            }
            node = node.children.entry(seg.to_string()).or_default();
        }
        node.entry = Some(e);
    }
    let mut s = format!("{root}  ({}, {})\n", count_files(top.files), human_bytes(top.bytes));
    render_tree(&top, "", depth, dirs_only, &mut s);
    out(s.as_bytes())
}

fn count_files(n: usize) -> String {
    if n == 1 { "1 file".to_string() } else { format!("{n} files") }
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
            s.push_str(&format!("{indent}{branch}{name}/  ({}, {})\n", count_files(kid.files), human_bytes(kid.bytes)));
            let indent = format!("{indent}{}", if last { "    " } else { "│   " });
            render_tree(kid, &indent, depth.map(|d| d - 1), dirs_only, s);
        } else {
            let size = kid.entry.as_ref().and_then(|e| e.nbytes).unwrap_or(0);
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
