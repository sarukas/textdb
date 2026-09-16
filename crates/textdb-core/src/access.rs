//! The access model: accounts, shares and the translation between a store path and the path an
//! account sees. Pure — no database, no engine.
//!
//! Both bindings read the same three tables into a [`View`] once per connection and then ask this
//! module every question. That is what makes "identically on SQLite and Postgres" (#12 Q3) a fact
//! about one algorithm rather than a promise about two.
//!
//! The model in one paragraph: a **grant** gives an account one folder with everything below it,
//! under an **alias** chosen when the grant is made. The account's root lists its aliases and
//! nothing else; under an alias the subtree is the real subtree, unchanged. So a view path is
//! `/<alias>/<rest>` and the store path is `<grant's store path>/<rest>`, a prefix swap. A
//! **single-root** account is the one exception: its root *is* its share, and it can hold no
//! other grant.
//!
//! Why the rules are shaped the way they are is in the issue; what matters here is that they are
//! all decided at *grant* time ([`Grants::add`]) rather than at query time, because an alias
//! derived from the current set of shares would move an account's paths whenever another share
//! was added or removed.

use std::collections::BTreeMap;

/// What an account may do inside a share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rights {
    /// Read: `cat`, `ls`, `tree`, `stat`, `search`, `grep`, `history`, `links`, `meta get/find`,
    /// `sync` to disk, `export`.
    Ro,
    /// `Ro` plus every write, inside the share.
    Rw,
}

impl Rights {
    pub fn as_str(self) -> &'static str {
        match self {
            Rights::Ro => "ro",
            Rights::Rw => "rw",
        }
    }

    pub fn parse(s: &str) -> Option<Rights> {
        match s {
            "ro" => Some(Rights::Ro),
            "rw" => Some(Rights::Rw),
            _ => None,
        }
    }

    pub fn can_write(self) -> bool {
        self == Rights::Rw
    }
}

/// One share: a folder, everything below it, and the account's own name for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The node the grant binds to. Grants follow the *node*, so the owner renaming or moving the
    /// shared folder leaves the account's paths untouched.
    pub node_id: i64,
    /// The account's stable name for this share, and the first segment of every path it sees
    /// through it. Empty for a single-root account, whose root is the share itself.
    pub alias: String,
    /// The share root's current path in the store, normalised, without a trailing slash.
    pub store_path: String,
    pub rights: Rights,
    /// A share root that has been deleted to the trash: the alias vanishes from the account's
    /// root and everything under it answers `forbidden` rather than `not found`, so that `sync`
    /// leaves the files on disk instead of deleting them. Restoring the folder revives it.
    pub dormant: bool,
    /// The grant was taken away. The row stays, for the same reason a dormant one does: an
    /// account whose checkout still holds the files must be told `forbidden`, not `not found`,
    /// or its next sync deletes them. This is the one case that destroys data if the two are
    /// conflated, so a revoked grant is remembered rather than dropped.
    pub revoked: bool,
}

impl Grant {
    /// Live, or the reason it is not.
    pub fn denied(&self) -> Option<Denial> {
        // Revocation first: a share both revoked and whose folder was later trashed is, to its
        // former holder, revoked — that is the fact about them rather than about the store.
        if self.revoked {
            Some(Denial::Revoked)
        } else if self.dormant {
            Some(Denial::Dormant)
        } else {
            None
        }
    }

    pub fn live(&self) -> bool {
        self.denied().is_none()
    }
}

