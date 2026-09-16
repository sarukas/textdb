//! Accounts, bearer tokens and grants on SQLite: the three tables, and loading a connection's
//! [`View`] from a bearer.
//!
//! The model itself is [`textdb_core::access`]; this file is only its persistence. The Postgres
//! extension has the same three tables and loads the same `View`, so a behaviour difference
//! between the engines has to be a bug in one of these two files rather than a difference of
//! opinion about the rules.
//!
//! **The bearer never reaches the tables.** `token.hash` is the SHA-256 of it, hex; a lookup
//! hashes what the caller presented and matches on that, so a store file discloses no bearer even
//! to whoever can open it (which is otherwise full access — see #12 L1).

use rusqlite::{Connection, OptionalExtension};
use textdb_core::access::{Grant, Grants, Namespace, Rights, View};

use textdb_core::storage::Result;
use textdb_core::TextdbError;

use crate::storage::sql_err;

/// One account as stored.
#[derive(Clone, Debug)]
pub struct Account {
    pub id: i64,
    pub name: String,
    pub kind: String,
    /// Set for a single-root account: the node its root *is*.
    pub root_node_id: Option<i64>,
    pub created_at: String,
    pub disabled_at: Option<String>,
}

impl Account {
    pub fn namespace(&self) -> Namespace {
        if self.root_node_id.is_some() {
            Namespace::SingleRoot
        } else {
            Namespace::Aliased
        }
    }
}

/// One token as stored. The bearer is not here and cannot be recovered.
#[derive(Clone, Debug)]
pub struct Token {
    pub id: i64,
    pub account_id: i64,
    pub account: String,
    pub label: Option<String>,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
}

impl Token {
    /// Usable right now, given the store's clock.
    pub fn live(&self, now: &str) -> bool {
        self.revoked_at.is_none() && self.expires_at.as_deref().is_none_or(|e| e > now)
    }
}

/// The hex SHA-256 of a bearer, which is what `token.hash` holds.
pub fn hash_bearer(bearer: &str) -> String {
    // blake3 is the store's hash everywhere else, but a token hash is a credential digest and
    // sha256 is what the design names, what an operator expects to be able to reproduce with
    // `sha256sum`, and what a future server-side check in another language will reach for.
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bearer.as_bytes());
    hex_lower(&h.finalize())
}

/// A fresh bearer: 32 bytes of randomness, URL-safe, prefixed so it is recognisable in a log or
/// an environment variable and greppable by a secret scanner.
pub fn new_bearer() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("tdb_{}", hex_lower(&b))
}

