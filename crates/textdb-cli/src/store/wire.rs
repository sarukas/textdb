//! A store that counts what crosses the seam.
//!
//! `TEXTDB_WIRE_STATS=<path>` (or `-` for stderr) wraps whichever store the CLI opens in this
//! counter, which records every `Store` call with the bytes it sent (`up`) and got back
//! (`down`), and writes the totals as JSON when the store is dropped. It is the instrument ADR
//! 0008 measures the sync algorithm with: the numbers it reports on a store on localhost are
//! the bytes a remote store would move, because the `Store` trait is the seam a remote store
//! sits behind.
//!
//! Sizes are the payload as a codec-neutral encoding would carry it: strings and byte slices at
//! their length, integers at eight bytes, rows at their JSON length. That is not any one wire
//! format, and it is not meant to be; the point is that a whole file counts as a whole file and
//! a hunk as a hunk.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

use super::*;

#[derive(Default, Serialize, Clone, Copy)]
pub struct Counter {
    pub calls: u64,
    pub up: u64,
    pub down: u64,
}

#[derive(Default, Serialize)]
pub struct Stats {
    pub calls: u64,
    pub up: u64,
    pub down: u64,
    pub total: u64,
    pub methods: BTreeMap<&'static str, Counter>,
}

pub struct Counting {
    inner: Box<dyn Store>,
    stats: Stats,
    /// `-` for stderr, else a file written on drop.
    sink: String,
}

impl Counting {
    pub fn wrap(inner: Box<dyn Store>, sink: String) -> Self {
        Counting { inner, stats: Stats::default(), sink }
    }

    fn record(&mut self, method: &'static str, up: usize, down: usize) {
        let c = self.stats.methods.entry(method).or_default();
        c.calls += 1;
        c.up += up as u64;
        c.down += down as u64;
        self.stats.calls += 1;
        self.stats.up += up as u64;
        self.stats.down += down as u64;
        self.stats.total = self.stats.up + self.stats.down;
        // A file sink is rewritten after every call: the CLI leaves through `process::exit` on
        // some paths (a sync with conflicts, say), and nothing is dropped on the way out.
        if !self.to_stderr() {
            self.flush();
        }
    }

    fn to_stderr(&self) -> bool {
        self.sink == "-" || self.sink == "1"
    }

    fn flush(&self) {
        let json = serde_json::to_string_pretty(&self.stats).unwrap_or_default();
        if self.to_stderr() {
            eprintln!("{json}");
        } else if let Err(e) = std::fs::write(&self.sink, json) {
            eprintln!("textdb: could not write wire stats to {}: {e}", self.sink);
        }
    }
}

impl Drop for Counting {
    fn drop(&mut self) {
        if self.to_stderr() {
            self.flush();
        }
    }
}

// ------------------------------------------------------------------ sizing helpers

fn s(x: &str) -> usize {
    x.len()
}
fn o(x: Option<&str>) -> usize {
    x.map_or(0, str::len)
}
const N: usize = 8;
fn j<T: Serialize>(x: &T) -> usize {
    serde_json::to_vec(x).map_or(0, |v| v.len())
}
fn head(h: &FileHead) -> usize {
    h.path.len() + N + h.updated_by.as_deref().map_or(0, str::len)
}
fn base_file(b: &BaseFile) -> usize {
    b.rel.len() + N + b.blob.len() + N + N + 1
}
fn err(e: &StoreError) -> usize {
    e.code.len() + e.message.len() + e.conflict.as_ref().map_or(0, j)
}

/// Forward a call, sizing what went up before it and what came down after it.
macro_rules! fwd {
    ($self:ident, $name:literal, up = $up:expr, call = $call:expr, down = $down:expr) => {{
        let up: usize = $up;
        let r = $call;
        let down = match &r {
            Ok(v) => ($down)(v),
            Err(e) => err(e),
        };
        $self.record($name, up, down);
        r
    }};
}

