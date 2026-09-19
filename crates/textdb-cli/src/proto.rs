//! The store seam as messages (ADR 0008, step 3).
//!
//! Every `Store` method is a [`Request`] answered by a [`Response`]; both are plain `serde`
//! types with no notion of a wire. A [`Codec`] turns them into bytes and back — JSON first,
//! because it can be read with curl — and a [`Transport`] carries the bytes. [`RemoteStore`]
//! is a `Store` that turns each call into a request over a transport, and [`dispatch`] is the
//! other end: a request applied to a real store. Nothing above the `Store` trait can tell the
//! two apart, which is the point.
//!
//! [`Loopback`] is the transport with no network: it encodes the request, decodes it, applies
//! it to a store in this process, and sends the reply back the same way. It costs nothing but
//! the encoding, and `TEXTDB_TEST_LOOPBACK=1` puts it under whatever store the CLI opens, so
//! the whole test suite proves every type crosses the seam and back unchanged before any
//! carrier exists. HTTPS, WebSocket or Arrow Flight are later transports; each is an
//! implementation of [`Transport`] and nothing else changes.
//!
//! Identity is a property of the connection, as `kb.auth` makes it one on Postgres:
//! `authenticate` is a request like any other here, and a carrier that multiplexes sessions
//! will carry the token as a session attribute rather than a request. Errors stay
//! [`StoreError`] with its `TX` codes and structured detail, so a conflict reads the same over
//! any wire.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::store::{
    AccountRow, AssetStore, BaseFile, Change, Chunk, Commit, Entry, FileHead, GitState, HeadingName, HeadsDelta, Hit, Hunk, ImportStats,
    ItemUsers, ItemsNamed, LineRange, LinkRow, MovedLink, OutlineRow, PathEvent, PropHit, PropKey, PropValue, Result, RevertOutcome,
    ShareRow, SqlResult, Store, StoreError, SyncBase, TokenRow, TrashRow, Whoami, Written,
};

/// A sync base as it crosses the seam: every field, including the ones its report form skips.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncBaseWire {
    pub prefix: String,
    pub dir: String,
    pub seq: i64,
    pub synced_at: Option<String>,
    pub author: Option<String>,
    pub git: Option<GitState>,
    pub rules: Option<String>,
    pub generation: i64,
    pub dir_id: Option<String>,
    pub files: Vec<BaseFile>,
}

impl From<SyncBase> for SyncBaseWire {
    fn from(b: SyncBase) -> Self {
        SyncBaseWire {
            prefix: b.prefix,
            dir: b.dir,
            seq: b.seq,
            synced_at: b.synced_at,
            author: b.author,
            git: b.git,
            rules: b.rules,
            generation: b.generation,
            dir_id: b.dir_id,
            files: b.files,
        }
    }
}

impl From<SyncBaseWire> for SyncBase {
    fn from(b: SyncBaseWire) -> Self {
        SyncBase {
            prefix: b.prefix,
            dir: b.dir,
            seq: b.seq,
            synced_at: b.synced_at,
            author: b.author,
            git: b.git,
            rules: b.rules,
            generation: b.generation,
            dir_id: b.dir_id,
            files: b.files,
        }
    }
}

