//! The acceptance catalogue of issue #12: a central store with folder-scoped delegated access.
//!
//! Every numbered scenario of the catalogue (A1–L5) has a test here, and each runs against both
//! engines — SQLite always, Postgres when `TEXTDB_TEST_PG` is set — because the model is required
//! to behave identically on both (L5). A test names its scenarios in its doc comment and in the
//! `scenarios!` list, which `scenario_coverage_is_complete` checks against the catalogue, so a
//! scenario cannot quietly go missing.
//!
//! These are written before the feature exists. Until it does they fail, which is the point.

use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

// ---------------------------------------------------------------------------- the catalogue

/// Every scenario the issue numbers, in order. A test claims the ones it covers with
/// `scenarios!`, and the coverage test below insists the two agree.
const CATALOGUE: &[&str] = &[
    // A. Roots and listing
    "A1", "A2", "A3", "A4", "A5", "A6", "A7", "A8", "A9", "A10", "A11", "A12", "A13",
    // B. Reading and searching
    "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8", "B9", "B10", "B11", "B12", "B13", "B14",
    "B15", "B16", "B17", "B18", "B19",
    // C. Writing inside shares
    "C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8", "C9", "C10", "C11", "C12", "C13", "C14",
    "C15", "C16", "C17", "C18", "C19", "C20",
    // D. Moves and deletes
    "D1", "D2", "D3", "D4", "D5", "D6", "D7", "D8", "D9", "D10", "D11", "D12", "D13", "D14",
    // E. Links
    "E1", "E2", "E3", "E4", "E5", "E6", "E7", "E8", "E9", "E10", "E11", "E12", "E13", "E14",
    "E15", "E16", "E17", "E18", "E19", "E20", "E21", "E22",
    // F. History, change feed, watch
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11",
    // G. Sync to disk
    "G1", "G2", "G3", "G4", "G5", "G6", "G7", "G8", "G9", "G10", "G11", "G12", "G13", "G14",
    "G15", "G16", "G17",
    // H. Grants, aliases, tokens
    "H1", "H2", "H3", "H4", "H5", "H6", "H7", "H8", "H9", "H10", "H11", "H12", "H13", "H14",
    "H15", "H16", "H17", "H18", "H19", "H20", "H21", "H22", "H23", "H24",
    // I. Single-root accounts
    "I1", "I2", "I3", "I4", "I5", "I6", "I7",
    // J. Cross-view references
    "J1", "J2", "J3", "J4", "J5", "J6",
    // K. SQL and web
    "K1", "K2", "K3", "K4", "K5", "K6", "K7", "K8", "K9",
    // L. Bypass, accepted by design
    "L1", "L2", "L3", "L4", "L5",
];

/// Scenarios this file has no test for, each with the reason. Empty is the goal; anything here
/// is a gap stated out loud rather than one hidden by a missing line in `COVERED`.
const NOT_COVERED: &[(&str, &str)] = &[];

/// What every test claims. `scenarios!` appends to this through the inventory below.
static COVERED: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

/// Claim the catalogue rows a test covers. Called at the top of the test body.
macro_rules! scenarios {
    ($($s:literal),+ $(,)?) => {
        COVERED.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&[$($s),+]);
    };
}

// ---------------------------------------------------------------------------- engines

/// Which store a scenario runs against. Every scenario runs on both (L5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engine {
    Sqlite,
    Postgres,
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Engine::Sqlite => "sqlite",
            Engine::Postgres => "postgres",
        }
    }
}

/// SQLite always; Postgres when a server is configured, as `cli_pg.rs` does it.
fn engines() -> Vec<Engine> {
    let mut all = vec![Engine::Sqlite];
    if std::env::var("TEXTDB_TEST_PG").ok().filter(|u| !u.is_empty()).is_some() {
        all.push(Engine::Postgres);
    }
    all
}

/// A store for one scenario on one engine, removed with the test.
struct Store {
    url: String,
    engine: Engine,
    /// Held so the directory outlives the store.
    _dir: tempfile::TempDir,
    /// Set for Postgres, so the database is dropped.
    pg: Option<(String, String)>,
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Some((admin, name)) = &self.pg {
            if let Ok(mut c) = postgres::Client::connect(admin, postgres::NoTls) {
                let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"));
            }
        }
    }
}

fn store(engine: Engine) -> Store {
    let dir = tempfile::tempdir().unwrap();
    let s = match engine {
        Engine::Sqlite => Store {
            url: dir.path().join("kb.db").to_string_lossy().into_owned(),
            engine,
            _dir: dir,
            pg: None,
        },
        Engine::Postgres => {
            let admin = std::env::var("TEXTDB_TEST_PG").expect("TEXTDB_TEST_PG");
            static N: AtomicUsize = AtomicUsize::new(0);
            // As in cli_pg.rs: CREATE DATABASE copies template1 and fails while another is copying.
            static CREATING: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let name = format!("textdb_acl_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst));
            let guard = CREATING.lock().unwrap_or_else(|e| e.into_inner());
            let mut c = postgres::Client::connect(&admin, postgres::NoTls).expect("connect to TEXTDB_TEST_PG");
            c.batch_execute(&format!("CREATE DATABASE {name}")).expect("create a test database");
            drop(c);
            drop(guard);
            let base = admin.rsplit_once('/').map_or(admin.as_str(), |(base, _)| base);
            let url = format!("{base}/{name}");
            Store { url, engine, _dir: dir, pg: Some((admin, name)) }
        }
    };
    ok(cmd(&s, None).arg("init"), None);
    s
}

// ---------------------------------------------------------------------------- running the CLI

struct Output {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Output {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}\n{}", self.stdout, self.stderr))
    }
}

