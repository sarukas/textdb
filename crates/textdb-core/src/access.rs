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

// ---------------------------------------------------------------- link projection (#12 part 2)

/// One link's target and where it is written, as the projector needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSpan {
    /// The target exactly as written, without `#anchor` or `|alias`.
    pub target: String,
    /// Byte range of that text inside the document.
    pub from: usize,
    pub to: usize,
    /// `wiki`, `embed`, `md` or `image`.
    pub kind: String,
    /// The store path of the document it resolved to, if it resolved to one.
    pub resolved: Option<String>,
    /// The resolved node's id, for the hidden-target form.
    pub resolved_id: Option<i64>,
    /// The display text a wiki link was written with (`[[nda|the NDA]]`), when it has one.
    ///
    /// Only the hidden form needs it: `[[textdb:8]]` renders as an opaque number where `[[nda]]`
    /// rendered as a word, so the projector supplies the name the link was written with as the
    /// alias. A link that already has one keeps it.
    pub alias: Option<String>,
}

/// What a projection did to one link, so a caller can report it without re-deriving it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Projected {
    /// Left exactly as written.
    Same,
    /// Rewritten to the reader's own path for the target.
    Path,
    /// Rewritten to `textdb:<id>`, because the reader cannot see the target.
    Hidden,
}

/// Does this target text name its resolved document as a path from the vault root?
///
/// This is the whole of "`match = root`" from the design, decided from what is already stored
/// rather than recorded at index time: the link text, and the store path it reached. A target
/// that got there by file name (`[[q3]]`), by suffix (`[[2026/q3]]` matching deeper), or
/// relatively (`../policies/nda.md`) is **not** a root path — it means the same thing in every
/// namespace and must stay byte-identical.
pub fn is_root_link(kind: &str, target: &str, resolved: &str) -> bool {
    let md = kind == "md" || kind == "image";
    if md && !target.starts_with('/') {
        return false;
    }
    if !md && !target.contains('/') {
        return false;
    }
    let want = format!("/{}", target.trim_start_matches('/'));
    let resolved = resolved.strip_suffix(".tdbasset").unwrap_or(resolved);
    // `.md` is optional in both link forms, so both spellings name the same document.
    want == resolved || format!("{want}.md") == resolved
}

/// Is this target written as a position rather than as a name or a root path?
///
/// A relative markdown target (`../policies/nda.md`) and a wiki target that matched deeper
/// (`[[2026/q3]]`) both mean "from here", which is the same document in every namespace.
fn is_positional(kind: &str, target: &str, resolved: &str) -> bool {
    let md = kind == "md" || kind == "image";
    if md {
        return !target.starts_with('/');
    }
    target.contains('/') && !is_root_link(kind, target, resolved)
}

/// The word a link target reads as: its last segment, without `.md`.
fn display_name(target: &str) -> String {
    let last = target.trim_end_matches('/').rsplit('/').next().unwrap_or(target);
    last.strip_suffix(".md").unwrap_or(last).to_string()
}

/// Write `store_path` the way `target` was written: `.md` kept only if it was there, and a wiki
/// target without its leading slash, as Obsidian writes a vault path.
fn like(target: &str, store_path: &str, md: bool) -> String {
    let kept = target.to_ascii_lowercase().ends_with(".md");
    let p = if kept { store_path.to_string() } else { store_path.strip_suffix(".md").unwrap_or(store_path).to_string() };
    if md {
        p
    } else {
        p.trim_start_matches('/').to_string()
    }
}