/// Why a grant could not be added. Each is a rule from §2 of the design, refused at grant time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantError {
    /// The default alias is taken in this account and no explicit one was given. Never
    /// auto-suffixed: `contracts-2` would make an account's paths depend on the order its shares
    /// were added, which is the one thing the alias exists to prevent.
    AliasTaken { alias: String, held_by: String },
    /// This account already holds a grant that contains the new one, or is contained by it. One
    /// file would have two paths with two rights.
    Overlaps { existing: String, existing_alias: String },
    /// The account already holds this very node under another alias.
    AlreadyGranted { alias: String },
    /// A single-root account holds its one share and no other.
    SingleRoot { root: String },
    /// The root is not a folder anyone can share: `/` itself, or a path that is not normalised.
    BadRoot { path: String, why: &'static str },
    /// An alias is one path segment, and not one of the two that mean something else.
    BadAlias { alias: String, why: &'static str },
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrantError::AliasTaken { alias, held_by } => write!(
                f,
                "the alias '{alias}' is already this account's name for {held_by}; choose another with --as, \
                 or --reuse-alias to bind it to a different folder — which is a move for anything that already \
                 synced it to disk"
            ),
            GrantError::Overlaps { existing, existing_alias } => write!(
                f,
                "this account already has {existing} as '{existing_alias}', and shares may not overlap; \
                 grant the subfolder alone, or raise the parent's rights"
            ),
            GrantError::AlreadyGranted { alias } => {
                write!(f, "this account already sees that folder as '{alias}'; regrant to change its rights")
            }
            GrantError::SingleRoot { root } => write!(
                f,
                "this is a single-root account: its root is {root} and it can hold no other share. \
                 `account convert NAME --multi` makes it a multi-share account, which changes every path it sees"
            ),
            GrantError::BadRoot { path, why } => write!(f, "cannot share {path}: {why}"),
            GrantError::BadAlias { alias, why } => write!(f, "'{alias}' cannot be an alias: {why}"),
        }
    }
}

/// What a view path turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The account's root. Not a folder: it lists the shares and nothing may be written at it.
    Root,
    /// A path inside a share, with the rights that share carries.
    In { store_path: String, rights: Rights, alias: String, node_id: i64 },
    /// Under an alias this account has, or had — a dormant or revoked share. Distinct from
    /// `NotFound` on purpose: this is the case that destroys data if the two are conflated,
    /// because `sync` deletes what is absent and leaves alone what is forbidden.
    Forbidden { alias: String, why: Denial },
    /// No alias of this account begins this path. The account learns nothing, and in particular
    /// cannot tell a real store path from one that was never there.
    NotFound,
}

/// Why a path under an alias the account knows is nonetheless refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denial {
    /// The share root is in the trash.
    Dormant,
    /// The grant was revoked, but the account's checkout still has the files.
    Revoked,
    /// Visible, but this operation needs `rw` and the share is `ro`.
    ReadOnly,
}

impl Denial {
    pub fn why(self) -> &'static str {
        match self {
            Denial::Dormant => "its folder is in the trash",
            Denial::Revoked => "it is no longer shared with you",
            Denial::ReadOnly => "you have read-only access to it",
        }
    }
}

/// What kind of namespace an account has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namespace {
    /// The usual shape: the root lists aliases, and every path has an alias as its first segment.
    /// Always one alias level, even for an account with a single share — otherwise adding a
    /// second share later would shift every existing path down one level.
    Aliased,
    /// `account create NAME --root /legal/contracts`: the root *is* the share. The right shape
    /// for an agent that owns exactly one vault.
    SingleRoot,
}

/// An account's namespace: its grants, and the rules for adding to them.
///
/// Kept sorted by alias, which is the order a root listing wants and makes the overlap test a
/// scan of a handful of rows. The table is tiny by construction — a grant per share per account.
#[derive(Clone, Debug, Default)]
pub struct Grants {
    by_alias: BTreeMap<String, Grant>,
}

impl Grants {
    pub fn new() -> Self {
        Grants::default()
    }

    /// Build from rows as they come out of the store, refusing nothing: the store is the record
    /// of what was granted, and a rule added later must not make an existing store unreadable.
    pub fn from_rows(rows: impl IntoIterator<Item = Grant>) -> Self {
        let mut g = Grants::new();
        for r in rows {
            g.by_alias.insert(r.alias.clone(), r);
        }
        g
    }

    pub fn is_empty(&self) -> bool {
        self.by_alias.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_alias.len()
    }

    /// Every grant, by alias.
    pub fn iter(&self) -> impl Iterator<Item = &Grant> {
        self.by_alias.values()
    }

    /// The grants an account's root lists: the live ones. A dormant share is still a grant — it
    /// is how `sync` knows to leave the directory alone — but its alias is not listed.
    pub fn live(&self) -> impl Iterator<Item = &Grant> {
        self.by_alias.values().filter(|g| g.live())
    }

    pub fn by_alias(&self, alias: &str) -> Option<&Grant> {
        self.by_alias.get(alias)
    }

    pub fn by_node(&self, node_id: i64) -> Option<&Grant> {
        self.by_alias.values().find(|g| g.node_id == node_id)
    }