/// A command against `store`, as `token`'s account — or as the owner opening the store directly
/// when `token` is `None`, which is what the catalogue calls admin.
fn cmd(store: &Store, token: Option<&str>) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_textdb"));
    c.env_remove("TEXTDB_STORE")
        .env_remove("TEXTDB_AUTHOR")
        .env_remove("TEXTDB_PATH_HISTORY")
        .env_remove("TEXTDB_TOKEN")
        .env("TEXTDB_CONFIG_DIR", std::env::temp_dir().join(format!("textdb-acl-{}", std::process::id())))
        .env("TEXTDB_CEILING_DIRECTORIES", std::env::current_dir().unwrap_or_default())
        .arg("--store")
        .arg(&store.url);
    if let Some(t) = token {
        c.env("TEXTDB_TOKEN", t);
    }
    c
}

fn run(c: &mut Command, stdin: Option<&str>) -> Output {
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().expect("spawn textdb");
    let mut pipe = child.stdin.take().unwrap();
    if let Some(text) = stdin {
        pipe.write_all(text.as_bytes()).unwrap();
    }
    drop(pipe);
    let o = child.wait_with_output().unwrap();
    Output {
        status: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn ok(c: &mut Command, stdin: Option<&str>) -> Output {
    let o = run(c, stdin);
    assert_eq!(o.status, 0, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    o
}

/// Exit codes the catalogue uses by name.
const NOT_FOUND: i32 = 5;
const INVALID: i32 = 6;
const CONFLICT: i32 = 3;
/// New in #12: exists in your view but you may not do this.
const FORBIDDEN: i32 = 7;

/// Assert a command failed with `code` and said `code_name` (`TX003`, `TX005`, …).
#[track_caller]
fn refused(o: &Output, code: i32, code_name: &str) {
    assert_eq!(o.status, code, "expected {code_name} (exit {code})\nstdout: {}\nstderr: {}", o.stdout, o.stderr);
    assert!(
        o.stderr.contains(code_name) || o.stdout.contains(code_name),
        "expected {code_name} in the message\nstdout: {}\nstderr: {}",
        o.stdout,
        o.stderr
    );
}

// ---------------------------------------------------------------------------- the fixture

/// The catalogue's fixture: the store contents, the accounts and the grants it names.
struct Fixture {
    store: Store,
    /// One bearer per account, by account name.
    tokens: std::collections::BTreeMap<String, String>,
}

impl Fixture {
    /// A command as `account`, or as admin for `"admin"`.
    fn as_(&self, account: &str) -> Command {
        match account {
            "admin" => cmd(&self.store, None),
            name => cmd(&self.store, Some(self.tokens.get(name).unwrap_or_else(|| panic!("no token for {name}")))),
        }
    }

    /// The store's node id for a central path, read as admin.
    fn id(&self, store_path: &str) -> i64 {
        ok(self.as_("admin").args(["--json", "stat", store_path]), None).json()["id"].as_i64().expect("id")
    }

    fn engine(&self) -> Engine {
        self.store.engine
    }
}

/// Build the catalogue's fixture on `engine`.
///
/// Contents, accounts and grants are exactly the table in Part 3; the ids the catalogue writes
/// (11 for acme.md and so on) are illustrative, so tests ask the store for them.
fn fixture(engine: Engine) -> Fixture {
    let store = store(engine);
    let admin = |args: &[&str], stdin: Option<&str>| {
        ok(cmd(&store, None).args(args), stdin);
    };

    // Store contents, as the central view.
    admin(&["write", "/legal/contracts/acme.md"], Some("# Acme\n\nSee [[q3]], [[nda]] and [[beta]].\nAlso [2026](2026/q3.md).\n"));
    admin(&["write", "/legal/contracts/acme.md"], Some("# Acme\n\nSee [[q3]], [[nda]] and [[beta]].\nAlso [2026](2026/q3.md).\nv2\n"));
    admin(&["write", "/legal/contracts/acme.md"], Some("# Acme\n\nSee [[q3]], [[nda]] and [[beta]].\nAlso [2026](2026/q3.md).\nv3\n"));
    admin(&["write", "/legal/contracts/2026/q3.md"], Some("# Q3\n\n## Terms\n\nquarterly review\n"));
    admin(&["write", "/legal/policies/nda.md"], Some("# NDA\n\nconfidential\n"));
    admin(&["write", "/products/catalog/x.md"], Some("# X\n\ncatalog entry\n"));
    admin(&["write", "/products/roadmap.md"], Some("# Roadmap\n\nacme integration\n"));
    admin(&["write", "/hr/salaries.md"], Some("# Salaries\n\nsecret\n"));
    admin(&["write", "/vendor/contracts/beta.md"], Some("# Beta\n\nvendor\n"));

    // Accounts, then grants, then one token each.
    admin(&["account", "create", "accounts-agent", "--kind", "agent"], None);
    admin(&["account", "create", "product-agent", "--kind", "agent"], None);
    admin(&["account", "create", "auditor", "--kind", "person"], None);
    admin(&["account", "create", "contracts-agent", "--kind", "agent", "--root", "/legal/contracts"], None);

    admin(&["access", "grant", "accounts-agent", "/legal/contracts", "rw", "--as", "contracts"], None);
    admin(&["access", "grant", "accounts-agent", "/products", "ro", "--as", "products"], None);
    admin(&["access", "grant", "product-agent", "/products", "rw", "--as", "products"], None);
    admin(&["access", "grant", "auditor", "/legal", "ro", "--as", "legal"], None);

    let mut tokens = std::collections::BTreeMap::new();
    for name in ["accounts-agent", "product-agent", "auditor", "contracts-agent"] {
        let out = ok(cmd(&store, None).args(["--json", "token", "create", name]), None).json();
        let bearer = out["bearer"].as_str().expect("the bearer, printed once").to_string();
        tokens.insert(name.to_string(), bearer);
    }
    Fixture { store, tokens }
}

/// Run `body` against every engine, naming the engine when it fails.
fn on_each_engine(body: impl Fn(&Fixture)) {
    for engine in engines() {
        let f = fixture(engine);
        println!("--- {} ---", engine.name());
        body(&f);
    }
}

// ---------------------------------------------------------------------------- coverage

/// Every catalogue row has a test, or an entry in `NOT_COVERED` saying why not.
///
/// Runs last by name; the scenario tests fill `COVERED` as they run. `cargo test` runs the whole
/// file in one process, so the list is complete by the time this compares it — unless a test was
/// filtered out, which is why it only reports a gap when the whole file ran.
#[test]
fn zz_scenario_coverage_is_complete() {
    if std::env::args().any(|a| a.starts_with("--exact") || a == "zz_scenario_coverage_is_complete") {
        return; // filtered run: the other tests did not fill COVERED
    }
    let covered: BTreeSet<&str> = COVERED.lock().unwrap_or_else(|e| e.into_inner()).iter().copied().collect();
    let excused: BTreeSet<&str> = NOT_COVERED.iter().map(|(s, _)| *s).collect();
    let missing: Vec<&str> = CATALOGUE.iter().copied().filter(|s| !covered.contains(s) && !excused.contains(s)).collect();
    assert!(missing.is_empty(), "catalogue rows with no test: {missing:?}");
    let unknown: Vec<&str> = covered.iter().copied().filter(|s| !CATALOGUE.contains(s)).collect();
    assert!(unknown.is_empty(), "tests claim rows the catalogue does not have: {unknown:?}");
}

// ============================================================================ A. Roots and listing

/// A1, A2, A11, A12: each account's root lists its own shares under their aliases, and the
/// central view never shows an alias.
#[test]
fn a_root_lists_the_shares_of_the_account_and_nothing_else() {
    scenarios!("A1", "A2", "A11", "A12");
    on_each_engine(|f| {
        let names = |account: &str| -> Vec<String> {
            ok(f.as_(account).args(["ls", "-1", "/"]), None)
                .stdout
                .lines()
                .map(|l| l.trim_end_matches('/').trim_start_matches('/').to_string())
                .filter(|l| !l.is_empty())
                .collect()
        };
        // A1: admin sees store paths, no aliases.
        assert_eq!(names("admin"), ["hr", "legal", "products", "vendor"], "A1 on {}", f.engine().name());
        // A2: two shares, under their aliases; no `legal`, no `hr`.
        assert_eq!(names("accounts-agent"), ["contracts", "products"], "A2 on {}", f.engine().name());
        // A11: one share.
        assert_eq!(names("product-agent"), ["products"], "A11 on {}", f.engine().name());
        // A12: single-root — the root *is* the share, so its children are at the top.
        assert_eq!(names("contracts-agent"), ["2026", "acme.md"], "A12 on {}", f.engine().name());
    });
}

/// A3, A10: under an alias the subtree is the shared folder's own, and two accounts reach the
/// same files under different aliases.
#[test]
fn a_share_shows_the_shared_folders_children_directly() {
    scenarios!("A3", "A10");
    on_each_engine(|f| {
        let names = |account: &str, path: &str| -> Vec<String> {
            ok(f.as_(account).args(["ls", "-1", path]), None)
                .stdout
                .lines()
                .map(|l| l.rsplit('/').next().unwrap_or_default().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        };
        // A3.
        assert_eq!(names("accounts-agent", "/contracts"), ["2026", "acme.md"]);
        // A10: auditor's share is one level up, so it walks legal/ → contracts/ → the same files.
        assert_eq!(names("auditor", "/legal"), ["contracts", "policies"]);
        assert_eq!(names("auditor", "/legal/contracts"), ["2026", "acme.md"]);
    });
}

/// A4, A5: a store path is *not found* in a local view, whether or not the content below it is
/// visible under another name. Existence outside the view is never disclosed.
#[test]
fn a_store_path_is_not_found_in_a_local_view() {
    scenarios!("A4", "A5");
    on_each_engine(|f| {
        // A4: /legal holds the very files accounts-agent can see as /contracts.
        refused(&run(f.as_("accounts-agent").args(["ls", "/legal"]), None), NOT_FOUND, "TX003");
        // A5: /hr is invisible, and says the same thing, so the two cannot be told apart.
        refused(&run(f.as_("accounts-agent").args(["ls", "/hr"]), None), NOT_FOUND, "TX003");
        let a = run(f.as_("accounts-agent").args(["ls", "/legal"]), None);
        let b = run(f.as_("accounts-agent").args(["ls", "/hr"]), None);
        assert_eq!(
            a.stderr.replace("/legal", "X"),
            b.stderr.replace("/hr", "X"),
            "A5: the two must be indistinguishable beyond the path"
        );
    });
}

/// A6: `tree /` is complete below each share root and shows nothing else.
#[test]
fn a_tree_of_the_root_is_complete_below_each_share() {
    scenarios!("A6");
    on_each_engine(|f| {
        let out = ok(f.as_("accounts-agent").args(["tree", "/"]), None).stdout;
        for want in ["contracts/", "acme.md", "2026/", "q3.md", "products/", "roadmap.md", "catalog/", "x.md"] {
            assert!(out.contains(want), "A6: missing {want}\n{out}");
        }
        for never in ["legal", "policies", "nda", "hr", "salaries", "vendor", "beta"] {
            assert!(!out.contains(never), "A6: leaked {never}\n{out}");
        }
    });
}

/// A7: `ls -l /` is one row per share, with a RIGHTS column and the shared node's exact totals.
#[test]
fn a_long_root_listing_names_the_rights_and_the_shares_totals() {
    scenarios!("A7");
    on_each_engine(|f| {
        let out = ok(f.as_("accounts-agent").args(["ls", "-l", "/"]), None).stdout;
        assert!(out.contains("RIGHTS"), "A7: no RIGHTS column\n{out}");
        let rows: Vec<&str> = out.lines().filter(|l| l.contains("contracts/") || l.contains("products/")).collect();
        assert_eq!(rows.len(), 2, "A7: one row per share\n{out}");
        assert!(rows.iter().any(|r| r.contains("rw") && r.contains("contracts/")), "A7\n{out}");
        assert!(rows.iter().any(|r| r.contains("ro") && r.contains("products/")), "A7\n{out}");

        // The totals are the shared node's own, exactly: whole-subtree shares make them exact.
        let mine = ok(f.as_("accounts-agent").args(["--json", "ls", "/"]), None).json();
        let theirs = ok(f.as_("admin").args(["--json", "stat", "/legal/contracts"]), None).json();
        let row = mine.as_array().unwrap().iter().find(|e| e["path"] == "/contracts").expect("the share row");
        for key in ["nbytes", "nlines", "nwords", "files", "folders", "versions"] {
            assert_eq!(row[key], theirs[key], "A7: {key} must be the shared node's own");
        }
    });
}

/// A8, A9: the root is not a folder — it is the list of shares — and a share root says so.
#[test]
fn a_stat_of_the_root_and_of_a_share_root() {
    scenarios!("A8", "A9");
    on_each_engine(|f| {
        // A8.
        let root = ok(f.as_("accounts-agent").args(["--json", "stat", "/"]), None).json();
        assert_eq!(root["kind"], "root", "A8: the root is not a folder: {root}");
        let shares = root["shares"].as_array().expect("shares on the root row");
        let named: Vec<(&str, &str)> = shares
            .iter()
            .map(|s| (s["alias"].as_str().unwrap_or_default(), s["rights"].as_str().unwrap_or_default()))
            .collect();
        assert_eq!(named, [("contracts", "rw"), ("products", "ro")], "A8");

        // A9: a share root is a folder, carries its rights, is marked as a share root, and its
        // id is the store's own — the same node the admin sees at /legal/contracts.
        let share = ok(f.as_("accounts-agent").args(["--json", "stat", "/contracts"]), None).json();
        assert_eq!(share["kind"], "folder", "A9");
        assert_eq!(share["rights"], "rw", "A9");
        assert_eq!(share["share"], "contracts", "A9");
        assert_eq!(share["id"].as_i64(), Some(f.id("/legal/contracts")), "A9: the store's stable id");
    });
}

/// A13: a local path is normalised as any other — trailing slash, no leading slash.
#[test]
fn a_local_paths_are_normalised_like_any_other() {
    scenarios!("A13");
    on_each_engine(|f| {
        let canonical = ok(f.as_("accounts-agent").args(["--json", "ls", "/contracts"]), None).stdout;
        for spelling in ["/contracts/", "contracts", "contracts/"] {
            let out = ok(f.as_("accounts-agent").args(["--json", "ls", spelling]), None).stdout;
            assert_eq!(out, canonical, "A13: {spelling}");
        }
    });
}

// ============================================================ H. Grants, aliases, tokens

/// H1, H2, H23, H24: the alias is chosen at grant time and never auto-suffixed, so paths in an
/// account's view cannot move because of the order shares were added in.
#[test]
fn h_an_alias_collision_is_refused_rather_than_suffixed() {
    scenarios!("H1", "H2", "H23", "H24");
    on_each_engine(|f| {
        // H1: the default alias is the folder's name, and `contracts` is taken.
        let clash = run(f.as_("admin").args(["access", "grant", "accounts-agent", "/vendor/contracts", "rw"]), None);
        refused(&clash, INVALID, "TX004");
        assert!(clash.stderr.contains("contracts") && clash.stderr.contains("--as"), "H1: {}", clash.stderr);
        assert!(!clash.stderr.contains("contracts-2"), "H1: never auto-suffixed: {}", clash.stderr);

        // H2: with an explicit alias it goes through.
        ok(f.as_("admin").args(["access", "grant", "accounts-agent", "/vendor/contracts", "rw", "--as", "vendor-contracts"]), None);
        let roots = ok(f.as_("accounts-agent").args(["ls", "-1", "/"]), None).stdout;
        assert!(roots.contains("vendor-contracts"), "H2: {roots}");

        // H23: an alias need not resemble the store path it points at.
        ok(f.as_("admin").args(["account", "create", "odd", "--kind", "agent"]), None);
        ok(f.as_("admin").args(["access", "grant", "odd", "/legal/contracts", "rw", "--as", "legal"]), None);
        // H24: and two accounts may use the same alias for different folders.
        ok(f.as_("admin").args(["access", "grant", "product-agent", "/legal/contracts", "ro", "--as", "contracts"]), None);
        let his = ok(f.as_("admin").args(["--json", "access", "ls", "product-agent"]), None).json();
        let aliases: Vec<&str> = his.as_array().unwrap().iter().filter_map(|g| g["alias"].as_str()).collect();
        assert!(aliases.contains(&"contracts") && aliases.contains(&"products"), "H24: {his}");
    });
}

/// H3, H4: no overlapping grants inside one account in v1 — one file, one path, one right.
#[test]
fn h_overlapping_grants_in_one_account_are_refused() {
    scenarios!("H3", "H4");
    on_each_engine(|f| {
        // H3: /legal contains the already-granted /legal/contracts.
        let outer = run(f.as_("admin").args(["access", "grant", "accounts-agent", "/legal", "rw"]), None);
        refused(&outer, INVALID, "TX004");
        assert!(outer.stderr.contains("overlap") && outer.stderr.contains("/legal/contracts"), "H3: {}", outer.stderr);
        // H4: and the other way round — auditor holds /legal.
        let inner = run(f.as_("admin").args(["access", "grant", "auditor", "/legal/contracts", "rw"]), None);
        refused(&inner, INVALID, "TX004");
        assert!(inner.stderr.contains("overlap") && inner.stderr.contains("/legal"), "H4: {}", inner.stderr);
    });
}

/// H5: granting the same node again replaces the rights and keeps the alias.
#[test]
fn h_regranting_the_same_node_replaces_the_rights() {
    scenarios!("H5");
    on_each_engine(|f| {
        let out = ok(f.as_("admin").args(["access", "grant", "accounts-agent", "/legal/contracts", "ro"]), None);
        assert!(out.stdout.contains("contracts") && out.stdout.contains("ro"), "H5: {}", out.stdout);
        let grants = ok(f.as_("admin").args(["--json", "access", "ls", "accounts-agent"]), None).json();
        let g = grants.as_array().unwrap().iter().find(|g| g["alias"] == "contracts").expect("H5: alias kept");
        assert_eq!(g["rights"], "ro", "H5");
        // And the account feels it: a write that worked now does not.
        refused(&run(f.as_("accounts-agent").args(["write", "/contracts/x.md"]), Some("x\n")), FORBIDDEN, "TX005");
    });
}

/// H6: renaming an alias is a move in that account's view, not a delete and a create.
#[test]
fn h_an_alias_rename_is_a_move_in_the_accounts_view() {
    scenarios!("H6");
    on_each_engine(|f| {
        ok(f.as_("admin").args(["access", "rename", "accounts-agent", "contracts", "legal-contracts"]), None);
        let roots = ok(f.as_("accounts-agent").args(["ls", "-1", "/"]), None).stdout;
        assert!(roots.contains("legal-contracts") && !roots.contains("/contracts"), "H6: {roots}");
        // The content is the same document, under the new name.
        assert_eq!(
            ok(f.as_("accounts-agent").args(["--json", "stat", "/legal-contracts/acme.md"]), None).json()["id"].as_i64(),
            Some(f.id("/legal/contracts/acme.md")),
            "H6"
        );
    });
}

/// H7, H8, H9: a revoked alias answers forbidden rather than vanishing, revives on a re-grant of
/// the same folder, and is not silently rebound to a different one.
#[test]
fn h_a_revoked_alias_is_forbidden_not_absent_and_is_not_reused_by_accident() {
    scenarios!("H7", "H8", "H9");
    on_each_engine(|f| {
        // H7.
        ok(f.as_("admin").args(["access", "revoke", "accounts-agent", "products"]), None);
        assert!(!ok(f.as_("accounts-agent").args(["ls", "-1", "/"]), None).stdout.contains("products"), "H7");
        refused(&run(f.as_("accounts-agent").args(["cat", "/products/roadmap.md"]), None), FORBIDDEN, "TX005");

        // H9: the alias cannot be bound to a different folder without saying so.
        let rebind = run(f.as_("admin").args(["access", "grant", "accounts-agent", "/hr", "ro", "--as", "products"]), None);
        refused(&rebind, INVALID, "TX004");
        assert!(rebind.stderr.contains("--reuse-alias"), "H9: {}", rebind.stderr);

        // H8: the same folder again revives it with the same alias.
        ok(f.as_("admin").args(["access", "grant", "accounts-agent", "/products", "ro"]), None);
        assert!(ok(f.as_("accounts-agent").args(["ls", "-1", "/"]), None).stdout.contains("products"), "H8");
        ok(f.as_("accounts-agent").args(["cat", "/products/roadmap.md"]), None);
    });
}

/// H10, H11, H12: what cannot be granted — an unknown account, a file, the store root.
#[test]
fn h_grants_are_folders_of_a_known_account_and_never_the_root() {
    scenarios!("H10", "H11", "H12");
    on_each_engine(|f| {
        // H10.
        let who = run(f.as_("admin").args(["access", "grant", "nobody", "/products", "ro"]), None);
        refused(&who, NOT_FOUND, "TX003");
        assert!(who.stderr.contains("nobody"), "H10: {}", who.stderr);
        // H11: shares are folders.
        let file = run(f.as_("admin").args(["access", "grant", "product-agent", "/legal/contracts/acme.md", "ro"]), None);
        refused(&file, INVALID, "TX004");
        assert!(file.stderr.contains("folder"), "H11: {}", file.stderr);
        // H12: the root is not shareable in v1.
        let root = run(f.as_("admin").args(["access", "grant", "product-agent", "/", "ro"]), None);
        refused(&root, INVALID, "TX004");
    });
}

/// H13, H14, H15: who can see a node and under what name, an account's own grants, and `whoami`.
#[test]
fn h_access_ls_answers_from_both_ends_and_whoami_from_inside() {
    scenarios!("H13", "H14", "H15");
    on_each_engine(|f| {
        // H13: by node — every account that reaches it, with the alias each uses.
        let holders = ok(f.as_("admin").args(["--json", "access", "ls", "/legal/contracts"]), None).json();
        let rows: Vec<(String, String, String)> = holders
            .as_array()
            .unwrap()
            .iter()
            .map(|g| {
                (
                    g["account"].as_str().unwrap_or_default().to_string(),
                    g["alias"].as_str().unwrap_or_default().to_string(),
                    g["rights"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        assert!(rows.contains(&("accounts-agent".into(), "contracts".into(), "rw".into())), "H13: {holders}");
        // auditor reaches it through the enclosing share, which the row says.
        assert!(
            rows.iter().any(|(a, alias, r)| a == "auditor" && alias == "legal" && r == "ro"),
            "H13: auditor via legal: {holders}"
        );
        assert!(rows.iter().any(|(a, _, r)| a == "contracts-agent" && r == "rw"), "H13: {holders}");

        // H14: by account.
        let mine = ok(f.as_("admin").args(["--json", "access", "ls", "accounts-agent"]), None).json();
        let by_alias: Vec<(String, String, String)> = mine
            .as_array()
            .unwrap()
            .iter()
            .map(|g| {
                (
                    g["alias"].as_str().unwrap_or_default().to_string(),
                    g["path"].as_str().unwrap_or_default().to_string(),
                    g["rights"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        assert_eq!(
            by_alias,
            [
                ("contracts".to_string(), "/legal/contracts".to_string(), "rw".to_string()),
                ("products".to_string(), "/products".to_string(), "ro".to_string()),
            ],
            "H14"
        );

        // H15: from inside, the account sees its aliases and rights — and no store paths.
        let me = ok(f.as_("accounts-agent").args(["whoami"]), None).stdout;
        assert!(me.contains("accounts-agent") && me.contains("contracts") && me.contains("rw"), "H15: {me}");
        assert!(!me.contains("/legal"), "H15: store paths are the admin's to see: {me}");
    });
}

/// H16, H17, H18: a bearer is printed once and stored hashed; revoking it, or disabling the
/// account, stops every command with it.
#[test]
fn h_tokens_are_printed_once_stored_hashed_and_can_be_stopped() {
    scenarios!("H16", "H17", "H18");
    on_each_engine(|f| {
        // H16.
        let made = ok(f.as_("admin").args(["--json", "token", "create", "auditor", "--expires", "30d"]), None).json();
        let bearer = made["bearer"].as_str().expect("H16: the bearer, once").to_string();
        let id = made["id"].as_i64().expect("H16: an id to revoke by");
        assert!(bearer.len() >= 32, "H16: {bearer}");
        ok(cmd(&f.store, Some(&bearer)).args(["ls", "/"]), None);

        // Stored hashed: the bearer itself is nowhere in the store.
        let listed = ok(f.as_("admin").args(["--json", "token", "ls", "auditor"]), None).json();
        assert!(!listed.to_string().contains(&bearer), "H16: the bearer must not be readable back: {listed}");

        // H17.
        ok(f.as_("admin").args(["token", "revoke", &id.to_string()]), None);
        refused(&run(cmd(&f.store, Some(&bearer)).args(["ls", "/"]), None), FORBIDDEN, "TX005");

        // H18: disabling the account stops its other tokens too, and keeps the grants.
        ok(f.as_("admin").args(["account", "disable", "accounts-agent"]), None);
        refused(&run(f.as_("accounts-agent").args(["ls", "/"]), None), FORBIDDEN, "TX005");
        let kept = ok(f.as_("admin").args(["--json", "access", "ls", "accounts-agent"]), None).json();
        assert_eq!(kept.as_array().map(Vec::len), Some(2), "H18: grants kept: {kept}");
    });
}

/// H20, H21, H22: a single-root account owns exactly one vault, and widening it is explicit.
#[test]
fn h_a_single_root_account_holds_one_share_until_it_is_converted() {
    scenarios!("H20", "H21", "H22");
    on_each_engine(|f| {
        // H20: made in the fixture; its root is the share.
        let mine = ok(f.as_("contracts-agent").args(["ls", "-1", "/"]), None).stdout;
        assert!(mine.contains("acme.md"), "H20: {mine}");

        // H21: a second grant is refused, naming the way to change that.
        let second = run(f.as_("admin").args(["access", "grant", "contracts-agent", "/products", "ro"]), None);
        refused(&second, INVALID, "TX004");
        assert!(second.stderr.contains("single-root") && second.stderr.contains("convert"), "H21: {}", second.stderr);

        // H22: converting says what it costs, and every path gains the prefix.
        let converted = ok(
            f.as_("admin").args(["account", "convert", "contracts-agent", "--multi", "--root-alias", "contracts"]),
            None,
        );
        assert!(converted.stdout.contains("/contracts"), "H22: {}", converted.stdout);
        assert_eq!(
            ok(f.as_("contracts-agent").args(["--json", "stat", "/contracts/acme.md"]), None).json()["id"].as_i64(),
            Some(f.id("/legal/contracts/acme.md")),
            "H22"
        );
        refused(&run(f.as_("contracts-agent").args(["cat", "/acme.md"]), None), NOT_FOUND, "TX003");
    });
}

/// H19: the hosted server has no anonymous admin; an admin-kind account and token is the way in.
#[test]
fn h_the_server_requires_a_token_even_for_admin() {
    scenarios!("H19");
    on_each_engine(|f| {
        // Opening the store directly is admin (L1); presenting no token *where one is required*
        // is not. The store models this as: a token session is never implicitly admin.
        ok(f.as_("admin").args(["account", "create", "ops", "--kind", "admin"]), None);
        let made = ok(f.as_("admin").args(["--json", "token", "create", "ops"]), None).json();
        let bearer = made["bearer"].as_str().unwrap().to_string();
        // An admin-kind account sees the store's own paths.
        let roots = ok(cmd(&f.store, Some(&bearer)).args(["ls", "-1", "/"]), None).stdout;
        assert!(roots.contains("/legal") && roots.contains("/hr"), "H19: {roots}");
        // While a garbage bearer is refused rather than falling back to admin.
        refused(&run(cmd(&f.store, Some("not-a-real-token")).args(["ls", "/"]), None), FORBIDDEN, "TX005");
    });
}

// ============================================================ B. Reading and searching

/// B1, B2, B8: reading inside a share, including a read-only one and past versions.
#[test]
fn b_reading_inside_a_share() {
    scenarios!("B1", "B2", "B8");
    on_each_engine(|f| {
        // B1: the header names the local path.
        let numbered = ok(f.as_("accounts-agent").args(["cat", "-n", "/contracts/acme.md"]), None).stdout;
        assert!(numbered.starts_with("/contracts/acme.md v3 "), "B1: {numbered}");
        // B2: read-only still reads.
        assert!(ok(f.as_("accounts-agent").args(["cat", "/products/roadmap.md"]), None).stdout.contains("Roadmap"), "B2");
        // B8: once a file is readable, so is its history.
        assert!(ok(f.as_("accounts-agent").args(["cat", "/contracts/acme.md", "-v", "1"]), None).stdout.contains("Acme"), "B8");
    });
}

/// B3, B13: a store path is not a local path, wherever one is accepted.
#[test]
fn b_store_paths_do_not_resolve_in_a_local_view() {
    scenarios!("B3", "B13");
    on_each_engine(|f| {
        refused(&run(f.as_("accounts-agent").args(["cat", "/legal/contracts/acme.md"]), None), NOT_FOUND, "TX003");
        refused(&run(f.as_("accounts-agent").args(["search", "acme", "-p", "/legal"]), None), NOT_FOUND, "TX003");
    });
}

/// B4, B5, B6, B7: `id:` is the cross-view reference, and an id outside the view is as absent as
/// a path outside it.
#[test]
fn b_ids_address_a_document_in_whichever_view_can_see_it() {
    scenarios!("B4", "B5", "B6", "B7");
    on_each_engine(|f| {
        let acme = f.id("/legal/contracts/acme.md");
        let salaries = f.id("/hr/salaries.md");
        let path_of = |account: &str, id: i64| -> String {
            ok(f.as_(account).args(["--json", "cat", &format!("id:{id}")]), None).json()["path"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(path_of("accounts-agent", acme), "/contracts/acme.md", "B4");
        assert_eq!(path_of("auditor", acme), "/legal/contracts/acme.md", "B6");
        assert_eq!(path_of("contracts-agent", acme), "/acme.md", "B7");
        // B5: an id nobody granted looks like nothing at all.
        refused(&run(f.as_("accounts-agent").args(["cat", &format!("id:{salaries}")]), None), NOT_FOUND, "TX003");
    });
}

/// B9, B10, B11, B12: search is scoped to the view, exact inside it, and silent about the rest.
#[test]
fn b_search_covers_the_view_and_says_nothing_of_what_is_outside() {
    scenarios!("B9", "B10", "B11", "B12");
    on_each_engine(|f| {
        let paths = |account: &str, args: &[&str]| -> Vec<String> {
            let out = ok(f.as_(account).args(["--json", "search"]).args(args), None).json();
            let mut p: Vec<String> =
                out.as_array().unwrap().iter().filter_map(|h| h["path"].as_str().map(str::to_string)).collect();
            p.sort();
            p.dedup();
            p
        };
        // B9: hits under the two shares, as local paths, and nothing from /vendor or /hr.
        let mine = paths("accounts-agent", &["acme"]);
        assert!(mine.iter().all(|p| p.starts_with("/contracts/") || p.starts_with("/products/")), "B9: {mine:?}");
        assert!(mine.iter().any(|p| p == "/products/roadmap.md"), "B9: {mine:?}");
        // B11: admin sees every hit, under store paths.
        let theirs = paths("admin", &["acme"]);
        assert!(theirs.iter().any(|p| p == "/legal/contracts/acme.md"), "B11: {theirs:?}");
        assert!(theirs.iter().any(|p| p == "/vendor/contracts/beta.md" || p == "/products/roadmap.md"), "B11: {theirs:?}");
        // B10: a word only in a hidden file finds nothing, with no hint the file exists.
        let empty = run(f.as_("auditor").args(["search", "salaries"]), None);
        assert_eq!(empty.status, 0, "B10");
        assert!(empty.stdout.trim().is_empty(), "B10: {}", empty.stdout);
        assert!(!empty.stderr.contains("/hr"), "B10: {}", empty.stderr);
        // B12: a prefix inside the share scopes as usual.
        let scoped = paths("accounts-agent", &["review", "-p", "/contracts/2026"]);
        assert!(scoped.iter().all(|p| p.starts_with("/contracts/2026/")), "B12: {scoped:?}");
    });
}

/// B14: grep and the front-matter commands see the view, and their counts are exact because a
/// share is a whole subtree.
#[test]
fn b_grep_and_meta_cover_the_view_exactly() {
    scenarios!("B14");
    on_each_engine(|f| {
        ok(f.as_("admin").args(["write", "/legal/contracts/tagged.md"]), Some("---\nstatus: draft\n---\n# T\n\nfindme\n"));
        ok(f.as_("admin").args(["write", "/hr/tagged.md"]), Some("---\nstatus: draft\n---\n# H\n\nfindme\n"));

        let hits = ok(f.as_("accounts-agent").args(["--json", "grep", "findme"]), None).json();
        let paths: Vec<&str> = hits.as_array().unwrap().iter().filter_map(|h| h["path"].as_str()).collect();
        assert_eq!(paths, ["/contracts/tagged.md"], "B14: grep");

        let keys = ok(f.as_("accounts-agent").args(["--json", "meta", "keys"]), None).json();
        let status = keys.as_array().unwrap().iter().find(|k| k["key"] == "status").expect("B14: status");
        assert_eq!(status["docs"], 1, "B14: counts exact and view-scoped: {keys}");

        let found = ok(f.as_("accounts-agent").args(["--json", "meta", "find", "status:draft"]), None).json();
        let paths: Vec<&str> = found.as_array().unwrap().iter().filter_map(|h| h["path"].as_str()).collect();
        assert_eq!(paths, ["/contracts/tagged.md"], "B14: meta find");
    });
}

/// B15, B16: history is complete for a readable file, and a path event from outside the view is
/// masked rather than shown.
#[test]
fn b_history_is_complete_but_masks_paths_from_outside_the_view() {
    scenarios!("B15", "B16");
    on_each_engine(|f| {
        // B15: versions committed before the grant existed are still readable.
        let h = ok(f.as_("accounts-agent").args(["--json", "history", "/contracts/acme.md", "--versions-only"]), None).json();
        assert_eq!(h.as_array().map(Vec::len), Some(3), "B15: {h}");

        // B16: a file moved in from outside keeps its history; the old path is not disclosed.
        ok(f.as_("admin").args(["write", "/hr/old.md"]), Some("moved later\n"));
        ok(f.as_("admin").args(["mv", "/hr/old.md", "/legal/contracts/moved.md"]), None);
        let mine = ok(f.as_("accounts-agent").args(["--json", "history", "/contracts/moved.md"]), None).json();
        let text = mine.to_string();
        assert!(text.contains("/contracts/moved.md"), "B16: {text}");
        assert!(!text.contains("/hr/old.md") && !text.contains("/hr"), "B16: the old path must be masked: {text}");
        // Admin sees it in full.
        let theirs = ok(f.as_("admin").args(["--json", "history", "/legal/contracts/moved.md"]), None).json();
        assert!(theirs.to_string().contains("/hr/old.md"), "B16: {theirs}");
    });
}

/// B17: `links` in a local view — the shapes are section E; here only that it answers at all.
#[test]
fn b_links_answers_in_a_local_view() {
    scenarios!("B17");
    on_each_engine(|f| {
        let out = ok(f.as_("accounts-agent").args(["--json", "links", "/contracts/acme.md"]), None).json();
        assert!(out.as_array().is_some_and(|r| !r.is_empty()), "B17: {out}");
        assert!(out.as_array().unwrap().iter().all(|l| l["path"] == "/contracts/acme.md"), "B17: {out}");
    });
}

/// B18, B19: export writes the view's own layout, from a share or from the root.
#[test]
fn b_export_writes_the_views_own_layout() {
    scenarios!("B18", "B19");
    on_each_engine(|f| {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one");
        ok(f.as_("accounts-agent").args(["export", "/contracts"]).arg(&one), None);
        assert!(one.join("acme.md").is_file(), "B18");
        assert!(one.join("2026/q3.md").is_file(), "B18");

        let all = dir.path().join("all");
        ok(f.as_("accounts-agent").args(["export", "/"]).arg(&all), None);
        assert!(all.join("contracts/acme.md").is_file(), "B19");
        assert!(all.join("products/roadmap.md").is_file(), "B19");
        assert!(!all.join("legal").exists() && !all.join("hr").exists(), "B19: nothing outside the view");
    });
}