/// One `Store` call, owned: what a client sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Authenticate { bearer: String },
    Whoami,
    AccountCreate { name: String, kind: String, root: Option<String> },
    AccountLs,
    AccountDisable { name: String, disabled: bool },
    AccountConvert { name: String, alias: Option<String> },
    TokenCreate { account: String, label: Option<String>, expires_at: Option<String> },
    TokenLs { account: Option<String> },
    TokenRevoke { id: i64 },
    AccessGrant { account: String, path: String, rights: String, alias: Option<String> },
    AccessRename { account: String, from: String, to: String },
    AccessRevoke { account: String, alias: String },
    AccessLs { who: Option<String> },
    ShareState,
    Backend,
    Init,
    Trash { parent: Option<i64> },
    TrashRestore { id: i64, author: Option<String> },
    Nodes { prefix: String },
    Ls { path: String, recursive: bool },
    Stat { path: String },
    Read { path: String, version: Option<i64> },
    Section { path: String, heading: String },
    Search { query: String, prefix: String, limit: i64, per_file: i64 },
    SectionsOf { path: String },
    PropertyKeys { prefix: String, limit: i64 },
    PropertyValues { key: String, prefix: String, limit: i64 },
    PropertyFind { query: String, folder: String, limit: i64 },
    Outline { prefix: String, heading: Option<String>, mode: String, max_level: Option<i64>, limit: i64 },
    HeadingNames { prefix: String, starts: String, limit: i64 },
    Mkdir { path: String },
    Settle,
    Write { path: String, content: Vec<u8>, base_version: Option<i64>, author: Option<String>, message: Option<String> },
    Edit { path: String, old: Vec<u8>, new: Vec<u8>, author: Option<String>, message: Option<String> },
    Append { path: String, tail: Vec<u8>, author: Option<String>, message: Option<String> },
    ReplaceLines { path: String, from: i64, to: i64, text: Vec<u8>, base_version: Option<i64>, author: Option<String>, message: Option<String> },
    ReplaceRanges { path: String, ranges: Vec<LineRange>, base_version: Option<i64>, author: Option<String>, message: Option<String> },
    Links { path: String, statuses: Vec<String> },
    Backlinks { path: String },
    History { path: String },
    Diff { path: String, v1: i64, v2: i64 },
    Hunks { path: String, v1: i64, v2: i64 },
    Chunks { path: String, version: Option<i64> },
    Mv { from: String, to: String, author: Option<String>, message: Option<String> },
    MvLinks { from: String, to: String, author: Option<String>, message: Option<String>, update: Option<bool> },
    Rm { path: String, author: Option<String>, message: Option<String> },
    PathHistory { path: String },
    SetSessionPathHistory { on: Option<bool> },
    PathHistoryEnabled,
    Setting { key: String },
    SetSetting { key: String, value: Option<String> },
    LastSeq,
    Feed { since: i64, limit: i64 },
    Wait { millis: u64 },
    /// One batch of an import; the client cuts the files up and reports progress itself.
    Import { files: Vec<(String, Vec<u8>)>, author: Option<String> },
    FileHeads { prefix: String },
    SyncBases { prefix: String },
    SyncBase { prefix: String, dir: String },
    SaveSyncBase { base: SyncBaseWire },
    PutSyncFiles { prefix: String, dir: String, files: Vec<BaseFile> },
    SyncHead { prefix: String, dir: String },
    FileHeadsDelta { prefix: String, dir: String },
    SaveSyncBaseDelta { base: SyncBaseWire, upsert: Vec<BaseFile>, remove: Vec<String> },
    RenameSyncDir { prefix: String, from: String, to: String },
    Sql { query: String, params: Vec<String>, author: Option<String>, write: bool, dry_run: bool },
    RevertBatch { batch: String, author: Option<String>, skip_changed: bool, dry_run: bool },
    AllSyncBases,
    OwnerPaths { paths: Vec<String> },
    AssetItemsNamed { store: String, locations: Vec<String> },
    MayName { location: String },
    AssetItemUsers { store: String, location: String, own: String },
    AssetStoreUsers { store: String },
    AssetStores,
    PutAssetStore { store: AssetStore },
    RemoveAssetStore { name: String },
    /// Several requests in one message, answered in order. Each answer stands on its own: one
    /// failing does not stop the rest, and the caller reads each reply.
    Batch { requests: Vec<Request> },
}

/// What a request comes back with.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Response {
    Unit,
    Bool(bool),
    Int(i64),
    Text(String),
    MaybeText(Option<String>),
    Strings(Vec<String>),
    Pairs(Vec<(String, String)>),
    Token((String, i64)),
    Whoami(Whoami),
    AccountRows(Vec<AccountRow>),
    TokenRows(Vec<TokenRow>),
    ShareRow(ShareRow),
    ShareRows(Vec<ShareRow>),
    TrashRows(Vec<TrashRow>),
    TrashRow(TrashRow),
    Entries(Vec<Entry>),
    Entry(Entry),
    Read((Vec<u8>, i64)),
    MaybeBytes(Option<Vec<u8>>),
    Hits(Vec<Hit>),
    Sections(Vec<(i64, i64, String)>),
    PropKeys(Vec<PropKey>),
    PropValues(Vec<PropValue>),
    PropHits(Vec<PropHit>),
    OutlineRows(Vec<OutlineRow>),
    HeadingNames(Vec<HeadingName>),
    Written(Written),
    LinkRows(Vec<LinkRow>),
    Commits(Vec<Commit>),
    Hunks(Vec<Hunk>),
    Chunks(Vec<Chunk>),
    MovedLinks(Vec<MovedLink>),
    PathEvents(Vec<PathEvent>),
    Changes(Vec<Change>),
    Import { stats: ImportStats, failed: Vec<(String, StoreError)> },
    FileHeads(Vec<FileHead>),
    SyncBases(Vec<SyncBaseWire>),
    MaybeSyncBase(Option<SyncBaseWire>),
    HeadsDelta(Option<HeadsDelta>),
    Sql(SqlResult),
    Revert(RevertOutcome),
    ItemsNamed(Option<ItemsNamed>),
    ItemUsers(Option<ItemUsers>),
    MaybeUsize(Option<usize>),
    AssetStores(Vec<AssetStore>),
    Batch(Vec<Reply>),
}

/// A response or the store's error, as one message.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "body", rename_all = "snake_case")]
pub enum Reply {
    Ok(Response),
    Err(StoreError),
}

impl From<Result<Response>> for Reply {
    fn from(r: Result<Response>) -> Self {
        match r {
            Ok(v) => Reply::Ok(v),
            Err(e) => Reply::Err(e),
        }
    }
}

impl From<Reply> for Result<Response> {
    fn from(r: Reply) -> Self {
        match r {
            Reply::Ok(v) => Ok(v),
            Reply::Err(e) => Err(e),
        }
    }
}

// ----------------------------------------------------------------------------- codec