/// Rewrite a document's root-absolute link targets into `view`'s own paths.
///
/// Spans only: nothing here looks at the text between links, so code spans, fenced blocks,
/// escaped brackets and URLs are untouched by construction. Rewriting never adds or removes a
/// newline, so **line numbers are identical** in canonical and projected text — which is what
/// lets `cat -n`, `replace-lines`, `hunks` and search line numbers mean the same thing in every
/// view.
///
/// Returns the text and what happened to each link, in the order given.
pub fn project(view: &View, text: &[u8], links: &[TargetSpan]) -> (Vec<u8>, Vec<Projected>) {
    let mut acts = vec![Projected::Same; links.len()];
    if view.is_admin() || links.is_empty() {
        return (text.to_vec(), acts);
    }
    // Back to front, so an earlier span's offsets are still right after a later one is replaced.
    let mut order: Vec<usize> = (0..links.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(links[i].from));
    let mut out = text.to_vec();
    for i in order {
        let l = &links[i];
        if l.from > l.to || l.to > out.len() {
            continue;
        }
        let md = l.kind == "md" || l.kind == "image";
        let Some(resolved) = l.resolved.as_deref() else {
            // Nothing in the store to resolve against, so the target is taken at face value. A
            // root path is namespace-dependent whether or not it names a document that exists —
            // `[[legal/contracts/2026/q4]]` is what this account calls `contracts/2026/q4` — and
            // leaving it alone would put the store's layout in text the account may edit. One
            // that names nothing it can see stays exactly as written.
            let looks_root = if md { l.target.starts_with('/') } else { l.target.contains('/') };
            if !looks_root {
                continue;
            }
            let store = format!("/{}", l.target.trim_start_matches('/'));
            let Some(seen) = view.to_view(&store) else { continue };
            acts[i] = Projected::Path;
            out.splice(l.from..l.to, like(&l.target, &seen, md).into_bytes());
            continue;
        };
        let replacement = match view.to_view(resolved) {
            // Visible: only a root path is namespace-dependent; everything else already means
            // the same thing here as it does centrally.
            Some(seen) if is_root_link(&l.kind, &l.target, resolved) => {
                acts[i] = Projected::Path;
                like(&l.target, &seen, md)
            }
            Some(_) => continue,
            // Hidden: whatever form it was written in, the reader learns only that there is a
            // document and cannot see it. `textdb:13` is not a path and cannot collide with one.
            None => {
                let Some(id) = l.resolved_id else { continue };
                // A target written as a position — a relative path, or a wiki path matching
                // deeper — means the same thing in every namespace and is the author's own
                // text, so it keeps its bytes even when what it reaches is hidden. Only a name
                // or a root path is rewritten: a name would read as broken here when it is not,
                // and a root path is the store's layout (#12 E5, E14, E15).
                if is_positional(&l.kind, &l.target, resolved) {
                    continue;
                }
                acts[i] = Projected::Hidden;
                // A wiki link renders its target, so an id on its own would read as a number
                // where a word was. The name the link was written with becomes the alias — it is
                // the author's own text, and it says nothing about where the document lives.
                match (md, l.alias.as_deref()) {
                    (false, None) => format!("textdb:{id}|{}", display_name(&l.target)),
                    _ => format!("textdb:{id}"),
                }
            }
        };
        out.splice(l.from..l.to, replacement.into_bytes());
    }
    (out, acts)
}

/// The inverse: a document written in `view`'s paths, as the store holds it.
///
/// `resolve` is how the caller turns a store path into the node it names, so an id reference can
/// be written back as that node's *current* path — which is why an id link survives a move that
/// happened while the reader had the file open.
pub fn unproject(view: &View, text: &[u8], links: &[TargetSpan], path_of_id: impl Fn(i64) -> Option<String>) -> Vec<u8> {
    if view.is_admin() || links.is_empty() {
        return text.to_vec();
    }
    let mut order: Vec<usize> = (0..links.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(links[i].from));
    let mut out = text.to_vec();
    for i in order {
        let l = &links[i];
        if l.from > l.to || l.to > out.len() {
            continue;
        }
        let md = l.kind == "md" || l.kind == "image";
        let replacement = match l.target.strip_prefix("textdb:") {
            // An id reference: back to wherever that document is *now*, which is why an id
            // link survives a move made while the reader had the file open.
            //
            // `textdb:13` says nothing about how the path was spelled, so the spelling comes
            // from the link form: a markdown target is a path and keeps its extension, a wiki
            // target is a vault path and drops `.md`, as Obsidian writes them.
            Some(n) => match n.trim().parse::<i64>().ok().and_then(&path_of_id) {
                Some(p) if md => p,
                Some(p) => p.strip_suffix(".md").unwrap_or(&p).trim_start_matches('/').to_string(),
                None => continue,
            },
            None => {
                // A root path in the account's namespace becomes the store's. One that names
                // nothing it can see is left exactly as written: there is no document to point
                // at, and the text is the author's (#12 §3.4).
                let looks_root = if md { l.target.starts_with('/') } else { l.target.contains('/') };
                if !looks_root {
                    continue;
                }
                let local = format!("/{}", l.target.trim_start_matches('/'));
                match to_store_path(view, &local) {
                    Some(store) => like(&l.target, &store, md),
                    None => continue,
                }
            }
        };
        out.splice(l.from..l.to, replacement.into_bytes());
    }
    out
}