fn unit(_: &()) -> usize {
    0
}
fn rows<T: Serialize>(v: &Vec<T>) -> usize {
    j(v)
}

impl Store for Counting {
    fn authenticate(&mut self, bearer: &str) -> Result<()> {
        fwd!(self, "authenticate", up = s(bearer), call = self.inner.authenticate(bearer), down = unit)
    }
    fn whoami(&mut self) -> Result<Whoami> {
        fwd!(self, "whoami", up = 0, call = self.inner.whoami(), down = j)
    }
    fn account_create(&mut self, name: &str, kind: &str, root: Option<&str>) -> Result<()> {
        fwd!(self, "account_create", up = s(name) + s(kind) + o(root), call = self.inner.account_create(name, kind, root), down = unit)
    }
    fn account_ls(&mut self) -> Result<Vec<AccountRow>> {
        fwd!(self, "account_ls", up = 0, call = self.inner.account_ls(), down = rows)
    }
    fn account_disable(&mut self, name: &str, disabled: bool) -> Result<()> {
        fwd!(self, "account_disable", up = s(name) + 1, call = self.inner.account_disable(name, disabled), down = unit)
    }
    fn account_convert(&mut self, name: &str, alias: Option<&str>) -> Result<String> {
        fwd!(self, "account_convert", up = s(name) + o(alias), call = self.inner.account_convert(name, alias), down = |v: &String| v.len())
    }
    fn token_create(&mut self, account: &str, label: Option<&str>, expires_at: Option<&str>) -> Result<(String, i64)> {
        fwd!(self, "token_create", up = s(account) + o(label) + o(expires_at), call = self.inner.token_create(account, label, expires_at), down = |v: &(String, i64)| v.0.len() + N)
    }
    fn token_ls(&mut self, account: Option<&str>) -> Result<Vec<TokenRow>> {
        fwd!(self, "token_ls", up = o(account), call = self.inner.token_ls(account), down = rows)
    }
    fn token_revoke(&mut self, id: i64) -> Result<()> {
        fwd!(self, "token_revoke", up = N, call = self.inner.token_revoke(id), down = unit)
    }
    fn access_grant(&mut self, account: &str, path: &str, rights: &str, alias: Option<&str>) -> Result<ShareRow> {
        fwd!(self, "access_grant", up = s(account) + s(path) + s(rights) + o(alias), call = self.inner.access_grant(account, path, rights, alias), down = j)
    }
    fn access_rename(&mut self, account: &str, from: &str, to: &str) -> Result<()> {
        fwd!(self, "access_rename", up = s(account) + s(from) + s(to), call = self.inner.access_rename(account, from, to), down = unit)
    }
    fn access_revoke(&mut self, account: &str, alias: &str) -> Result<()> {
        fwd!(self, "access_revoke", up = s(account) + s(alias), call = self.inner.access_revoke(account, alias), down = unit)
    }
    fn access_ls(&mut self, who: Option<&str>) -> Result<Vec<ShareRow>> {
        fwd!(self, "access_ls", up = o(who), call = self.inner.access_ls(who), down = rows)
    }
    fn share_state(&mut self) -> Result<Vec<(String, String)>> {
        fwd!(self, "share_state", up = 0, call = self.inner.share_state(), down = rows)
    }
    fn backend(&self) -> &'static str {
        self.inner.backend()
    }
    fn init(&mut self) -> Result<()> {
        fwd!(self, "init", up = 0, call = self.inner.init(), down = unit)
    }
    fn trash(&mut self, parent: Option<i64>) -> Result<Vec<TrashRow>> {
        fwd!(self, "trash", up = N, call = self.inner.trash(parent), down = rows)
    }
    fn trash_restore(&mut self, id: i64, author: Option<&str>) -> Result<TrashRow> {
        fwd!(self, "trash_restore", up = N + o(author), call = self.inner.trash_restore(id, author), down = j)
    }
    fn nodes(&mut self, prefix: &str) -> Result<Vec<Entry>> {
        fwd!(self, "nodes", up = s(prefix), call = self.inner.nodes(prefix), down = rows)
    }
    fn ls(&mut self, path: &str, recursive: bool) -> Result<Vec<Entry>> {
        fwd!(self, "ls", up = s(path) + 1, call = self.inner.ls(path, recursive), down = rows)
    }
    fn stat(&mut self, path: &str) -> Result<Entry> {
        fwd!(self, "stat", up = s(path), call = self.inner.stat(path), down = j)
    }
    fn read(&mut self, path: &str, version: Option<i64>) -> Result<(Vec<u8>, i64)> {
        fwd!(self, "read", up = s(path) + N, call = self.inner.read(path, version), down = |v: &(Vec<u8>, i64)| v.0.len() + N)
    }
    fn section(&mut self, path: &str, heading: &str) -> Result<Option<Vec<u8>>> {
        fwd!(self, "section", up = s(path) + s(heading), call = self.inner.section(path, heading), down = |v: &Option<Vec<u8>>| v.as_ref().map_or(0, Vec::len))
    }
    fn search(&mut self, query: &str, prefix: &str, limit: i64, per_file: i64) -> Result<Vec<Hit>> {
        fwd!(self, "search", up = s(query) + s(prefix) + N + N, call = self.inner.search(query, prefix, limit, per_file), down = rows)
    }
    fn sections_of(&mut self, path: &str) -> Result<Vec<(i64, i64, String)>> {
        fwd!(self, "sections_of", up = s(path), call = self.inner.sections_of(path), down = rows)
    }
    fn property_keys(&mut self, prefix: &str, limit: i64) -> Result<Vec<PropKey>> {
        fwd!(self, "property_keys", up = s(prefix) + N, call = self.inner.property_keys(prefix, limit), down = rows)
    }
    fn property_values(&mut self, key: &str, prefix: &str, limit: i64) -> Result<Vec<PropValue>> {
        fwd!(self, "property_values", up = s(key) + s(prefix) + N, call = self.inner.property_values(key, prefix, limit), down = rows)
    }
    fn property_find(&mut self, query: &str, folder: &str, limit: i64) -> Result<Vec<PropHit>> {
        fwd!(self, "property_find", up = s(query) + s(folder) + N, call = self.inner.property_find(query, folder, limit), down = rows)
    }
    fn outline(&mut self, prefix: &str, heading: Option<&str>, mode: &str, max_level: Option<i64>, limit: i64) -> Result<Vec<OutlineRow>> {
        fwd!(self, "outline", up = s(prefix) + o(heading) + s(mode) + N + N, call = self.inner.outline(prefix, heading, mode, max_level, limit), down = rows)
    }
    fn heading_names(&mut self, prefix: &str, starts: &str, limit: i64) -> Result<Vec<HeadingName>> {
        fwd!(self, "heading_names", up = s(prefix) + s(starts) + N, call = self.inner.heading_names(prefix, starts, limit), down = rows)
    }
    fn mkdir(&mut self, path: &str) -> Result<()> {
        fwd!(self, "mkdir", up = s(path), call = self.inner.mkdir(path), down = unit)
    }
    fn settle(&mut self) -> Result<()> {
        fwd!(self, "settle", up = 0, call = self.inner.settle(), down = unit)
    }
    fn write(&mut self, path: &str, content: &[u8], base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<Written> {
        fwd!(self, "write", up = s(path) + content.len() + N + o(author) + o(message), call = self.inner.write(path, content, base_version, author, message), down = j)
    }
    fn edit(&mut self, path: &str, old: &[u8], new: &[u8], author: Option<&str>, message: Option<&str>) -> Result<Written> {
        fwd!(self, "edit", up = s(path) + old.len() + new.len() + o(author) + o(message), call = self.inner.edit(path, old, new, author, message), down = j)
    }
    fn append(&mut self, path: &str, tail: &[u8], author: Option<&str>, message: Option<&str>) -> Result<Written> {
        fwd!(self, "append", up = s(path) + tail.len() + o(author) + o(message), call = self.inner.append(path, tail, author, message), down = j)
    }
    fn replace_lines(&mut self, path: &str, from: i64, to: i64, text: &[u8], base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<Written> {
        fwd!(self, "replace_lines", up = s(path) + N + N + text.len() + N + o(author) + o(message), call = self.inner.replace_lines(path, from, to, text, base_version, author, message), down = j)
    }
    fn replace_ranges(&mut self, path: &str, ranges: &[LineRange], base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<Written> {
        let body: usize = ranges.iter().map(|r| N + N + r.text.len()).sum();
        fwd!(self, "replace_ranges", up = s(path) + body + N + o(author) + o(message), call = self.inner.replace_ranges(path, ranges, base_version, author, message), down = j)
    }
    fn links(&mut self, path: &str, statuses: &[&str]) -> Result<Vec<LinkRow>> {
        fwd!(self, "links", up = s(path) + statuses.iter().map(|s| s.len()).sum::<usize>(), call = self.inner.links(path, statuses), down = rows)
    }
    fn backlinks(&mut self, path: &str) -> Result<Vec<LinkRow>> {
        fwd!(self, "backlinks", up = s(path), call = self.inner.backlinks(path), down = rows)
    }
    fn history(&mut self, path: &str) -> Result<Vec<Commit>> {
        fwd!(self, "history", up = s(path), call = self.inner.history(path), down = rows)
    }
    fn diff(&mut self, path: &str, v1: i64, v2: i64) -> Result<String> {
        fwd!(self, "diff", up = s(path) + N + N, call = self.inner.diff(path, v1, v2), down = |v: &String| v.len())
    }
    fn hunks(&mut self, path: &str, v1: i64, v2: i64) -> Result<Vec<Hunk>> {
        fwd!(self, "hunks", up = s(path) + N + N, call = self.inner.hunks(path, v1, v2), down = |v: &Vec<Hunk>| v.iter().map(|h| 4 * N + h.old_text.len() + h.new_text.len()).sum())
    }
    fn chunks(&mut self, path: &str, version: Option<i64>) -> Result<Vec<Chunk>> {
        fwd!(self, "chunks", up = s(path) + N, call = self.inner.chunks(path, version), down = rows)
    }
    fn mv(&mut self, from: &str, to: &str, author: Option<&str>, message: Option<&str>) -> Result<()> {
        fwd!(self, "mv", up = s(from) + s(to) + o(author) + o(message), call = self.inner.mv(from, to, author, message), down = unit)
    }
    fn mv_links(&mut self, from: &str, to: &str, author: Option<&str>, message: Option<&str>, update: Option<bool>) -> Result<Vec<MovedLink>> {
        fwd!(self, "mv_links", up = s(from) + s(to) + o(author) + o(message) + 1, call = self.inner.mv_links(from, to, author, message, update), down = rows)
    }
    fn rm(&mut self, path: &str, author: Option<&str>, message: Option<&str>) -> Result<()> {
        fwd!(self, "rm", up = s(path) + o(author) + o(message), call = self.inner.rm(path, author, message), down = unit)
    }
    fn path_history(&mut self, path: &str) -> Result<Vec<PathEvent>> {
        fwd!(self, "path_history", up = s(path), call = self.inner.path_history(path), down = rows)
    }
    fn set_session_path_history(&mut self, on: Option<bool>) -> Result<()> {
        fwd!(self, "set_session_path_history", up = 1, call = self.inner.set_session_path_history(on), down = unit)
    }
    fn path_history_enabled(&mut self) -> Result<bool> {
        fwd!(self, "path_history_enabled", up = 0, call = self.inner.path_history_enabled(), down = |_: &bool| 1)
    }
    fn setting(&mut self, key: &str) -> Result<Option<String>> {
        fwd!(self, "setting", up = s(key), call = self.inner.setting(key), down = |v: &Option<String>| v.as_deref().map_or(0, str::len))
    }
    fn set_setting(&mut self, key: &str, value: Option<&str>) -> Result<Option<String>> {
        fwd!(self, "set_setting", up = s(key) + o(value), call = self.inner.set_setting(key, value), down = |v: &Option<String>| v.as_deref().map_or(0, str::len))
    }
    fn last_seq(&mut self) -> Result<i64> {
        fwd!(self, "last_seq", up = 0, call = self.inner.last_seq(), down = |_: &i64| N)
    }
    fn feed(&mut self, since: i64, limit: i64) -> Result<Vec<Change>> {
        fwd!(self, "feed", up = N + N, call = self.inner.feed(since, limit), down = rows)
    }
    fn wait(&mut self, timeout: Duration) -> Result<()> {
        fwd!(self, "wait", up = N, call = self.inner.wait(timeout), down = unit)
    }
    fn import(
        &mut self,
        files: &mut dyn Iterator<Item = (String, Vec<u8>)>,
        author: Option<&str>,
        batch: usize,
        progress: &mut dyn FnMut(&ImportStats),
        on_error: &mut dyn FnMut(&str, &StoreError),
    ) -> Result<ImportStats> {
        let up = Cell::new(o(author));
        let mut counted = files.map(|(p, b)| {
            up.set(up.get() + p.len() + b.len());
            (p, b)
        });
        let r = self.inner.import(&mut counted, author, batch, progress, on_error);
        let down = match &r {
            Ok(v) => j(v),
            Err(e) => err(e),
        };
        self.record("import", up.get(), down);
        r
    }
    fn file_heads(&mut self, prefix: &str) -> Result<Vec<FileHead>> {
        fwd!(self, "file_heads", up = s(prefix), call = self.inner.file_heads(prefix), down = |v: &Vec<FileHead>| v.iter().map(head).sum())
    }
    fn sync_bases(&mut self, prefix: &str) -> Result<Vec<SyncBase>> {
        fwd!(self, "sync_bases", up = s(prefix), call = self.inner.sync_bases(prefix), down = rows)
    }
    fn sync_base(&mut self, prefix: &str, dir: &str) -> Result<Option<SyncBase>> {
        fwd!(self, "sync_base", up = s(prefix) + s(dir), call = self.inner.sync_base(prefix, dir), down = |v: &Option<SyncBase>| v.as_ref().map_or(0, |b| j(b) + b.files.iter().map(base_file).sum::<usize>()))
    }
    fn save_sync_base(&mut self, base: &SyncBase) -> Result<()> {
        fwd!(self, "save_sync_base", up = j(base) + base.files.iter().map(base_file).sum::<usize>(), call = self.inner.save_sync_base(base), down = unit)
    }
    fn put_sync_files(&mut self, prefix: &str, dir: &str, files: &[BaseFile]) -> Result<bool> {
        fwd!(self, "put_sync_files", up = s(prefix) + s(dir) + files.iter().map(base_file).sum::<usize>(), call = self.inner.put_sync_files(prefix, dir, files), down = |_: &bool| 1)
    }
    fn sync_head(&mut self, prefix: &str, dir: &str) -> Result<Option<SyncBase>> {
        fwd!(self, "sync_head", up = s(prefix) + s(dir), call = self.inner.sync_head(prefix, dir), down = |v: &Option<SyncBase>| v.as_ref().map_or(0, j))
    }
    fn file_heads_delta(&mut self, prefix: &str, dir: &str) -> Result<Option<HeadsDelta>> {
        fwd!(self, "file_heads_delta", up = s(prefix) + s(dir), call = self.inner.file_heads_delta(prefix, dir), down = |v: &Option<HeadsDelta>| {
            v.as_ref().map_or(0, |d| d.changed.iter().map(head).sum::<usize>() + d.gone.iter().map(String::len).sum::<usize>())
        })
    }
    fn save_sync_base_delta(&mut self, base: &SyncBase, upsert: &[BaseFile], remove: &[String]) -> Result<()> {
        fwd!(self, "save_sync_base_delta", up = j(base) + upsert.iter().map(base_file).sum::<usize>() + remove.iter().map(String::len).sum::<usize>(), call = self.inner.save_sync_base_delta(base, upsert, remove), down = unit)
    }
    fn rename_sync_dir(&mut self, prefix: &str, from: &str, to: &str) -> Result<()> {
        fwd!(self, "rename_sync_dir", up = s(prefix) + s(from) + s(to), call = self.inner.rename_sync_dir(prefix, from, to), down = unit)
    }
    fn sql(&mut self, query: &str, params: &[String], author: Option<&str>, write: bool, dry_run: bool) -> Result<SqlResult> {
        fwd!(self, "sql", up = s(query) + params.iter().map(String::len).sum::<usize>() + o(author) + 2, call = self.inner.sql(query, params, author, write, dry_run), down = |v: &SqlResult| j(&v.columns) + j(&v.rows) + N + v.batch.as_deref().map_or(0, str::len))
    }
    fn revert_batch(&mut self, batch: &str, author: Option<&str>, skip_changed: bool, dry_run: bool) -> Result<RevertOutcome> {
        fwd!(self, "revert_batch", up = s(batch) + o(author) + 2, call = self.inner.revert_batch(batch, author, skip_changed, dry_run), down = j)
    }
    fn all_sync_bases(&mut self) -> Result<Vec<SyncBase>> {
        fwd!(self, "all_sync_bases", up = 0, call = self.inner.all_sync_bases(), down = rows)
    }
    fn owner_paths(&mut self, paths: &[String]) -> Result<Vec<String>> {
        fwd!(self, "owner_paths", up = paths.iter().map(String::len).sum::<usize>(), call = self.inner.owner_paths(paths), down = |v: &Vec<String>| v.iter().map(String::len).sum())
    }
    fn asset_items_named(&mut self, store: &str, locations: &[String]) -> Result<Option<ItemsNamed>> {
        fwd!(self, "asset_items_named", up = s(store) + locations.iter().map(String::len).sum::<usize>(), call = self.inner.asset_items_named(store, locations), down = |v: &Option<ItemsNamed>| v.as_ref().map_or(0, |n| n.named.len() + N))
    }
    fn may_name(&mut self, location: &str) -> Result<bool> {
        fwd!(self, "may_name", up = s(location), call = self.inner.may_name(location), down = |_: &bool| 1)
    }
    fn asset_item_users(&mut self, store: &str, location: &str, own: &str) -> Result<Option<ItemUsers>> {
        fwd!(self, "asset_item_users", up = s(store) + s(location) + s(own), call = self.inner.asset_item_users(store, location, own), down = |v: &Option<ItemUsers>| v.map_or(0, |_| 2 * N))
    }
    fn asset_store_users(&mut self, store: &str) -> Result<Option<usize>> {
        fwd!(self, "asset_store_users", up = s(store), call = self.inner.asset_store_users(store), down = |v: &Option<usize>| v.map_or(0, |_| N))
    }
    fn asset_stores(&mut self) -> Result<Vec<AssetStore>> {
        fwd!(self, "asset_stores", up = 0, call = self.inner.asset_stores(), down = rows)
    }
    fn put_asset_store(&mut self, store: &AssetStore) -> Result<()> {
        fwd!(self, "put_asset_store", up = j(store), call = self.inner.put_asset_store(store), down = unit)
    }
    fn remove_asset_store(&mut self, name: &str) -> Result<bool> {
        fwd!(self, "remove_asset_store", up = s(name), call = self.inner.remove_asset_store(name), down = |_: &bool| 1)
    }
}