    /// Add a grant, applying every rule the design decides at grant time.
    ///
    /// `alias` is the explicit `--as`; `None` defaults to the share root's own name, and a
    /// collision is then refused rather than suffixed.
    pub fn add(&mut self, ns: Namespace, node_id: i64, store_path: &str, alias: Option<&str>, rights: Rights) -> Result<Grant, GrantError> {
        let store_path = store_path.trim_end_matches('/');
        if store_path.is_empty() || store_path == "/" {
            return Err(GrantError::BadRoot {
                path: "/".into(),
                why: "the store root is everything; share a folder inside it",
            });
        }
        if !store_path.starts_with('/') || store_path.contains("//") {
            return Err(GrantError::BadRoot { path: store_path.into(), why: "not a normalised store path" });
        }
        if ns == Namespace::SingleRoot {
            let held = self.by_alias.values().next();
            if let Some(g) = held {
                return Err(GrantError::SingleRoot { root: g.store_path.clone() });
            }
        }

        let alias = match alias {
            Some(a) => a.to_string(),
            None => store_path.rsplit('/').next().unwrap_or_default().to_string(),
        };
        if ns == Namespace::Aliased {
            check_alias(&alias)?;
            // A revoked grant keeps its alias reserved: the account may still have those files
            // on disk, and handing the same name to a different folder would make its next sync
            // write one share's content over another's. Re-granting the same node revives it.
            if let Some(held) = self.by_alias.get(&alias) {
                if !(held.revoked && held.node_id == node_id) {
                    return Err(GrantError::AliasTaken { alias, held_by: held.store_path.clone() });
                }
            }
        }

        // Overlap: this account may not hold two shares where one contains the other, because the
        // same file would then have two paths and two rights.
        for g in self.by_alias.values() {
            if g.node_id == node_id {
                // Granting a folder that was taken away is how it comes back, with whatever
                // rights this grant names. Granting one the account already has is a mistake
                // worth naming — `access grant` on a live share changes its rights, and goes
                // through the regrant path rather than here.
                if g.denied().is_some() {
                    let back = Grant { node_id, alias: g.alias.clone(), store_path: store_path.to_string(), rights, dormant: false, revoked: false };
                    self.by_alias.insert(back.alias.clone(), back.clone());
                    return Ok(back);
                }
                return Err(GrantError::AlreadyGranted { alias: g.alias.clone() });
            }
            if contains(&g.store_path, store_path) || contains(store_path, &g.store_path) {
                return Err(GrantError::Overlaps { existing: g.store_path.clone(), existing_alias: g.alias.clone() });
            }
        }

        let alias = if ns == Namespace::SingleRoot { String::new() } else { alias };
        let g = Grant { node_id, alias: alias.clone(), store_path: store_path.to_string(), rights, dormant: false, revoked: false };
        self.by_alias.insert(alias, g.clone());
        Ok(g)
    }

    /// Change an alias. A move for the account, not a delete and a create: it is recorded as a
    /// path event so that `sync` moves the directory on disk rather than deleting it and pulling
    /// every file down again.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<Grant, GrantError> {
        check_alias(to)?;
        if let Some(held) = self.by_alias.get(to) {
            return Err(GrantError::AliasTaken { alias: to.into(), held_by: held.store_path.clone() });
        }
        let mut g = match self.by_alias.remove(from) {
            Some(g) => g,
            None => return Err(GrantError::BadAlias { alias: from.into(), why: "this account has no such share" }),
        };
        g.alias = to.to_string();
        self.by_alias.insert(to.to_string(), g.clone());
        Ok(g)
    }

    pub fn revoke(&mut self, alias: &str) -> Option<Grant> {
        self.by_alias.remove(alias)
    }
}

/// An alias is one path segment. `.` and `..` are refused by the path normaliser anyway; the rest
/// would produce a path no one can address.
fn check_alias(alias: &str) -> Result<(), GrantError> {
    let bad = |why| Err(GrantError::BadAlias { alias: alias.into(), why });
    match alias {
        "" => bad("it is empty"),
        "." | ".." => bad("it is a relative path segment"),
        a if a.contains('/') => bad("an alias is one path segment"),
        a if a.contains('\0') => bad("it contains NUL"),
        a if a.trim() != a => bad("it has leading or trailing whitespace"),
        _ => Ok(()),
    }
}