/// A view path to a store path, for un-projection: `None` when the account cannot address it.
///
/// Unlike [`View::to_store`] this never refuses — a link to nothing is not an error, it is a
/// broken link, and the writer's bytes are kept as they are.
fn to_store_path(view: &View, local: &str) -> Option<String> {
    match view.to_store(local) {
        Resolved::In { store_path, .. } => Some(store_path),
        _ => None,
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    fn view() -> View {
        let mut g = Grants::new();
        g.add(Namespace::Aliased, 1, "/legal/contracts", Some("contracts"), Rights::Rw).unwrap();
        View::account("accounts-agent", Namespace::Aliased, g)
    }

    fn span(text: &str, target: &str, kind: &str, resolved: Option<&str>, id: Option<i64>) -> TargetSpan {
        let from = text.find(target).expect("target in text");
        TargetSpan {
            target: target.to_string(),
            from,
            to: from + target.len(),
            kind: kind.to_string(),
            resolved: resolved.map(str::to_string),
            resolved_id: id,
            alias: None,
        }
    }

    #[test]
    fn a_root_link_is_rewritten_to_the_readers_path() {
        let t = "see [q3](/legal/contracts/2026/q3.md)\n";
        let l = [span(t, "/legal/contracts/2026/q3.md", "md", Some("/legal/contracts/2026/q3.md"), Some(9))];
        let (out, acts) = project(&view(), t.as_bytes(), &l);
        assert_eq!(String::from_utf8(out).unwrap(), "see [q3](/contracts/2026/q3.md)\n");
        assert_eq!(acts, [Projected::Path]);
    }

    #[test]
    fn a_hidden_target_becomes_an_id_and_names_no_path() {
        let t = "see [nda](/legal/policies/nda.md)\n";
        let l = [span(t, "/legal/policies/nda.md", "md", Some("/legal/policies/nda.md"), Some(13))];
        let (out, acts) = project(&view(), t.as_bytes(), &l);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out, "see [nda](textdb:13)\n");
        assert!(!out.contains("policies"));
        assert_eq!(acts, [Projected::Hidden]);
    }

    #[test]
    fn everything_that_is_not_a_root_path_keeps_its_bytes() {
        let v = view();
        // A name link, a relative link, and a suffix match: each means the same in every
        // namespace, so none is touched.
        for (t, target, kind, resolved) in [
            ("see [[q3]]\n", "q3", "wiki", "/legal/contracts/2026/q3.md"),
            ("see [q3](2026/q3.md)\n", "2026/q3.md", "md", "/legal/contracts/2026/q3.md"),
            ("see [[2026/q3]]\n", "2026/q3", "wiki", "/legal/contracts/2026/q3.md"),
        ] {
            let l = [span(t, target, kind, Some(resolved), Some(9))];
            let (out, acts) = project(&v, t.as_bytes(), &l);
            assert_eq!(String::from_utf8(out).unwrap(), t, "{target}");
            assert_eq!(acts, [Projected::Same], "{target}");
        }
    }

    #[test]
    fn projection_and_un_projection_are_inverse() {
        let v = view();
        let canonical = "a [q3](/legal/contracts/2026/q3.md) and [[legal/contracts/acme]]\n";
        let l = [
            span(canonical, "/legal/contracts/2026/q3.md", "md", Some("/legal/contracts/2026/q3.md"), Some(9)),
            span(canonical, "legal/contracts/acme", "wiki", Some("/legal/contracts/acme.md"), Some(4)),
        ];
        let (projected, _) = project(&v, canonical.as_bytes(), &l);
        let projected = String::from_utf8(projected).unwrap();
        assert_eq!(projected, "a [q3](/contracts/2026/q3.md) and [[contracts/acme]]\n");
        // Re-scanned in the projected text, as a real save would.
        let back = [
            span(&projected, "/contracts/2026/q3.md", "md", None, None),
            span(&projected, "contracts/acme", "wiki", None, None),
        ];
        let out = unproject(&v, projected.as_bytes(), &back, |_| None);
        assert_eq!(String::from_utf8(out).unwrap(), canonical);
    }

    #[test]
    fn an_id_reference_un_projects_to_wherever_the_document_is_now() {
        let v = view();
        let t = "see [nda](textdb:13)\n";
        let l = [span(t, "textdb:13", "md", None, None)];
        let out = unproject(&v, t.as_bytes(), &l, |id| (id == 13).then(|| "/law/policies/nda.md".to_string()));
        assert_eq!(String::from_utf8(out).unwrap(), "see [nda](/law/policies/nda.md)\n");
    }

    #[test]
    fn a_local_link_to_nothing_is_stored_as_written() {
        let v = view();
        let t = "see [[hr/salaries]]\n";
        let l = [span(t, "hr/salaries", "wiki", None, None)];
        assert_eq!(String::from_utf8(unproject(&v, t.as_bytes(), &l, |_| None)).unwrap(), t);
    }

    #[test]
    fn two_links_on_one_line_both_move() {
        let v = view();
        let t = "[a](/legal/contracts/a.md) [b](/legal/contracts/b.md)\n";
        let l = [
            span(t, "/legal/contracts/a.md", "md", Some("/legal/contracts/a.md"), Some(1)),
            TargetSpan {
                target: "/legal/contracts/b.md".into(),
                from: t.rfind("/legal/contracts/b.md").unwrap(),
                to: t.rfind("/legal/contracts/b.md").unwrap() + "/legal/contracts/b.md".len(),
                kind: "md".into(),
                resolved: Some("/legal/contracts/b.md".into()),
                resolved_id: Some(2),
                alias: None,
            },
        ];
        let (out, _) = project(&v, t.as_bytes(), &l);
        assert_eq!(String::from_utf8(out).unwrap(), "[a](/contracts/a.md) [b](/contracts/b.md)\n");
    }

    #[test]
    fn line_numbers_never_move() {
        let v = view();
        let t = "one\n[q3](/legal/contracts/2026/q3.md)\nthree\n";
        let l = [span(t, "/legal/contracts/2026/q3.md", "md", Some("/legal/contracts/2026/q3.md"), Some(9))];
        let (out, _) = project(&v, t.as_bytes(), &l);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.lines().count(), t.lines().count());
        assert_eq!(out.lines().nth(2), Some("three"));
    }
}