fn hex_lower(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Does this store delegate at all?
///
/// A store with no accounts has no token anyone could present, so every connection is the owner.
/// Checked once when a connection opens, so a store that never uses the feature pays one
/// `SELECT count(*)` over an empty table for it and nothing else — which is the difference
/// between "this costs nothing if you don't use it" being a claim and being a fact.
pub fn any_accounts(conn: &Connection, p: &str) -> Result<bool> {
    let n: i64 = conn
        .query_row(&format!("SELECT count(*) FROM (SELECT 1 FROM {p}account LIMIT 1)"), [], |r| r.get(0))
        .map_err(sql_err)?;
    Ok(n > 0)
}

pub fn account_by_name(conn: &Connection, p: &str, name: &str) -> Result<Option<Account>> {
    conn.query_row(
        &format!("SELECT id, name, kind, root_node_id, created_at, disabled_at FROM {p}account WHERE name = ?1"),
        [name],
        row_to_account,
    )
    .optional()
    .map_err(sql_err)
}

pub fn accounts(conn: &Connection, p: &str) -> Result<Vec<Account>> {
    let mut st = conn
        .prepare(&format!("SELECT id, name, kind, root_node_id, created_at, disabled_at FROM {p}account ORDER BY name"))
        .map_err(sql_err)?;
    let rows = st.query_map([], row_to_account).map_err(sql_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
}

fn row_to_account(r: &rusqlite::Row) -> rusqlite::Result<Account> {
    Ok(Account {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: r.get(2)?,
        root_node_id: r.get(3)?,
        created_at: r.get(4)?,
        disabled_at: r.get(5)?,
    })
}

/// Create an account. `root` makes it single-root: its root *is* that node.
pub fn create_account(conn: &Connection, p: &str, name: &str, kind: &str, root_node_id: Option<i64>, now: &str) -> Result<Account> {
    if name.is_empty() || name.contains('/') || name.trim() != name {
        return Err(TextdbError::InvalidEdit(format!("'{name}' cannot be an account name")));
    }
    if !matches!(kind, "agent" | "person") {
        return Err(TextdbError::InvalidEdit(format!("account kind is 'agent' or 'person', not '{kind}'")));
    }
    if account_by_name(conn, p, name)?.is_some() {
        return Err(TextdbError::InvalidEdit(format!("there is already an account called '{name}'")));
    }
    conn.execute(
        &format!("INSERT INTO {p}account (name, kind, root_node_id, created_at) VALUES (?1, ?2, ?3, ?4)"),
        rusqlite::params![name, kind, root_node_id, now],
    )
    .map_err(sql_err)?;
    account_by_name(conn, p, name)?.ok_or_else(|| TextdbError::Storage("account vanished after insert".into()))
}

/// Mint a token for an account and return `(bearer, id)`. The bearer is returned once and is not
/// recoverable afterwards.
pub fn create_token(conn: &Connection, p: &str, account_id: i64, label: Option<&str>, expires_at: Option<&str>, now: &str) -> Result<(String, i64)> {
    let bearer = new_bearer();
    conn.execute(
        &format!("INSERT INTO {p}token (account_id, hash, label, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)"),
        rusqlite::params![account_id, hash_bearer(&bearer), label, now, expires_at],
    )
    .map_err(sql_err)?;
    Ok((bearer, conn.last_insert_rowid()))
}

pub fn tokens(conn: &Connection, p: &str, account: Option<&str>) -> Result<Vec<Token>> {
    let mut sql = format!(
        "SELECT t.id, t.account_id, a.name, t.label, t.created_at, t.expires_at, t.revoked_at, t.last_used_at \
         FROM {p}token t JOIN {p}account a ON a.id = t.account_id"
    );
    if account.is_some() {
        sql.push_str(" WHERE a.name = ?1");
    }
    sql.push_str(" ORDER BY t.id");
    let mut st = conn.prepare(&sql).map_err(sql_err)?;
    let map = |r: &rusqlite::Row| {
        Ok(Token {
            id: r.get(0)?,
            account_id: r.get(1)?,
            account: r.get(2)?,
            label: r.get(3)?,
            created_at: r.get(4)?,
            expires_at: r.get(5)?,
            revoked_at: r.get(6)?,
            last_used_at: r.get(7)?,
        })
    };
    let rows = match account {
        Some(a) => st.query_map([a], map).map_err(sql_err)?.collect::<rusqlite::Result<Vec<_>>>(),
        None => st.query_map([], map).map_err(sql_err)?.collect::<rusqlite::Result<Vec<_>>>(),
    };
    rows.map_err(sql_err)
}

pub fn revoke_token(conn: &Connection, p: &str, id: i64, now: &str) -> Result<bool> {
    let n = conn
        .execute(
            &format!("UPDATE {p}token SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL"),
            rusqlite::params![id, now],
        )
        .map_err(sql_err)?;
    Ok(n > 0)
}

/// The grants of one account, as the model's own type.
///
/// A grant binds to a node, so the share root's *current* path is read here rather than stored:
/// the owner renaming or moving the shared folder leaves the account's paths untouched, which is
/// only true if the path is looked up fresh. A share root in the trash comes back `dormant`.
pub fn grants_of(conn: &Connection, p: &str, account_id: i64) -> Result<Grants> {
    let mut st = conn
        .prepare(&format!(
            "SELECT g.node_id, g.alias, g.rights, n.path, n.deleted_at IS NOT NULL, g.revoked_at IS NOT NULL \
             FROM {p}grant g JOIN {p}node n ON n.id = g.node_id \
             WHERE g.account_id = ?1 ORDER BY g.alias"
        ))
        .map_err(sql_err)?;
    let rows = st
        .query_map([account_id], |r| {
            Ok(Grant {
                node_id: r.get(0)?,
                alias: r.get(1)?,
                rights: Rights::parse(&r.get::<_, String>(2)?).unwrap_or(Rights::Ro),
                store_path: r.get(3)?,
                dormant: r.get(4)?,
                revoked: r.get(5)?,
            })
        })
        .map_err(sql_err)?;
    Ok(Grants::from_rows(rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)?))
}

/// Every grant in the store, for `access ls` with no argument.
pub fn all_grants(conn: &Connection, p: &str) -> Result<Vec<(String, Grant)>> {
    let mut st = conn
        .prepare(&format!(
            "SELECT a.name, g.node_id, g.alias, g.rights, n.path, n.deleted_at IS NOT NULL, g.revoked_at IS NOT NULL \
             FROM {p}grant g JOIN {p}account a ON a.id = g.account_id JOIN {p}node n ON n.id = g.node_id \
             ORDER BY a.name, g.alias"
        ))
        .map_err(sql_err)?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Grant {
                    node_id: r.get(1)?,
                    alias: r.get(2)?,
                    rights: Rights::parse(&r.get::<_, String>(3)?).unwrap_or(Rights::Ro),
                    store_path: r.get(4)?,
                    dormant: r.get(5)?,
                    revoked: r.get(6)?,
                },
            ))
        })
        .map_err(sql_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
}