/// Is `inner` `outer` itself or below it? Both normalised, no trailing slash.
pub fn contains(outer: &str, inner: &str) -> bool {
    inner == outer || (inner.starts_with(outer) && inner.as_bytes().get(outer.len()) == Some(&b'/'))
}

/// One connection's account, and everything a question about a path needs.
///
/// The admin — whoever opened the store without a token — has no view: they see store paths and
/// every node. `View::admin()` is that, and `is_admin()` is the one branch every caller takes
/// before doing any of this work, so the un-authenticated path costs a bool test.
#[derive(Clone, Debug)]
pub struct View {
    account: Option<String>,
    ns: Namespace,
    grants: Grants,
}

impl View {
    /// The owner of the store: no translation, no filtering.
    pub fn admin() -> Self {
        View { account: None, ns: Namespace::Aliased, grants: Grants::new() }
    }

    pub fn account(name: &str, ns: Namespace, grants: Grants) -> Self {
        View { account: Some(name.to_string()), ns, grants }
    }

    pub fn is_admin(&self) -> bool {
        self.account.is_none()
    }

    pub fn name(&self) -> Option<&str> {
        self.account.as_deref()
    }

    pub fn namespace(&self) -> Namespace {
        self.ns
    }

    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    /// Resolve a path as this account wrote it into the store path it names.
    ///
    /// The whole access model is this function and [`View::to_view`]. Everything else — listings,
    /// search, writes, sync — is a caller that asks one of them and obeys the answer.
    pub fn to_store(&self, view_path: &str) -> Resolved {
        let p = view_path.trim_end_matches('/');
        let p = if p.is_empty() { "/" } else { p };
        if self.is_admin() {
            return Resolved::In { store_path: p.to_string(), rights: Rights::Rw, alias: String::new(), node_id: 0 };
        }
        match self.ns {
            Namespace::SingleRoot => {
                let g = match self.grants.iter().next() {
                    Some(g) => g,
                    // A single-root account whose share was revoked has nothing at all, and its
                    // whole namespace is forbidden rather than empty — again so that a checkout
                    // is left alone rather than emptied.
                    None => return Resolved::Forbidden { alias: String::new(), why: Denial::Revoked },
                };
                if let Some(why) = g.denied() {
                    return Resolved::Forbidden { alias: String::new(), why };
                }
                let store_path = if p == "/" { g.store_path.clone() } else { format!("{}{}", g.store_path, p) };
                Resolved::In { store_path, rights: g.rights, alias: String::new(), node_id: g.node_id }
            }
            Namespace::Aliased => {
                if p == "/" {
                    return Resolved::Root;
                }
                let rest = &p[1..];
                let (alias, tail) = match rest.split_once('/') {
                    Some((a, t)) => (a, Some(t)),
                    None => (rest, None),
                };
                let g = match self.grants.by_alias(alias) {
                    Some(g) => g,
                    None => return Resolved::NotFound,
                };
                if let Some(why) = g.denied() {
                    return Resolved::Forbidden { alias: alias.to_string(), why };
                }
                let store_path = match tail {
                    None => g.store_path.clone(),
                    Some(t) => format!("{}/{}", g.store_path, t),
                };
                Resolved::In { store_path, rights: g.rights, alias: alias.to_string(), node_id: g.node_id }
            }
        }
    }

    /// The path this account sees a store path as, or `None` when it sees nothing there.
    ///
    /// The inverse of [`View::to_store`] over the visible set. Used on every row on the way out,
    /// so it is a prefix test against a handful of grants and a string join.
    pub fn to_view(&self, store_path: &str) -> Option<String> {
        if self.is_admin() {
            return Some(store_path.to_string());
        }
        let g = self.grant_for(store_path)?;
        if !g.live() {
            return None;
        }
        let tail = &store_path[g.store_path.len()..];
        Some(match self.ns {
            Namespace::SingleRoot => {
                if tail.is_empty() {
                    "/".to_string()
                } else {
                    tail.to_string()
                }
            }
            Namespace::Aliased => format!("/{}{}", g.alias, tail),
        })
    }

    /// The live grant whose subtree holds this store path.
    ///
    /// Overlapping grants are refused at grant time, so at most one matches and the first hit is
    /// the answer — no longest-prefix search.
    pub fn grant_for(&self, store_path: &str) -> Option<&Grant> {
        self.grants.iter().find(|g| contains(&g.store_path, store_path))
    }