/// Bytes for a message and back. The types are codec-neutral, so any codec that round-trips
/// `serde` data will do; which one a carrier uses is its own business.
pub trait Codec {
    fn encode<T: Serialize>(&self, v: &T) -> Result<Vec<u8>>;
    fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T>;
}

/// JSON: readable with curl, and every type here has a JSON form already.
pub struct Json;

impl Codec for Json {
    fn encode<T: Serialize>(&self, v: &T) -> Result<Vec<u8>> {
        serde_json::to_vec(v).map_err(|e| StoreError::other(format!("encoding a message: {e}")))
    }
    fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T> {
        serde_json::from_slice(bytes).map_err(|e| StoreError::other(format!("decoding a message: {e}")))
    }
}

// ------------------------------------------------------------------------- transport

/// Carries a request to a store and its reply back. Where a carrier can send several
/// requests in one exchange, `call_many` is the place to do it; the default sends a batch.
pub trait Transport {
    fn call(&mut self, req: Request) -> Result<Response>;

    // Nothing above the trait batches yet; a carrier with a round trip to save is what will.
    #[allow(dead_code)]
    fn call_many(&mut self, requests: Vec<Request>) -> Result<Vec<Result<Response>>> {
        match self.call(Request::Batch { requests })? {
            Response::Batch(replies) => Ok(replies.into_iter().map(Into::into).collect()),
            other => Err(unexpected("batch", &other)),
        }
    }
}

/// The transport with no wire: encode, decode, apply to a store here, and back. What it
/// proves is that every message survives its codec; what it costs is only the encoding.
pub struct Loopback<C: Codec> {
    store: Box<dyn Store>,
    codec: C,
    /// Encoded bytes each way, for whoever wants to compare codecs.
    pub bytes_up: u64,
    pub bytes_down: u64,
}

impl<C: Codec> Loopback<C> {
    pub fn new(store: Box<dyn Store>, codec: C) -> Self {
        Loopback { store, codec, bytes_up: 0, bytes_down: 0 }
    }
}

impl<C: Codec> Transport for Loopback<C> {
    fn call(&mut self, req: Request) -> Result<Response> {
        let up = self.codec.encode(&req)?;
        self.bytes_up += up.len() as u64;
        let req: Request = self.codec.decode(&up)?;
        let reply: Reply = dispatch(self.store.as_mut(), req).into();
        let down = self.codec.encode(&reply)?;
        self.bytes_down += down.len() as u64;
        let reply: Reply = self.codec.decode(&down)?;
        reply.into()
    }
}

fn unexpected(wanted: &str, got: &Response) -> StoreError {
    let got = serde_json::to_value(got).map(|v| v["kind"].as_str().unwrap_or("?").to_string()).unwrap_or_default();
    StoreError::other(format!("the store answered {got} where {wanted} was expected"))
}

// ---------------------------------------------------------------------- remote store

/// A `Store` on the near side of a transport.
pub struct RemoteStore<T: Transport> {
    transport: T,
    backend: &'static str,
}

impl<T: Transport> RemoteStore<T> {
    /// Connect: one round trip to learn what engine answers, so `backend()` can say so.
    pub fn connect(mut transport: T) -> Result<Self> {
        let backend = match transport.call(Request::Backend)? {
            Response::Text(name) => match name.as_str() {
                "sqlite" => "sqlite",
                "postgres" => "postgres",
                _ => "remote",
            },
            other => return Err(unexpected("the backend name", &other)),
        };
        Ok(RemoteStore { transport, backend })
    }

    #[allow(dead_code)]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    #[allow(dead_code)]
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }
}

/// `$call` must answer with `$variant`; anything else is a protocol error.
macro_rules! answer {
    ($self:ident, $req:expr, $variant:ident) => {
        match $self.transport.call($req)? {
            Response::$variant(v) => Ok(v),
            other => Err(unexpected(stringify!($variant), &other)),
        }
    };
}

fn owned(s: Option<&str>) -> Option<String> {
    s.map(str::to_string)
}