/// Store a grant that the model has already approved.
pub fn insert_grant(conn: &Connection, p: &str, account_id: i64, g: &Grant, by: Option<&str>, now: &str) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO {p}grant (account_id, node_id, alias, rights, granted_by, granted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(account_id, node_id) DO UPDATE SET alias = excluded.alias, rights = excluded.rights, \
             granted_by = excluded.granted_by, granted_at = excluded.granted_at, revoked_at = NULL"
        ),
        rusqlite::params![account_id, g.node_id, g.alias, g.rights.as_str(), by, now],
    )
    .map_err(sql_err)?;
    Ok(())
}

pub fn rename_grant(conn: &Connection, p: &str, account_id: i64, from: &str, to: &str) -> Result<bool> {
    let n = conn
        .execute(
            &format!("UPDATE {p}grant SET alias = ?3 WHERE account_id = ?1 AND alias = ?2"),
            rusqlite::params![account_id, from, to],
        )
        .map_err(sql_err)?;
    Ok(n > 0)
}

/// Take a share away without forgetting it.
///
/// The row stays, marked. An account whose checkout still holds those files must be answered
/// `forbidden` rather than `not found`, because `sync` deletes what is absent from the store and
/// leaves alone what it may not touch — so a `DELETE` here would empty someone's vault.
pub fn revoke_grant(conn: &Connection, p: &str, account_id: i64, alias: &str, now: &str) -> Result<bool> {
    let n = conn
        .execute(
            &format!("UPDATE {p}grant SET revoked_at = ?3 WHERE account_id = ?1 AND alias = ?2 AND revoked_at IS NULL"),
            rusqlite::params![account_id, alias, now],
        )
        .map_err(sql_err)?;
    Ok(n > 0)
}

/// Why a bearer did not produce a view. Each is reported the same way to the caller — one
/// message, no detail about which — because telling a holder of a revoked token apart from a
/// holder of a guessed one is free information.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Unknown,
    Expired,
    Revoked,
    AccountDisabled,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this token is not usable: it is unknown, expired or revoked")
    }
}