    /// The share whose root this store path *is*, if it is one.
    ///
    /// A share root is a folder the account works inside and does not own: moving or deleting it
    /// is the owner's to do, and the account's alias is its name for the share rather than for
    /// the folder. Writing inside it is ordinary.
    pub fn share_root_of(&self, store_path: &str) -> Option<&Grant> {
        self.grants.iter().find(|g| g.store_path == store_path && g.live())
    }

    /// Can this account see this store path at all?
    pub fn can_see(&self, store_path: &str) -> bool {
        self.is_admin() || self.grant_for(store_path).is_some_and(Grant::live)
    }

    /// Resolve for an operation that writes, so that `ro` is refused here rather than by whatever
    /// the caller would have done next.
    pub fn to_store_rw(&self, view_path: &str) -> Resolved {
        match self.to_store(view_path) {
            Resolved::In { rights: Rights::Ro, alias, .. } => Resolved::Forbidden { alias, why: Denial::ReadOnly },
            other => other,
        }
    }

    /// The store-path subtrees this account can see, for a query that needs to restrict rows
    /// rather than test one path: the visible set is the union of the live grants' subtrees.
    ///
    /// `None` for the admin, meaning "everything" — a caller adds no predicate at all, which is
    /// what keeps today's queries exactly as they are.
    pub fn visible_roots(&self) -> Option<Vec<&str>> {
        if self.is_admin() {
            return None;
        }
        Some(self.grants.live().map(|g| g.store_path.as_str()).collect())
    }