impl<T: Transport> Store for RemoteStore<T> {
    fn authenticate(&mut self, bearer: &str) -> Result<()> {
        self.transport.call(Request::Authenticate { bearer: bearer.to_string() }).map(drop)
    }
    fn whoami(&mut self) -> Result<Whoami> {
        answer!(self, Request::Whoami, Whoami)
    }
    fn account_create(&mut self, name: &str, kind: &str, root: Option<&str>) -> Result<()> {
        self.transport.call(Request::AccountCreate { name: name.into(), kind: kind.into(), root: owned(root) }).map(drop)
    }
    fn account_ls(&mut self) -> Result<Vec<AccountRow>> {
        answer!(self, Request::AccountLs, AccountRows)
    }
    fn account_disable(&mut self, name: &str, disabled: bool) -> Result<()> {
        self.transport.call(Request::AccountDisable { name: name.into(), disabled }).map(drop)
    }
    fn account_convert(&mut self, name: &str, alias: Option<&str>) -> Result<String> {
        answer!(self, Request::AccountConvert { name: name.into(), alias: owned(alias) }, Text)
    }
    fn token_create(&mut self, account: &str, label: Option<&str>, expires_at: Option<&str>) -> Result<(String, i64)> {
        answer!(self, Request::TokenCreate { account: account.into(), label: owned(label), expires_at: owned(expires_at) }, Token)
    }
    fn token_ls(&mut self, account: Option<&str>) -> Result<Vec<TokenRow>> {
        answer!(self, Request::TokenLs { account: owned(account) }, TokenRows)
    }
    fn token_revoke(&mut self, id: i64) -> Result<()> {
        self.transport.call(Request::TokenRevoke { id }).map(drop)
    }
    fn access_grant(&mut self, account: &str, path: &str, rights: &str, alias: Option<&str>) -> Result<ShareRow> {
        answer!(self, Request::AccessGrant { account: account.into(), path: path.into(), rights: rights.into(), alias: owned(alias) }, ShareRow)
    }
    fn access_rename(&mut self, account: &str, from: &str, to: &str) -> Result<()> {
        self.transport.call(Request::AccessRename { account: account.into(), from: from.into(), to: to.into() }).map(drop)
    }
    fn access_revoke(&mut self, account: &str, alias: &str) -> Result<()> {
        self.transport.call(Request::AccessRevoke { account: account.into(), alias: alias.into() }).map(drop)
    }
    fn access_ls(&mut self, who: Option<&str>) -> Result<Vec<ShareRow>> {
        answer!(self, Request::AccessLs { who: owned(who) }, ShareRows)
    }
    fn share_state(&mut self) -> Result<Vec<(String, String)>> {
        answer!(self, Request::ShareState, Pairs)
    }
    fn backend(&self) -> &'static str {
        self.backend
    }
    fn init(&mut self) -> Result<()> {
        self.transport.call(Request::Init).map(drop)
    }
    fn trash(&mut self, parent: Option<i64>) -> Result<Vec<TrashRow>> {
        answer!(self, Request::Trash { parent }, TrashRows)
    }
    fn trash_restore(&mut self, id: i64, author: Option<&str>) -> Result<TrashRow> {
        answer!(self, Request::TrashRestore { id, author: owned(author) }, TrashRow)
    }
    fn nodes(&mut self, prefix: &str) -> Result<Vec<Entry>> {
        answer!(self, Request::Nodes { prefix: prefix.into() }, Entries)
    }
    fn ls(&mut self, path: &str, recursive: bool) -> Result<Vec<Entry>> {
        answer!(self, Request::Ls { path: path.into(), recursive }, Entries)
    }
    fn stat(&mut self, path: &str) -> Result<Entry> {
        answer!(self, Request::Stat { path: path.into() }, Entry)
    }
    fn read(&mut self, path: &str, version: Option<i64>) -> Result<(Vec<u8>, i64)> {
        answer!(self, Request::Read { path: path.into(), version }, Read)
    }
    fn section(&mut self, path: &str, heading: &str) -> Result<Option<Vec<u8>>> {
        answer!(self, Request::Section { path: path.into(), heading: heading.into() }, MaybeBytes)
    }
    fn search(&mut self, query: &str, prefix: &str, limit: i64, per_file: i64) -> Result<Vec<Hit>> {
        answer!(self, Request::Search { query: query.into(), prefix: prefix.into(), limit, per_file }, Hits)
    }
    fn sections_of(&mut self, path: &str) -> Result<Vec<(i64, i64, String)>> {
        answer!(self, Request::SectionsOf { path: path.into() }, Sections)
    }
    fn property_keys(&mut self, prefix: &str, limit: i64) -> Result<Vec<PropKey>> {
        answer!(self, Request::PropertyKeys { prefix: prefix.into(), limit }, PropKeys)
    }
    fn property_values(&mut self, key: &str, prefix: &str, limit: i64) -> Result<Vec<PropValue>> {
        answer!(self, Request::PropertyValues { key: key.into(), prefix: prefix.into(), limit }, PropValues)
    }
    fn property_find(&mut self, query: &str, folder: &str, limit: i64) -> Result<Vec<PropHit>> {
        answer!(self, Request::PropertyFind { query: query.into(), folder: folder.into(), limit }, PropHits)
    }
    fn outline(&mut self, prefix: &str, heading: Option<&str>, mode: &str, max_level: Option<i64>, limit: i64) -> Result<Vec<OutlineRow>> {
        answer!(self, Request::Outline { prefix: prefix.into(), heading: owned(heading), mode: mode.into(), max_level, limit }, OutlineRows)
    }
    fn heading_names(&mut self, prefix: &str, starts: &str, limit: i64) -> Result<Vec<HeadingName>> {
        answer!(self, Request::HeadingNames { prefix: prefix.into(), starts: starts.into(), limit }, HeadingNames)
    }
    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.transport.call(Request::Mkdir { path: path.into() }).map(drop)
    }
    fn settle(&mut self) -> Result<()> {
        self.transport.call(Request::Settle).map(drop)
    }
    fn write(&mut self, path: &str, content: &[u8], base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<Written> {
        answer!(self, Request::Write { path: path.into(), content: content.to_vec(), base_version, author: owned(author), message: owned(message) }, Written)
    }
    fn edit(&mut self, path: &str, old: &[u8], new: &[u8], author: Option<&str>, message: Option<&str>) -> Result<Written> {
        answer!(self, Request::Edit { path: path.into(), old: old.to_vec(), new: new.to_vec(), author: owned(author), message: owned(message) }, Written)
    }
    fn append(&mut self, path: &str, tail: &[u8], author: Option<&str>, message: Option<&str>) -> Result<Written> {
        answer!(self, Request::Append { path: path.into(), tail: tail.to_vec(), author: owned(author), message: owned(message) }, Written)
    }
    fn replace_lines(&mut self, path: &str, from: i64, to: i64, text: &[u8], base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<Written> {
        answer!(
            self,
            Request::ReplaceLines { path: path.into(), from, to, text: text.to_vec(), base_version, author: owned(author), message: owned(message) },
            Written
        )
    }
    fn replace_ranges(&mut self, path: &str, ranges: &[LineRange], base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<Written> {
        answer!(
            self,
            Request::ReplaceRanges { path: path.into(), ranges: ranges.to_vec(), base_version, author: owned(author), message: owned(message) },
            Written
        )
    }
    fn links(&mut self, path: &str, statuses: &[&str]) -> Result<Vec<LinkRow>> {
        answer!(self, Request::Links { path: path.into(), statuses: statuses.iter().map(|s| s.to_string()).collect() }, LinkRows)
    }
    fn backlinks(&mut self, path: &str) -> Result<Vec<LinkRow>> {
        answer!(self, Request::Backlinks { path: path.into() }, LinkRows)
    }
    fn history(&mut self, path: &str) -> Result<Vec<Commit>> {
        answer!(self, Request::History { path: path.into() }, Commits)
    }
    fn diff(&mut self, path: &str, v1: i64, v2: i64) -> Result<String> {
        answer!(self, Request::Diff { path: path.into(), v1, v2 }, Text)
    }
    fn hunks(&mut self, path: &str, v1: i64, v2: i64) -> Result<Vec<Hunk>> {
        answer!(self, Request::Hunks { path: path.into(), v1, v2 }, Hunks)
    }
    fn chunks(&mut self, path: &str, version: Option<i64>) -> Result<Vec<Chunk>> {
        answer!(self, Request::Chunks { path: path.into(), version }, Chunks)
    }
    fn mv(&mut self, from: &str, to: &str, author: Option<&str>, message: Option<&str>) -> Result<()> {
        self.transport.call(Request::Mv { from: from.into(), to: to.into(), author: owned(author), message: owned(message) }).map(drop)
    }
    fn mv_links(&mut self, from: &str, to: &str, author: Option<&str>, message: Option<&str>, update: Option<bool>) -> Result<Vec<MovedLink>> {
        answer!(self, Request::MvLinks { from: from.into(), to: to.into(), author: owned(author), message: owned(message), update }, MovedLinks)
    }
    fn rm(&mut self, path: &str, author: Option<&str>, message: Option<&str>) -> Result<()> {
        self.transport.call(Request::Rm { path: path.into(), author: owned(author), message: owned(message) }).map(drop)
    }
    fn path_history(&mut self, path: &str) -> Result<Vec<PathEvent>> {
        answer!(self, Request::PathHistory { path: path.into() }, PathEvents)
    }
    fn set_session_path_history(&mut self, on: Option<bool>) -> Result<()> {
        self.transport.call(Request::SetSessionPathHistory { on }).map(drop)
    }
    fn path_history_enabled(&mut self) -> Result<bool> {
        answer!(self, Request::PathHistoryEnabled, Bool)
    }
    fn setting(&mut self, key: &str) -> Result<Option<String>> {
        answer!(self, Request::Setting { key: key.into() }, MaybeText)
    }
    fn set_setting(&mut self, key: &str, value: Option<&str>) -> Result<Option<String>> {
        answer!(self, Request::SetSetting { key: key.into(), value: owned(value) }, MaybeText)
    }
    fn last_seq(&mut self) -> Result<i64> {
        answer!(self, Request::LastSeq, Int)
    }
    fn feed(&mut self, since: i64, limit: i64) -> Result<Vec<Change>> {
        answer!(self, Request::Feed { since, limit }, Changes)
    }
    fn wait(&mut self, timeout: Duration) -> Result<()> {
        self.transport.call(Request::Wait { millis: timeout.as_millis() as u64 }).map(drop)
    }
    fn import(
        &mut self,
        files: &mut dyn Iterator<Item = (String, Vec<u8>)>,
        author: Option<&str>,
        batch: usize,
        progress: &mut dyn FnMut(&ImportStats),
        on_error: &mut dyn FnMut(&str, &StoreError),
    ) -> Result<ImportStats> {
        // The store commits a batch at a time; here a batch is a message, and the progress
        // and the errors are reported from what each one answers.
        let batch = batch.max(1);
        let mut total = ImportStats::default();
        loop {
            let mut chunk: Vec<(String, Vec<u8>)> = Vec::with_capacity(batch);
            while chunk.len() < batch {
                match files.next() {
                    Some(f) => chunk.push(f),
                    None => break,
                }
            }
            if chunk.is_empty() {
                break;
            }
            let (stats, failed) = match self.transport.call(Request::Import { files: chunk, author: owned(author) })? {
                Response::Import { stats, failed } => (stats, failed),
                other => return Err(unexpected("import stats", &other)),
            };
            for (path, e) in &failed {
                on_error(path, e);
            }
            total.files += stats.files;
            total.created += stats.created;
            total.updated += stats.updated;
            total.unchanged += stats.unchanged;
            total.failed += stats.failed;
            total.bytes += stats.bytes;
            progress(&total);
        }
        Ok(total)
    }
    fn file_heads(&mut self, prefix: &str) -> Result<Vec<FileHead>> {
        answer!(self, Request::FileHeads { prefix: prefix.into() }, FileHeads)
    }
    fn sync_bases(&mut self, prefix: &str) -> Result<Vec<SyncBase>> {
        answer!(self, Request::SyncBases { prefix: prefix.into() }, SyncBases).map(|v| v.into_iter().map(Into::into).collect())
    }
    fn sync_base(&mut self, prefix: &str, dir: &str) -> Result<Option<SyncBase>> {
        answer!(self, Request::SyncBase { prefix: prefix.into(), dir: dir.into() }, MaybeSyncBase).map(|b| b.map(Into::into))
    }
    fn save_sync_base(&mut self, base: &SyncBase) -> Result<()> {
        self.transport.call(Request::SaveSyncBase { base: base.clone().into() }).map(drop)
    }
    fn put_sync_files(&mut self, prefix: &str, dir: &str, files: &[BaseFile]) -> Result<bool> {
        answer!(self, Request::PutSyncFiles { prefix: prefix.into(), dir: dir.into(), files: files.to_vec() }, Bool)
    }
    fn sync_head(&mut self, prefix: &str, dir: &str) -> Result<Option<SyncBase>> {
        answer!(self, Request::SyncHead { prefix: prefix.into(), dir: dir.into() }, MaybeSyncBase).map(|b| b.map(Into::into))
    }
    fn file_heads_delta(&mut self, prefix: &str, dir: &str) -> Result<Option<HeadsDelta>> {
        answer!(self, Request::FileHeadsDelta { prefix: prefix.into(), dir: dir.into() }, HeadsDelta)
    }
    fn save_sync_base_delta(&mut self, base: &SyncBase, upsert: &[BaseFile], remove: &[String]) -> Result<()> {
        self.transport
            .call(Request::SaveSyncBaseDelta { base: base.clone().into(), upsert: upsert.to_vec(), remove: remove.to_vec() })
            .map(drop)
    }
    fn rename_sync_dir(&mut self, prefix: &str, from: &str, to: &str) -> Result<()> {
        self.transport.call(Request::RenameSyncDir { prefix: prefix.into(), from: from.into(), to: to.into() }).map(drop)
    }
    fn sql(&mut self, query: &str, params: &[String], author: Option<&str>, write: bool, dry_run: bool) -> Result<SqlResult> {
        answer!(self, Request::Sql { query: query.into(), params: params.to_vec(), author: owned(author), write, dry_run }, Sql)
    }
    fn revert_batch(&mut self, batch: &str, author: Option<&str>, skip_changed: bool, dry_run: bool) -> Result<RevertOutcome> {
        answer!(self, Request::RevertBatch { batch: batch.into(), author: owned(author), skip_changed, dry_run }, Revert)
    }
    fn all_sync_bases(&mut self) -> Result<Vec<SyncBase>> {
        answer!(self, Request::AllSyncBases, SyncBases).map(|v| v.into_iter().map(Into::into).collect())
    }
    fn owner_paths(&mut self, paths: &[String]) -> Result<Vec<String>> {
        answer!(self, Request::OwnerPaths { paths: paths.to_vec() }, Strings)
    }
    fn asset_items_named(&mut self, store: &str, locations: &[String]) -> Result<Option<ItemsNamed>> {
        answer!(self, Request::AssetItemsNamed { store: store.into(), locations: locations.to_vec() }, ItemsNamed)
    }
    fn may_name(&mut self, location: &str) -> Result<bool> {
        answer!(self, Request::MayName { location: location.into() }, Bool)
    }
    fn asset_item_users(&mut self, store: &str, location: &str, own: &str) -> Result<Option<ItemUsers>> {
        answer!(self, Request::AssetItemUsers { store: store.into(), location: location.into(), own: own.into() }, ItemUsers)
    }
    fn asset_store_users(&mut self, store: &str) -> Result<Option<usize>> {
        answer!(self, Request::AssetStoreUsers { store: store.into() }, MaybeUsize)
    }
    fn asset_stores(&mut self) -> Result<Vec<AssetStore>> {
        answer!(self, Request::AssetStores, AssetStores)
    }
    fn put_asset_store(&mut self, store: &AssetStore) -> Result<()> {
        self.transport.call(Request::PutAssetStore { store: store.clone() }).map(drop)
    }
    fn remove_asset_store(&mut self, name: &str) -> Result<bool> {
        answer!(self, Request::RemoveAssetStore { name: name.into() }, Bool)
    }
}

// -------------------------------------------------------------------------- dispatch

/// A request applied to a store: the far end of every transport.
pub fn dispatch(st: &mut dyn Store, req: Request) -> Result<Response> {
    use Request as Q;
    use Response as R;
    fn d(s: &Option<String>) -> Option<&str> {
        s.as_deref()
    }
    Ok(match req {
        Q::Authenticate { bearer } => st.authenticate(&bearer).map(|()| R::Unit)?,
        Q::Whoami => R::Whoami(st.whoami()?),
        Q::AccountCreate { name, kind, root } => st.account_create(&name, &kind, d(&root)).map(|()| R::Unit)?,
        Q::AccountLs => R::AccountRows(st.account_ls()?),
        Q::AccountDisable { name, disabled } => st.account_disable(&name, disabled).map(|()| R::Unit)?,
        Q::AccountConvert { name, alias } => R::Text(st.account_convert(&name, d(&alias))?),
        Q::TokenCreate { account, label, expires_at } => R::Token(st.token_create(&account, d(&label), d(&expires_at))?),
        Q::TokenLs { account } => R::TokenRows(st.token_ls(d(&account))?),
        Q::TokenRevoke { id } => st.token_revoke(id).map(|()| R::Unit)?,
        Q::AccessGrant { account, path, rights, alias } => R::ShareRow(st.access_grant(&account, &path, &rights, d(&alias))?),
        Q::AccessRename { account, from, to } => st.access_rename(&account, &from, &to).map(|()| R::Unit)?,
        Q::AccessRevoke { account, alias } => st.access_revoke(&account, &alias).map(|()| R::Unit)?,
        Q::AccessLs { who } => R::ShareRows(st.access_ls(d(&who))?),
        Q::ShareState => R::Pairs(st.share_state()?),
        Q::Backend => R::Text(st.backend().to_string()),
        Q::Init => st.init().map(|()| R::Unit)?,
        Q::Trash { parent } => R::TrashRows(st.trash(parent)?),
        Q::TrashRestore { id, author } => R::TrashRow(st.trash_restore(id, d(&author))?),
        Q::Nodes { prefix } => R::Entries(st.nodes(&prefix)?),
        Q::Ls { path, recursive } => R::Entries(st.ls(&path, recursive)?),
        Q::Stat { path } => R::Entry(st.stat(&path)?),
        Q::Read { path, version } => R::Read(st.read(&path, version)?),
        Q::Section { path, heading } => R::MaybeBytes(st.section(&path, &heading)?),
        Q::Search { query, prefix, limit, per_file } => R::Hits(st.search(&query, &prefix, limit, per_file)?),
        Q::SectionsOf { path } => R::Sections(st.sections_of(&path)?),
        Q::PropertyKeys { prefix, limit } => R::PropKeys(st.property_keys(&prefix, limit)?),
        Q::PropertyValues { key, prefix, limit } => R::PropValues(st.property_values(&key, &prefix, limit)?),
        Q::PropertyFind { query, folder, limit } => R::PropHits(st.property_find(&query, &folder, limit)?),
        Q::Outline { prefix, heading, mode, max_level, limit } => R::OutlineRows(st.outline(&prefix, d(&heading), &mode, max_level, limit)?),
        Q::HeadingNames { prefix, starts, limit } => R::HeadingNames(st.heading_names(&prefix, &starts, limit)?),
        Q::Mkdir { path } => st.mkdir(&path).map(|()| R::Unit)?,
        Q::Settle => st.settle().map(|()| R::Unit)?,
        Q::Write { path, content, base_version, author, message } => R::Written(st.write(&path, &content, base_version, d(&author), d(&message))?),
        Q::Edit { path, old, new, author, message } => R::Written(st.edit(&path, &old, &new, d(&author), d(&message))?),
        Q::Append { path, tail, author, message } => R::Written(st.append(&path, &tail, d(&author), d(&message))?),
        Q::ReplaceLines { path, from, to, text, base_version, author, message } => {
            R::Written(st.replace_lines(&path, from, to, &text, base_version, d(&author), d(&message))?)
        }
        Q::ReplaceRanges { path, ranges, base_version, author, message } => {
            R::Written(st.replace_ranges(&path, &ranges, base_version, d(&author), d(&message))?)
        }
        Q::Links { path, statuses } => {
            let statuses: Vec<&str> = statuses.iter().map(String::as_str).collect();
            R::LinkRows(st.links(&path, &statuses)?)
        }
        Q::Backlinks { path } => R::LinkRows(st.backlinks(&path)?),
        Q::History { path } => R::Commits(st.history(&path)?),
        Q::Diff { path, v1, v2 } => R::Text(st.diff(&path, v1, v2)?),
        Q::Hunks { path, v1, v2 } => R::Hunks(st.hunks(&path, v1, v2)?),
        Q::Chunks { path, version } => R::Chunks(st.chunks(&path, version)?),
        Q::Mv { from, to, author, message } => st.mv(&from, &to, d(&author), d(&message)).map(|()| R::Unit)?,
        Q::MvLinks { from, to, author, message, update } => R::MovedLinks(st.mv_links(&from, &to, d(&author), d(&message), update)?),
        Q::Rm { path, author, message } => st.rm(&path, d(&author), d(&message)).map(|()| R::Unit)?,
        Q::PathHistory { path } => R::PathEvents(st.path_history(&path)?),
        Q::SetSessionPathHistory { on } => st.set_session_path_history(on).map(|()| R::Unit)?,
        Q::PathHistoryEnabled => R::Bool(st.path_history_enabled()?),
        Q::Setting { key } => R::MaybeText(st.setting(&key)?),
        Q::SetSetting { key, value } => R::MaybeText(st.set_setting(&key, d(&value))?),
        Q::LastSeq => R::Int(st.last_seq()?),
        Q::Feed { since, limit } => R::Changes(st.feed(since, limit)?),
        Q::Wait { millis } => st.wait(Duration::from_millis(millis)).map(|()| R::Unit)?,
        Q::Import { files, author } => {
            let mut failed = Vec::new();
            let n = files.len().max(1);
            let stats = st.import(&mut files.into_iter(), d(&author), n, &mut |_| {}, &mut |path, e| failed.push((path.to_string(), e.clone())))?;
            R::Import { stats, failed }
        }
        Q::FileHeads { prefix } => R::FileHeads(st.file_heads(&prefix)?),
        Q::SyncBases { prefix } => R::SyncBases(st.sync_bases(&prefix)?.into_iter().map(Into::into).collect()),
        Q::SyncBase { prefix, dir } => R::MaybeSyncBase(st.sync_base(&prefix, &dir)?.map(Into::into)),
        Q::SaveSyncBase { base } => st.save_sync_base(&base.into()).map(|()| R::Unit)?,
        Q::PutSyncFiles { prefix, dir, files } => R::Bool(st.put_sync_files(&prefix, &dir, &files)?),
        Q::SyncHead { prefix, dir } => R::MaybeSyncBase(st.sync_head(&prefix, &dir)?.map(Into::into)),
        Q::FileHeadsDelta { prefix, dir } => R::HeadsDelta(st.file_heads_delta(&prefix, &dir)?),
        Q::SaveSyncBaseDelta { base, upsert, remove } => st.save_sync_base_delta(&base.into(), &upsert, &remove).map(|()| R::Unit)?,
        Q::RenameSyncDir { prefix, from, to } => st.rename_sync_dir(&prefix, &from, &to).map(|()| R::Unit)?,
        Q::Sql { query, params, author, write, dry_run } => R::Sql(st.sql(&query, &params, d(&author), write, dry_run)?),
        Q::RevertBatch { batch, author, skip_changed, dry_run } => R::Revert(st.revert_batch(&batch, d(&author), skip_changed, dry_run)?),
        Q::AllSyncBases => R::SyncBases(st.all_sync_bases()?.into_iter().map(Into::into).collect()),
        Q::OwnerPaths { paths } => R::Strings(st.owner_paths(&paths)?),
        Q::AssetItemsNamed { store, locations } => R::ItemsNamed(st.asset_items_named(&store, &locations)?),
        Q::MayName { location } => R::Bool(st.may_name(&location)?),
        Q::AssetItemUsers { store, location, own } => R::ItemUsers(st.asset_item_users(&store, &location, &own)?),
        Q::AssetStoreUsers { store } => R::MaybeUsize(st.asset_store_users(&store)?),
        Q::AssetStores => R::AssetStores(st.asset_stores()?),
        Q::PutAssetStore { store } => st.put_asset_store(&store).map(|()| R::Unit)?,
        Q::RemoveAssetStore { name } => R::Bool(st.remove_asset_store(&name)?),
        Q::Batch { requests } => R::Batch(requests.into_iter().map(|r| dispatch(st, r).into()).collect()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store behind the loopback answers exactly as it does in process, and every message
    /// has been through the codec on the way.
    #[test]
    fn loopback_round_trips_calls_and_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = crate::store::open(&tmp.path().join("kb.db").to_string_lossy()).unwrap();
        let mut st = RemoteStore::connect(Loopback::new(inner, Json)).unwrap();
        assert_eq!(st.backend(), "sqlite");
        st.init().unwrap();
        let w = st.write("/a.md", b"one\ntwo\n", None, Some("me"), None).unwrap();
        assert_eq!((w.version, w.kind.as_str()), (1, "direct"));
        assert_eq!(st.read("/a.md", None).unwrap(), (b"one\ntwo\n".to_vec(), 1));
        // A store error keeps its code across the seam.
        let e = st.read("/missing.md", None).unwrap_err();
        assert_eq!(e.code, "TX003", "{e:?}");
        // A batch answers each request on its own.
        let replies = st.transport_mut().call_many(vec![Request::LastSeq, Request::Stat { path: "/nope".into() }, Request::Ls { path: "/".into(), recursive: false }]).unwrap();
        assert!(matches!(replies[0], Ok(Response::Int(_))));
        assert_eq!(replies[1].as_ref().unwrap_err().code, "TX003");
        assert!(matches!(&replies[2], Ok(Response::Entries(v)) if v.len() == 1));
        assert!(st.transport().bytes_up > 0 && st.transport().bytes_down > 0);
    }
}