/// Resolve a bearer into the connection's view, and record that it was used.
///
/// The one entry point: everything downstream holds the `View` and never sees the bearer again.
pub fn authenticate(conn: &Connection, p: &str, bearer: &str, now: &str) -> Result<std::result::Result<View, AuthError>> {
    let hash = hash_bearer(bearer);
    let row: Option<(i64, i64, Option<String>, Option<String>)> = conn
        .query_row(
            &format!("SELECT id, account_id, expires_at, revoked_at FROM {p}token WHERE hash = ?1"),
            [&hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let (token_id, account_id, expires_at, revoked_at) = match row {
        Some(r) => r,
        None => return Ok(Err(AuthError::Unknown)),
    };
    if revoked_at.is_some() {
        return Ok(Err(AuthError::Revoked));
    }
    if expires_at.as_deref().is_some_and(|e| e <= now) {
        return Ok(Err(AuthError::Expired));
    }
    let account: Option<Account> = conn
        .query_row(
            &format!("SELECT id, name, kind, root_node_id, created_at, disabled_at FROM {p}account WHERE id = ?1"),
            [account_id],
            row_to_account,
        )
        .optional()
        .map_err(sql_err)?;
    let account = match account {
        Some(a) if a.disabled_at.is_none() => a,
        Some(_) => return Ok(Err(AuthError::AccountDisabled)),
        None => return Ok(Err(AuthError::Unknown)),
    };
    // Best effort: a store opened read-only still authenticates, it just does not record the use.
    let _ = conn.execute(
        &format!("UPDATE {p}token SET last_used_at = ?2 WHERE id = ?1"),
        rusqlite::params![token_id, now],
    );
    let grants = grants_of(conn, p, account.id)?;
    Ok(Ok(View::account(&account.name, account.namespace(), grants)))
}

// ---------------------------------------------------------------- the view, as SQL and as paths

/// A SQL predicate restricting `col` to the account's visible set, with its parameters.
///
/// `None` for the admin, meaning no predicate at all — which is what keeps every query a store
/// with no accounts runs byte-identical to what it ran before this feature existed.
///
/// A share is its root and everything below it. Written as a range rather than a `LIKE` or a
/// `substr`, for the reason `subtree_bounds` gives: a function of the column cannot use the
/// `node_path` index, and that was measured at 58x on 2000 files. The equality is not folded into
/// the range because it cannot be: `'/legal/contracts'` and `'/legal/contracts/'` have
/// `'/legal/contracts!x'` between them under BINARY collation, and that is a sibling folder, not
/// a child.
pub fn visible_sql(view: &View, col: &str) -> Option<(String, Vec<rusqlite::types::Value>)> {
    use rusqlite::types::Value;
    if view.is_admin() {
        return None;
    }
    let mut parts = Vec::new();
    let mut args = Vec::new();
    for g in view.grants().live() {
        let (lo, hi) = (format!("{}/", g.store_path), format!("{}0", g.store_path));
        parts.push(format!("({col} = ?{} OR ({col} >= ?{} AND {col} < ?{}))", args.len() + 1, args.len() + 2, args.len() + 3));
        args.push(Value::Text(g.store_path.clone()));
        args.push(Value::Text(lo));
        args.push(Value::Text(hi));
    }
    // An account with no live share sees nothing at all, which is a predicate that is never true
    // rather than one that is missing.
    if parts.is_empty() {
        return Some(("0".to_string(), Vec::new()));
    }
    Some((parts.join(" OR "), args))
}

/// The same, restricted to the shares this account may write.
pub fn writable_sql(view: &View, col: &str) -> Option<(String, Vec<rusqlite::types::Value>)> {
    use rusqlite::types::Value;
    if view.is_admin() {
        return None;
    }
    let mut parts = Vec::new();
    let mut args = Vec::new();
    for g in view.grants().live().filter(|g| g.rights.can_write()) {
        let (lo, hi) = (format!("{}/", g.store_path), format!("{}0", g.store_path));
        parts.push(format!("({col} = ?{} OR ({col} >= ?{} AND {col} < ?{}))", args.len() + 1, args.len() + 2, args.len() + 3));
        args.push(Value::Text(g.store_path.clone()));
        args.push(Value::Text(lo));
        args.push(Value::Text(hi));
    }
    if parts.is_empty() {
        return Some(("0".to_string(), Vec::new()));
    }
    Some((parts.join(" OR "), args))
}

/// Renumber the `?n` placeholders of a predicate so it can be appended after `offset` existing
/// parameters. `visible_sql` numbers from 1 because most callers have no others.
pub fn renumber(sql: &str, offset: usize) -> String {
    if offset == 0 {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + 8);
    let mut it = sql.char_indices().peekable();
    while let Some((_, c)) = it.next() {
        if c != '?' {
            out.push(c);
            continue;
        }
        let mut n = String::new();
        while let Some((_, d)) = it.peek() {
            if d.is_ascii_digit() {
                n.push(*d);
                it.next();
            } else {
                break;
            }
        }
        match n.parse::<usize>() {
            Ok(k) => out.push_str(&format!("?{}", k + offset)),
            Err(_) => {
                out.push('?');
                out.push_str(&n);
            }
        }
    }
    out
}

/// A path as the caller wrote it, as a store path — or the error the model says it is.
///
/// This is the one place a view path becomes a store path, so it is also the one place that
/// decides between "there is nothing there" and "you may not". Everything downstream works in
/// store paths and never has to ask again.
pub fn to_store(view: &View, view_path: &str) -> Result<String> {
    use textdb_core::access::Resolved;
    match view.to_store(view_path) {
        Resolved::In { store_path, .. } => Ok(store_path),
        Resolved::Root => Ok("/".to_string()),
        Resolved::Forbidden { alias, why } => Err(TextdbError::Forbidden(denial(&alias, why))),
        Resolved::NotFound => Err(TextdbError::NotFound(view_path.to_string())),
    }
}

/// What to say about a share the account holds and may not use.
///
/// A single-root account has no alias to name — its root *is* the share — so the message says
/// that rather than pointing at `/`, which reads like a bug in the caller.
fn denial(alias: &str, why: textdb_core::access::Denial) -> String {
    if alias.is_empty() {
        return format!("your root share is no longer available: {}", why.why());
    }
    format!("/{alias}: {}", why.why())
}

/// As [`to_store`], but for an operation that writes, so a read-only share is refused here.
pub fn to_store_rw(view: &View, view_path: &str) -> Result<String> {
    use textdb_core::access::{Namespace, Resolved};
    match view.to_store_rw(view_path) {
        Resolved::In { store_path, .. } => Ok(store_path),
        Resolved::Root => Err(TextdbError::Forbidden(at_the_root(view))),
        Resolved::Forbidden { alias, why } => Err(TextdbError::Forbidden(denial(&alias, why))),
        // A write one level below the root names something that would have to *be* a share.
        // Saying so is not a disclosure — the account already knows its own shares — and the
        // alternative is "not found: /notes.md", which reads like a bug in the caller rather
        // than the rule it actually ran into. Deeper paths stay `not found`, indistinguishable
        // from a folder that never existed, which is what keeps store paths unguessable.
        Resolved::NotFound if view.namespace() == Namespace::Aliased && view_path.trim_matches('/').split('/').count() == 1 => {
            Err(TextdbError::Forbidden(at_the_root(view)))
        }
        Resolved::NotFound => Err(TextdbError::NotFound(view_path.to_string())),
    }
}

fn at_the_root(view: &View) -> String {
    let shares: Vec<String> = view.grants().live().map(|g| format!("/{}/ ({})", g.alias, g.rights.as_str())).collect();
    if shares.is_empty() {
        return "the root lists your shares, and you have none".into();
    }
    format!("the root lists your shares; write inside one of them: {}", shares.join(", "))
}

// ---------------------------------------------------------------- per-connection sessions

/// The view each authenticated connection is running as, keyed by its `sqlite3*`.
///
/// The scalar and table-valued functions build a fresh [`TextDb`] per call — they have nothing
/// else to hang state on — so `textdb_auth()` has to leave the account somewhere the next call
/// will find it. This is that somewhere.
static SESSIONS: std::sync::Mutex<Vec<(usize, View)>> = std::sync::Mutex::new(Vec::new());

/// Has anything on this process ever authenticated?
///
/// Read before the lock, on every function call, so a store that delegates nothing pays one
/// relaxed atomic load rather than a mutex acquisition per `textdb_content`. It never goes back
/// to false: a connection that closes leaves the flag set, which costs a map lookup that finds
/// nothing, and that is the right trade against making the common path pay.
static ANY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Bind `view` to a connection. `View::admin()` clears it.
pub fn set_session(handle: usize, view: View) {
    let mut s = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    s.retain(|(h, _)| *h != handle);
    if !view.is_admin() {
        s.push((handle, view));
        ANY.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The view bound to a connection; the owner's when none is.
pub fn session(handle: usize) -> View {
    if !ANY.load(std::sync::atomic::Ordering::Relaxed) {
        return View::admin();
    }
    let s = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    s.iter().find(|(h, _)| *h == handle).map(|(_, v)| v.clone()).unwrap_or_else(View::admin)
}

/// Forget a connection's session. Called when a connection closes; harmless if it had none.
pub fn clear_session(handle: usize) {
    if !ANY.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut s = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    s.retain(|(h, _)| *h != handle);
}

/// Refuse an operation that would move or delete the *root* of a share.
///
/// A share root is a folder the account works inside and does not own. Nothing else in the model
/// needs this: writing inside a share root is ordinary, and only `mv` and `rm` can address the
/// folder itself. Without it, `rm /contracts` deletes the shared folder — for everyone.
pub fn refuse_share_root(view: &View, store_path: &str, what: &str) -> Result<()> {
    if view.is_admin() {
        return Ok(());
    }
    match view.share_root_of(store_path) {
        Some(g) => Err(TextdbError::Forbidden(format!(
            "/{} is a share, not a folder of yours to {what}; it is the owner's to move or delete",
            g.alias
        ))),
        None => Ok(()),
    }
}

/// Record that an account's set of shares changed, in that account's own feed.
///
/// `op` is `share`, `unshare` or `move`; `path` and `old_path` are the account's own — `/contracts`,
/// not `/legal/contracts`, because a share has no store path from the holder's side and a
/// revocation has to be legible *after* the folder stops being visible. The row is marked
/// `for_account`, so nobody else's feed carries it and `sync` can act on it (#12 F7–F9).
pub fn record_share_event(
    conn: &Connection,
    p: &str,
    account: &str,
    op: &str,
    path: &str,
    old_path: Option<&str>,
    by: Option<&str>,
    now: &str,
) -> Result<i64> {
    conn.execute(
        &format!(
            "INSERT INTO {p}change(ts, op, node_id, node_kind, path, old_path, author, for_account) \
             VALUES (?1, ?2, 0, 0, ?3, ?4, ?5, ?6)"
        ),
        rusqlite::params![now, op, path, old_path, by, account],
    )
    .map_err(sql_err)?;
    Ok(conn.last_insert_rowid())
}