    /// Which of this account's subtrees a query under `view_prefix` must cover.
    ///
    /// `ls -R /` or `search` with no prefix is over every share; under an alias it is that one
    /// share's subtree. The point of returning a list is that a caller builds one predicate and
    /// never has to know which case it was.
    pub fn search_roots(&self, view_prefix: &str) -> Result<Vec<String>, Resolved> {
        if self.is_admin() {
            return Ok(vec![view_prefix.to_string()]);
        }
        match self.to_store(view_prefix) {
            Resolved::Root => Ok(self.grants.live().map(|g| g.store_path.clone()).collect()),
            Resolved::In { store_path, .. } => Ok(vec![store_path]),
            other => Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> View {
        let mut g = Grants::new();
        g.add(Namespace::Aliased, 1, "/legal/contracts", Some("contracts"), Rights::Rw).unwrap();
        g.add(Namespace::Aliased, 2, "/products", None, Rights::Ro).unwrap();
        View::account("accounts-agent", Namespace::Aliased, g)
    }

    fn single_root() -> View {
        let mut g = Grants::new();
        g.add(Namespace::SingleRoot, 1, "/legal/contracts", None, Rights::Rw).unwrap();
        View::account("contracts-agent", Namespace::SingleRoot, g)
    }

    #[test]
    fn an_alias_is_a_prefix_swap() {
        let v = fixture();
        assert_eq!(v.to_view("/legal/contracts/acme.md").as_deref(), Some("/contracts/acme.md"));
        assert_eq!(v.to_view("/legal/contracts/2026/q3.md").as_deref(), Some("/contracts/2026/q3.md"));
        assert_eq!(v.to_view("/products/catalog/x.md").as_deref(), Some("/products/catalog/x.md"));
        // Nothing else is visible, and the share's own ancestors least of all.
        assert_eq!(v.to_view("/hr/salaries.md"), None);
        assert_eq!(v.to_view("/legal/policies/nda.md"), None);
        assert_eq!(v.to_view("/legal"), None);
    }

    #[test]
    fn a_view_path_round_trips() {
        let v = fixture();
        for p in ["/contracts", "/contracts/acme.md", "/contracts/2026/q3.md", "/products/catalog/x.md"] {
            let store = match v.to_store(p) {
                Resolved::In { store_path, .. } => store_path,
                other => panic!("{p}: {other:?}"),
            };
            assert_eq!(v.to_view(&store).as_deref(), Some(p), "{p} -> {store} -> ?");
        }
    }

    #[test]
    fn a_store_path_is_not_a_view_path() {
        let v = fixture();
        // The account's own view has no /legal, so quoting a store path finds nothing — and says
        // exactly what it says for a folder that never existed.
        assert_eq!(v.to_store("/legal/contracts/acme.md"), Resolved::NotFound);
        assert_eq!(v.to_store("/nowhere/at/all"), Resolved::NotFound);
        assert_eq!(v.to_store("/hr"), Resolved::NotFound);
    }

    #[test]
    fn the_root_is_the_list_of_shares_and_not_a_folder() {
        assert_eq!(fixture().to_store("/"), Resolved::Root);
        // A single-root account's root is its share, so it is a folder like any other.
        assert!(matches!(single_root().to_store("/"), Resolved::In { .. }));
    }

    #[test]
    fn a_single_root_account_sees_the_share_at_its_root() {
        let v = single_root();
        assert_eq!(v.to_view("/legal/contracts/acme.md").as_deref(), Some("/acme.md"));
        assert_eq!(v.to_view("/legal/contracts").as_deref(), Some("/"));
        assert_eq!(v.to_view("/legal/policies/nda.md"), None);
        match v.to_store("/2026/q3.md") {
            Resolved::In { store_path, .. } => assert_eq!(store_path, "/legal/contracts/2026/q3.md"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rights_are_per_share() {
        let v = fixture();
        assert!(matches!(v.to_store_rw("/contracts/acme.md"), Resolved::In { rights: Rights::Rw, .. }));
        assert_eq!(
            v.to_store_rw("/products/roadmap.md"),
            Resolved::Forbidden { alias: "products".into(), why: Denial::ReadOnly }
        );
        // Reading it is fine.
        assert!(matches!(v.to_store("/products/roadmap.md"), Resolved::In { rights: Rights::Ro, .. }));
    }

    #[test]
    fn a_dormant_share_is_forbidden_rather_than_absent() {
        let mut g = Grants::new();
        g.add(Namespace::Aliased, 1, "/legal/contracts", None, Rights::Rw).unwrap();
        let grants = Grants::from_rows(g.iter().cloned().map(|mut g| {
            g.dormant = true;
            g
        }));
        assert_eq!(grants.len(), 1);
        let v = View::account("a", Namespace::Aliased, grants.clone());
        assert_eq!(
            v.to_store("/contracts/acme.md"),
            Resolved::Forbidden { alias: "contracts".into(), why: Denial::Dormant }
        );
        // And it is not listed at the root, though it is still a grant.
        assert_eq!(v.grants().live().count(), 0);
        // Revoking is the same answer for the same reason: the account may still have those
        // files on disk, so it is told it may not have them, never that they are not there.
        let mut revoked = Grants::from_rows(g.iter().cloned().map(|mut g| {
            g.revoked = true;
            g
        }));
        assert_eq!(
            View::account("a", Namespace::Aliased, revoked.clone()).to_store("/contracts/acme.md"),
            Resolved::Forbidden { alias: "contracts".into(), why: Denial::Revoked }
        );
        // Dropping the row outright — which nothing does — is the only way to get not-found.
        revoked.revoke("contracts");
        assert_eq!(View::account("a", Namespace::Aliased, revoked).to_store("/contracts/acme.md"), Resolved::NotFound);
    }

    #[test]
    fn a_revoked_share_keeps_its_alias_reserved() {
        let mut g = Grants::new();
        g.add(Namespace::Aliased, 1, "/legal/contracts", None, Rights::Rw).unwrap();
        let mut g = Grants::from_rows(g.iter().cloned().map(|mut g| {
            g.revoked = true;
            g
        }));
        // Handing the same name to a different folder would make the account's next sync write
        // one share's content over the other's, on a disk that still holds the first.
        let e = g.add(Namespace::Aliased, 2, "/vendor/contracts", Some("contracts"), Rights::Ro).unwrap_err();
        assert!(matches!(e, GrantError::AliasTaken { .. }), "{e:?}");
        // Granting the same folder again is how it comes back.
        let back = g.add(Namespace::Aliased, 1, "/legal/contracts", Some("contracts"), Rights::Ro).unwrap();
        assert!(back.live());
        assert_eq!(back.rights, Rights::Ro);
    }

    #[test]
    fn an_alias_collision_is_refused_rather_than_suffixed() {
        let mut g = Grants::new();
        g.add(Namespace::Aliased, 1, "/legal/contracts", None, Rights::Rw).unwrap();
        let e = g.add(Namespace::Aliased, 2, "/vendor/contracts", None, Rights::Ro).unwrap_err();
        assert_eq!(e, GrantError::AliasTaken { alias: "contracts".into(), held_by: "/legal/contracts".into() });
        // With an explicit alias it goes through, and the first share has not moved.
        g.add(Namespace::Aliased, 2, "/vendor/contracts", Some("vendor-contracts"), Rights::Ro).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g.by_alias("contracts").unwrap().store_path, "/legal/contracts");
    }

    #[test]
    fn overlapping_grants_are_refused_in_either_order() {
        for (first, second) in [("/legal", "/legal/contracts"), ("/legal/contracts", "/legal")] {
            let mut g = Grants::new();
            g.add(Namespace::Aliased, 1, first, Some("a"), Rights::Rw).unwrap();
            let e = g.add(Namespace::Aliased, 2, second, Some("b"), Rights::Ro).unwrap_err();
            assert!(matches!(e, GrantError::Overlaps { .. }), "{first} then {second}: {e:?}");
        }
        // A sibling is not an overlap, and neither is a name that merely shares a prefix.
        let mut g = Grants::new();
        g.add(Namespace::Aliased, 1, "/legal/contracts", Some("a"), Rights::Rw).unwrap();
        g.add(Namespace::Aliased, 2, "/legal/contracts-2026", Some("b"), Rights::Ro).unwrap();
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn a_single_root_account_holds_one_share() {
        let mut g = Grants::new();
        g.add(Namespace::SingleRoot, 1, "/legal/contracts", None, Rights::Rw).unwrap();
        let e = g.add(Namespace::SingleRoot, 2, "/products", None, Rights::Ro).unwrap_err();
        assert_eq!(e, GrantError::SingleRoot { root: "/legal/contracts".into() });
    }

    #[test]
    fn the_store_root_cannot_be_a_share() {
        let mut g = Grants::new();
        assert!(matches!(g.add(Namespace::Aliased, 1, "/", None, Rights::Rw), Err(GrantError::BadRoot { .. })));
    }

    #[test]
    fn renaming_an_alias_moves_the_share_and_keeps_the_node() {
        let mut g = Grants::new();
        g.add(Namespace::Aliased, 1, "/legal/contracts", None, Rights::Rw).unwrap();
        let moved = g.rename("contracts", "legal-contracts").unwrap();
        assert_eq!((moved.node_id, moved.store_path.as_str()), (1, "/legal/contracts"));
        let v = View::account("a", Namespace::Aliased, g);
        assert_eq!(v.to_view("/legal/contracts/acme.md").as_deref(), Some("/legal-contracts/acme.md"));
        assert_eq!(v.to_store("/contracts/acme.md"), Resolved::NotFound);
    }

    #[test]
    fn an_alias_that_is_also_a_folder_name_below_the_share_is_not_ambiguous() {
        // §2 rule 7: the alias is always the first segment, so /contracts/contracts/x is the
        // nested folder and there is no special case.
        let mut g = Grants::new();
        g.add(Namespace::Aliased, 1, "/legal/contracts", None, Rights::Rw).unwrap();
        let v = View::account("a", Namespace::Aliased, g);
        match v.to_store("/contracts/contracts/x.md") {
            Resolved::In { store_path, .. } => assert_eq!(store_path, "/legal/contracts/contracts/x.md"),
            other => panic!("{other:?}"),
        }
        assert_eq!(v.to_view("/legal/contracts/contracts/x.md").as_deref(), Some("/contracts/contracts/x.md"));
    }

    #[test]
    fn admin_translates_nothing() {
        let v = View::admin();
        assert_eq!(v.to_view("/hr/salaries.md").as_deref(), Some("/hr/salaries.md"));
        assert!(v.can_see("/anything"));
        assert!(v.visible_roots().is_none());
        assert!(matches!(v.to_store_rw("/hr/salaries.md"), Resolved::In { rights: Rights::Rw, .. }));
    }

    #[test]
    fn search_roots_cover_the_whole_view_or_one_share() {
        let v = fixture();
        assert_eq!(v.search_roots("/").unwrap(), vec!["/legal/contracts".to_string(), "/products".to_string()]);
        assert_eq!(v.search_roots("/products/catalog").unwrap(), vec!["/products/catalog".to_string()]);
        assert!(matches!(v.search_roots("/hr"), Err(Resolved::NotFound)));
        // The admin's prefix is its own, untouched.
        assert_eq!(View::admin().search_roots("/hr").unwrap(), vec!["/hr".to_string()]);
    }

    #[test]
    fn contains_is_a_subtree_test_and_not_a_string_prefix() {
        assert!(contains("/legal", "/legal"));
        assert!(contains("/legal", "/legal/contracts"));
        assert!(!contains("/legal", "/legality"));
        assert!(!contains("/legal/contracts", "/legal"));
    }

    // ------------------------------------------------------------------ the laws, over any view

    use proptest::prelude::*;

    fn seg() -> impl Strategy<Value = String> {
        "[a-z]{1,4}".prop_map(|s| s.to_string())
    }

    /// A view with up to four non-overlapping shares at random depths, built through `add` so it
    /// obeys every rule a real one does.
    fn any_view() -> impl Strategy<Value = View> {
        proptest::collection::vec((seg(), seg(), any::<bool>()), 1..5).prop_map(|specs| {
            let mut g = Grants::new();
            for (i, (a, b, rw)) in specs.into_iter().enumerate() {
                let rights = if rw { Rights::Rw } else { Rights::Ro };
                // Distinct roots by construction, so only the alias rule can bite.
                let path = format!("/{a}{i}/{b}");
                let _ = g.add(Namespace::Aliased, i as i64 + 1, &path, Some(&format!("{a}{i}")), rights);
            }
            View::account("a", Namespace::Aliased, g)
        })
    }

    proptest! {
        /// Every path the account can see round-trips, in both directions.
        #[test]
        fn to_view_inverts_to_store(v in any_view(), tail in proptest::collection::vec(seg(), 0..4)) {
            for g in v.grants().iter().cloned().collect::<Vec<_>>() {
                let store = if tail.is_empty() { g.store_path.clone() } else { format!("{}/{}", g.store_path, tail.join("/")) };
                let view = v.to_view(&store).expect("visible");
                match v.to_store(&view) {
                    Resolved::In { store_path, rights, .. } => {
                        prop_assert_eq!(&store_path, &store);
                        prop_assert_eq!(rights, g.rights);
                    }
                    other => prop_assert!(false, "{:?} for {}", other, view),
                }
            }
        }

        /// A path is never visible through more than one share, so a file has exactly one path in
        /// a view. This is what the overlap rule buys.
        #[test]
        fn a_store_path_has_at_most_one_view_path(v in any_view(), tail in proptest::collection::vec(seg(), 0..3)) {
            for g in v.grants().iter().cloned().collect::<Vec<_>>() {
                let store = format!("{}/{}", g.store_path, tail.join("/"));
                let hits = v.grants().iter().filter(|o| contains(&o.store_path, &store)).count();
                prop_assert_eq!(hits, 1, "{} matched {} shares", store, hits);
            }
        }

        /// Nothing outside the shares is ever reachable, and a path that is not reachable is
        /// `NotFound` rather than anything that distinguishes it from a path that never existed.
        #[test]
        fn nothing_outside_the_shares_resolves(v in any_view(), p in proptest::collection::vec(seg(), 1..4)) {
            let store = format!("/{}", p.join("/"));
            if v.grant_for(&store).is_none() {
                prop_assert_eq!(v.to_view(&store), None);
            }
            let view_path = format!("/{}", p.join("/"));
            let first = view_path[1..].split('/').next().unwrap_or_default();
            if v.grants().by_alias(first).is_none() {
                prop_assert_eq!(v.to_store(&view_path), Resolved::NotFound);
            }
        }

        /// A write is refused on anything the account cannot write, and never silently downgraded.
        #[test]
        fn to_store_rw_never_returns_ro(v in any_view(), tail in proptest::collection::vec(seg(), 0..3)) {
            for g in v.grants().iter().cloned().collect::<Vec<_>>() {
                let view = v.to_view(&format!("{}/{}", g.store_path, tail.join("/"))).expect("visible");
                match v.to_store_rw(&view) {
                    Resolved::In { rights, .. } => prop_assert_eq!(rights, Rights::Rw),
                    Resolved::Forbidden { why, .. } => prop_assert_eq!(why, Denial::ReadOnly),
                    other => prop_assert!(false, "{:?}", other),
                }
            }
        }
    }
}
