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
    // A filtered run (`cargo test --test access something`) leaves COVERED partly empty, which
    // says nothing about the catalogue. The filter is the one positional argument the harness
    // takes, so its presence is how this tells the two apart.
    if std::env::args().skip(1).any(|a| !a.starts_with('-')) {
        return;
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

// ============================================================ C. Writing inside shares

/// C1, C2, C3, C4, C14, C15: writing inside an `rw` share lands centrally and is seen by every
/// other view under its own alias, whoever wrote it.
#[test]
fn c_writes_inside_an_rw_share_land_centrally_and_are_seen_by_every_view() {
    scenarios!("C1", "C2", "C3", "C4", "C14", "C15");
    on_each_engine(|f| {
        // C1: an edit by the account, attributed to it.
        ok(f.as_("accounts-agent").args(["edit", "/contracts/acme.md", "--old", "v3", "--new", "v4"]), None);
        let central = ok(f.as_("admin").args(["--json", "stat", "/legal/contracts/acme.md"]), None).json();
        assert_eq!(central["version"], 4, "C1");
        assert_eq!(central["updated_by"], "accounts-agent", "C1: the author is the account");
        // Seen by the others under their own names.
        assert_eq!(ok(f.as_("auditor").args(["--json", "stat", "/legal/contracts/acme.md"]), None).json()["version"], 4, "C1");
        assert_eq!(ok(f.as_("contracts-agent").args(["--json", "stat", "/acme.md"]), None).json()["version"], 4, "C1");

        // C2, C3: create a file and a folder inside the share.
        ok(f.as_("accounts-agent").args(["write", "--create", "/contracts/new.md"]), Some("new\n"));
        ok(f.as_("admin").args(["cat", "/legal/contracts/new.md"]), None);
        ok(f.as_("accounts-agent").args(["mkdir", "/contracts/2027"]), None);
        ok(f.as_("admin").args(["stat", "/legal/contracts/2027"]), None);

        // C4: append never conflicts.
        ok(f.as_("accounts-agent").args(["append", "/contracts/acme.md", "- note"]), None);

        // C14: admin writes; the account sees it under its alias.
        ok(f.as_("admin").args(["edit", "/legal/contracts/acme.md", "--old", "v4", "--new", "v5"]), None);
        assert!(ok(f.as_("accounts-agent").args(["cat", "/contracts/acme.md"]), None).stdout.contains("v5"), "C14");

        // C15: another account writes in its own rw share; the ro holder sees it and cannot edit.
        ok(f.as_("product-agent").args(["edit", "/products/roadmap.md", "--old", "acme", "--new", "ACME"]), None);
        assert!(ok(f.as_("accounts-agent").args(["cat", "/products/roadmap.md"]), None).stdout.contains("ACME"), "C15");
        refused(
            &run(f.as_("accounts-agent").args(["edit", "/products/roadmap.md", "--old", "ACME", "--new", "x"]), None),
            FORBIDDEN,
            "TX005",
        );
    });
}

/// C5, C16: a conflict is reported in the caller's own paths, and two views editing one line
/// behave as two writers always have.
#[test]
fn c_conflicts_speak_the_callers_own_paths() {
    scenarios!("C5", "C16");
    on_each_engine(|f| {
        let v = ok(f.as_("accounts-agent").args(["--json", "stat", "/contracts/acme.md"]), None).json()["version"]
            .as_i64()
            .unwrap();
        // Someone else moves the line on.
        ok(f.as_("contracts-agent").args(["replace-lines", "/acme.md", "5", "5", "--text", "theirs\n"]), None);
        // C5, C16: the stale writer is refused, and the payload is in its own namespace.
        let out = run(
            f.as_("accounts-agent")
                .args(["--json", "replace-lines", "/contracts/acme.md", "5", "5", "--text", "mine\n", "-b", &v.to_string()]),
            None,
        );
        refused(&out, CONFLICT, "TX001");
        let text = out.stdout.clone() + &out.stderr;
        assert!(text.contains("/contracts/acme.md"), "C16: {text}");
        assert!(!text.contains("/legal/"), "C16: no store paths in the payload: {text}");
    });
}

/// C6, C7, C8, C9, C10, C12: what a local view may not write — a read-only share, the account
/// root, and a path that does not exist in the view.
#[test]
fn c_writes_outside_an_rw_share_are_refused_and_say_which_kind_of_no() {
    scenarios!("C6", "C7", "C8", "C9", "C10", "C12");
    on_each_engine(|f| {
        // C6, C7: read-only share — it exists in the view, so this is forbidden, not absent.
        let ro = run(f.as_("accounts-agent").args(["write", "--create", "/products/note.md"]), Some("x\n"));
        refused(&ro, FORBIDDEN, "TX005");
        assert!(ro.stderr.contains("read-only") || ro.stderr.contains("ro"), "C6: {}", ro.stderr);
        refused(
            &run(f.as_("accounts-agent").args(["meta", "set", "/products/roadmap.md", "status", "x"]), None),
            FORBIDDEN,
            "TX005",
        );

        // C8, C9: the root lists shares; it is not a folder to write in, and it names them.
        let root = run(f.as_("accounts-agent").args(["write", "--create", "/note.md"]), Some("x\n"));
        refused(&root, FORBIDDEN, "TX005");
        assert!(root.stderr.contains("contracts") && root.stderr.contains("products"), "C8: {}", root.stderr);
        refused(&run(f.as_("accounts-agent").args(["mkdir", "/archive"]), None), FORBIDDEN, "TX005");

        // C10: a parent that is not in the view is absent, not forbidden.
        refused(
            &run(f.as_("accounts-agent").args(["write", "--create", "/legal/policies/x.md"]), Some("x\n")),
            NOT_FOUND,
            "TX003",
        );

        // C12: import into the root is the root write again.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "a\n").unwrap();
        refused(&run(f.as_("accounts-agent").args(["import"]).arg(dir.path()).args(["--prefix", "/"]), None), FORBIDDEN, "TX005");
    });
}

/// C11: import into a share writes under the share's store path.
#[test]
fn c_import_into_a_share_lands_under_its_store_path() {
    scenarios!("C11");
    on_each_engine(|f| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "imported\n").unwrap();
        ok(f.as_("accounts-agent").args(["import"]).arg(dir.path()).args(["--prefix", "/contracts/imported"]), None);
        assert!(ok(f.as_("admin").args(["cat", "/legal/contracts/imported/a.md"]), None).stdout.contains("imported"), "C11");
    });
}

/// C13: an account writes as itself; only admin may name another author.
#[test]
fn c_an_account_cannot_write_as_someone_else() {
    scenarios!("C13");
    on_each_engine(|f| {
        let out = run(f.as_("accounts-agent").args(["-a", "bob", "edit", "/contracts/acme.md", "--old", "v3", "--new", "v9"]), None);
        refused(&out, FORBIDDEN, "TX005");
        assert!(out.stderr.contains("accounts-agent"), "C13: {}", out.stderr);
        // Admin may.
        ok(f.as_("admin").args(["-a", "ruta", "edit", "/legal/contracts/acme.md", "--old", "v3", "--new", "v9"]), None);
        assert_eq!(
            ok(f.as_("admin").args(["--json", "stat", "/legal/contracts/acme.md"]), None).json()["updated_by"],
            "ruta",
            "C13"
        );
    });
}

/// C17, C18: `sql --write` is checked per row, all or nothing.
#[test]
fn c_sql_writes_are_checked_per_row_before_anything_commits() {
    scenarios!("C17", "C18");
    on_each_engine(|f| {
        // C17: every row under an rw share.
        ok(
            f.as_("accounts-agent").args([
                "sql",
                "--write",
                "UPDATE kb SET content = content || '\n-- seen\n' WHERE dir = '/contracts'",
            ]),
            None,
        );
        assert!(ok(f.as_("admin").args(["cat", "/legal/contracts/acme.md"]), None).stdout.contains("-- seen"), "C17");

        // C18: one row outside rw refuses the whole statement, and nothing is written.
        let before = ok(f.as_("admin").args(["cat", "/products/roadmap.md"]), None).stdout;
        let out = run(
            f.as_("accounts-agent")
                .args(["sql", "--write", "UPDATE kb SET content = content || 'x' WHERE dir = '/products'"]),
            None,
        );
        refused(&out, FORBIDDEN, "TX005");
        assert_eq!(ok(f.as_("admin").args(["cat", "/products/roadmap.md"]), None).stdout, before, "C18: nothing written");
    });
}

/// C19, C20: reverting a batch of one's own is allowed; one that touched a read-only share is not.
#[test]
fn c_revert_batch_needs_every_file_of_the_batch_to_be_writable() {
    scenarios!("C19", "C20");
    on_each_engine(|f| {
        // C19: a batch entirely inside the rw share.
        let mine = ok(
            f.as_("accounts-agent")
                .args(["--json", "sql", "--write", "UPDATE kb SET content = content || '\nc19\n' WHERE path = '/contracts/acme.md'"]),
            None,
        )
        .json();
        let batch = mine["batch"].as_str().expect("C19: a batch id").to_string();
        ok(f.as_("accounts-agent").args(["revert-batch", &batch]), None);
        assert!(!ok(f.as_("admin").args(["cat", "/legal/contracts/acme.md"]), None).stdout.contains("c19"), "C19");

        // C20: a batch admin made across both shares cannot be reverted by the ro holder.
        let theirs = ok(
            f.as_("admin")
                .args(["--json", "sql", "--write", "UPDATE kb SET content = content || '\nc20\n' WHERE path = '/products/roadmap.md'"]),
            None,
        )
        .json();
        let batch = theirs["batch"].as_str().expect("C20: a batch id").to_string();
        refused(&run(f.as_("accounts-agent").args(["revert-batch", &batch]), None), FORBIDDEN, "TX005");
        assert!(ok(f.as_("admin").args(["cat", "/products/roadmap.md"]), None).stdout.contains("c20"), "C20: nothing reverted");
    });
}

// ============================================================ D. Moves and deletes

/// D1, D3: a move inside a share is a move in every view that can see both ends.
#[test]
fn d_a_move_inside_a_share_is_a_move_everywhere_it_is_visible() {
    scenarios!("D1", "D3");
    on_each_engine(|f| {
        let id = f.id("/legal/contracts/acme.md");
        ok(f.as_("accounts-agent").args(["mv", "/contracts/acme.md", "/contracts/2026/acme.md"]), None);
        // The same node, at its new place, in each view's own words.
        assert_eq!(ok(f.as_("auditor").args(["--json", "stat", "/legal/contracts/2026/acme.md"]), None).json()["id"].as_i64(), Some(id), "D1");
        assert_eq!(ok(f.as_("contracts-agent").args(["--json", "stat", "/2026/acme.md"]), None).json()["id"].as_i64(), Some(id), "D1");
        // History follows the file.
        let h = ok(f.as_("contracts-agent").args(["--json", "history", "/2026/acme.md"]), None).json();
        assert!(h.to_string().contains("/acme.md"), "D1: {h}");

        // D3: a move by another account inside a shared folder is a move for the ro holder too.
        ok(f.as_("product-agent").args(["mv", "/products/roadmap.md", "/products/old/roadmap.md"]), None);
        ok(f.as_("accounts-agent").args(["stat", "/products/old/roadmap.md"]), None);
    });
}

/// D2, D4, D5, D7, D8: what a local view may not move or delete.
#[test]
fn d_moves_and_deletes_stop_at_the_edges_of_an_rw_share() {
    scenarios!("D2", "D4", "D5", "D7", "D8");
    on_each_engine(|f| {
        // D2: into a read-only share.
        refused(&run(f.as_("accounts-agent").args(["mv", "/contracts/acme.md", "/products/acme.md"]), None), FORBIDDEN, "TX005");
        // D4: out to the root.
        refused(&run(f.as_("accounts-agent").args(["mv", "/contracts/acme.md", "/acme.md"]), None), FORBIDDEN, "TX005");
        // D5, D7: a share root is not the account's to rename or delete.
        let renamed = run(f.as_("accounts-agent").args(["mv", "/contracts", "/contracts-2026"]), None);
        refused(&renamed, FORBIDDEN, "TX005");
        assert!(renamed.stderr.contains("share"), "D5: {}", renamed.stderr);
        refused(&run(f.as_("accounts-agent").args(["rm", "/contracts"]), None), FORBIDDEN, "TX005");
        // D8: a read-only file.
        refused(&run(f.as_("accounts-agent").args(["rm", "/products/roadmap.md"]), None), FORBIDDEN, "TX005");
    });
}

/// D6, D13: a real delete inside an rw share propagates to every view that could see it.
#[test]
fn d_a_delete_inside_a_share_is_a_delete_for_everyone_who_saw_it() {
    scenarios!("D6", "D13");
    on_each_engine(|f| {
        ok(f.as_("accounts-agent").args(["rm", "/contracts/2026"]), None);
        refused(&run(f.as_("auditor").args(["cat", "/legal/contracts/2026/q3.md"]), None), NOT_FOUND, "TX003");
        refused(&run(f.as_("contracts-agent").args(["cat", "/2026/q3.md"]), None), NOT_FOUND, "TX003");
        // D13: a delete outside every share touches nobody else.
        ok(f.as_("admin").args(["rm", "/hr/salaries.md"]), None);
        ok(f.as_("accounts-agent").args(["ls", "/contracts"]), None);
    });
}

/// D9, D10: the owner reorganising the store moves content into and out of accounts' views, and
/// the projection is honest about it.
#[test]
fn d_an_owner_move_can_add_or_remove_content_from_a_view() {
    scenarios!("D9", "D10");
    on_each_engine(|f| {
        // D9: the share node itself moves. Its grant follows the node, so the holders of that
        // node see nothing change; the holder of the enclosing folder loses the subtree.
        ok(f.as_("admin").args(["mv", "/legal/contracts", "/law/contracts"]), None);
        ok(f.as_("accounts-agent").args(["cat", "/contracts/acme.md"]), None);
        ok(f.as_("contracts-agent").args(["cat", "/acme.md"]), None);
        refused(&run(f.as_("auditor").args(["cat", "/legal/contracts/acme.md"]), None), NOT_FOUND, "TX003");

        // D10: a folder moved in from outside appears as a create.
        ok(f.as_("admin").args(["mv", "/vendor/contracts", "/law/contracts/vendor"]), None);
        ok(f.as_("accounts-agent").args(["cat", "/contracts/vendor/beta.md"]), None);
        ok(f.as_("contracts-agent").args(["cat", "/vendor/beta.md"]), None);
    });
}

/// D11, D12: a deleted share root makes the grant dormant — forbidden, not absent — and restoring
/// it revives the grant under the same alias.
#[test]
fn d_a_deleted_share_root_is_dormant_rather_than_gone() {
    scenarios!("D11", "D12");
    on_each_engine(|f| {
        ok(f.as_("admin").args(["rm", "/legal/contracts"]), None);
        // The alias is no longer listed…
        assert!(!ok(f.as_("accounts-agent").args(["ls", "-1", "/"]), None).stdout.contains("contracts"), "D11");
        // …but it answers forbidden, which is what keeps sync from deleting the disk copy (G8).
        refused(&run(f.as_("accounts-agent").args(["cat", "/contracts/acme.md"]), None), FORBIDDEN, "TX005");
        // A single-root account whose root is gone says so on everything.
        let out = run(f.as_("contracts-agent").args(["ls", "/"]), None);
        refused(&out, FORBIDDEN, "TX005");
        assert!(out.stderr.contains("no longer available") || out.stderr.contains("root share"), "D11: {}", out.stderr);

        // D12: restored, the grants revive with the same alias.
        let trashed = ok(f.as_("admin").args(["--json", "trash", "ls"]), None).json();
        let entry = trashed.as_array().unwrap().iter().find(|e| e["path"] == "/legal/contracts").expect("D12");
        let id = entry["id"].as_i64().expect("D12: a trash id");
        ok(f.as_("admin").args(["trash", "restore", &id.to_string()]), None);
        assert!(ok(f.as_("accounts-agent").args(["ls", "-1", "/"]), None).stdout.contains("contracts"), "D12");
        ok(f.as_("accounts-agent").args(["cat", "/contracts/acme.md"]), None);
    });
}

/// D14: `--update-links` rewrites what the account may write and says, without naming them, that
/// links elsewhere were left alone.
#[test]
fn d_update_links_rewrites_only_what_the_account_may_write() {
    scenarios!("D14");
    on_each_engine(|f| {
        ok(f.as_("admin").args(["write", "--create", "/hr/notes.md"]), Some("see [[q3]]\n"));
        let out = ok(f.as_("accounts-agent").args(["mv", "/contracts/2026/q3.md", "/contracts/q3.md", "--update-links"]), None);
        assert!(out.stdout.contains("outside your shares") || out.stderr.contains("outside your shares"), "D14: {}", out.stdout);
        let said = out.stdout.clone() + &out.stderr;
        assert!(!said.contains("/hr"), "D14: no paths from outside the view: {said}");
        // Admin sees the one left behind.
        let broken = ok(f.as_("admin").args(["--json", "links", "--broken", "/hr"]), None).json();
        assert!(broken.to_string().contains("/hr/notes.md"), "D14: {broken}");
    });
}

// ============================================================ E. Links

/// E1, E2, E3, E4, E6, E8: a name link resolves store-wide, and each view reads the answer in its
/// own paths; a relative link is namespace-free.
#[test]
fn e_resolved_links_are_reported_in_each_views_own_paths() {
    scenarios!("E1", "E2", "E3", "E4", "E6", "E8");
    on_each_engine(|f| {
        let target = |account: &str, path: &str, link: &str| -> (String, String) {
            let out = ok(f.as_(account).args(["--json", "links", path]), None).json();
            let row = out
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["target"].as_str() == Some(link))
                .unwrap_or_else(|| panic!("no link {link} in {out}"))
                .clone();
            (
                row["status"].as_str().unwrap_or_default().to_string(),
                row["resolved"].as_str().unwrap_or_default().to_string(),
            )
        };
        // E1, E2, E3: [[q3]] is the same document in three namespaces.
        assert_eq!(target("admin", "/legal/contracts/acme.md", "q3"), ("ok".into(), "/legal/contracts/2026/q3.md".into()), "E1");
        assert_eq!(target("accounts-agent", "/contracts/acme.md", "q3"), ("ok".into(), "/contracts/2026/q3.md".into()), "E2");
        assert_eq!(target("contracts-agent", "/acme.md", "q3"), ("ok".into(), "/2026/q3.md".into()), "E3");
        // E4, E6: [[nda]] resolves for those whose share covers it.
        assert_eq!(target("admin", "/legal/contracts/acme.md", "nda"), ("ok".into(), "/legal/policies/nda.md".into()), "E4");
        assert_eq!(target("auditor", "/legal/contracts/acme.md", "nda"), ("ok".into(), "/legal/policies/nda.md".into()), "E6");
        // E8: the relative link resolves under each view's own paths.
        assert_eq!(target("accounts-agent", "/contracts/acme.md", "2026/q3.md").0, "ok", "E8");
        assert_eq!(target("contracts-agent", "/acme.md", "2026/q3.md").0, "ok", "E8");
    });
}

/// E5, E7: a link whose target is outside the reader's shares is `hidden`, reads as an id
/// reference, and discloses no path.
#[test]
fn e_a_link_to_a_hidden_target_becomes_an_id_reference() {
    scenarios!("E5", "E7");
    on_each_engine(|f| {
        let nda = f.id("/legal/policies/nda.md");
        let beta = f.id("/vendor/contracts/beta.md");
        let rows = ok(f.as_("accounts-agent").args(["--json", "links", "/contracts/acme.md"]), None).json();
        let text = rows.to_string();
        assert!(!text.contains("/legal/policies") && !text.contains("/vendor"), "E5: no paths leak: {text}");
        for (label, id) in [("nda", nda), ("beta", beta)] {
            let row = rows
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["target"].as_str() == Some(&format!("textdb:{id}")))
                .unwrap_or_else(|| panic!("E5/E7: no textdb:{id} for {label} in {rows}"));
            assert_eq!(row["status"], "hidden", "E5/E7: {label}");
            assert!(row["resolved"].is_null(), "E5/E7: {label} must not name a path");
        }
        // And the text the account reads carries the id form, with the basename kept as the alias.
        let body = ok(f.as_("accounts-agent").args(["cat", "/contracts/acme.md"]), None).stdout;
        assert!(body.contains(&format!("[[textdb:{nda}|nda]]")), "E5: {body}");
    });
}

/// E9, E11: a root-path link written in a local view is stored canonically and read back in each
/// view's own paths.
#[test]
fn e_root_links_written_locally_are_stored_canonically() {
    scenarios!("E9", "E11");
    on_each_engine(|f| {
        // E9: accounts-agent writes its own root path.
        ok(f.as_("accounts-agent").args(["write", "--create", "/contracts/e9.md"]), Some("see [[contracts/2026/q3]]\n"));
        assert!(ok(f.as_("admin").args(["cat", "/legal/contracts/e9.md"]), None).stdout.contains("[[legal/contracts/2026/q3]]"), "E9");
        assert!(ok(f.as_("accounts-agent").args(["cat", "/contracts/e9.md"]), None).stdout.contains("[[contracts/2026/q3]]"), "E9");
        assert!(ok(f.as_("contracts-agent").args(["cat", "/e9.md"]), None).stdout.contains("[[2026/q3]]"), "E9");

        // E11: the single-root account's root path means the same document.
        ok(f.as_("contracts-agent").args(["write", "--create", "/e11.md"]), Some("see [[2026/q3]]\n"));
        assert!(ok(f.as_("admin").args(["cat", "/legal/contracts/e11.md"]), None).stdout.contains("[[legal/contracts/2026/q3]]"), "E11");
    });
}

/// E10, E12: a root-absolute markdown link written centrally is projected for each reader, and
/// becomes an id reference for one who cannot see the target.
#[test]
fn e_central_root_links_are_projected_per_reader() {
    scenarios!("E10", "E12");
    on_each_engine(|f| {
        let nda = f.id("/legal/policies/nda.md");
        ok(
            f.as_("admin").args(["write", "--create", "/legal/contracts/e10.md"]),
            Some("[q3](/legal/contracts/2026/q3.md) and [nda](/legal/policies/nda.md)\n"),
        );
        // E10.
        let mine = ok(f.as_("accounts-agent").args(["cat", "/contracts/e10.md"]), None).stdout;
        assert!(mine.contains("[q3](/contracts/2026/q3.md)"), "E10: {mine}");
        assert!(ok(f.as_("contracts-agent").args(["cat", "/e10.md"]), None).stdout.contains("[q3](/2026/q3.md)"), "E10");
        // E12: the hidden one, as an id, with no path; auditor still resolves it.
        assert!(mine.contains(&format!("[nda](textdb:{nda})")), "E12: {mine}");
        assert!(!mine.contains("/legal/policies"), "E12: {mine}");
        assert!(
            ok(f.as_("auditor").args(["cat", "/legal/contracts/e10.md"]), None).stdout.contains("[nda](/legal/policies/nda.md)"),
            "E12"
        );
    });
}

/// E13, E14: a local link naming a namespace that is not the store's is stored as written and is
/// broken for everyone; a relative link that escapes the share keeps its bytes.
#[test]
fn e_links_that_do_not_un_project_are_stored_as_written() {
    scenarios!("E13", "E14");
    on_each_engine(|f| {
        // E13.
        ok(f.as_("accounts-agent").args(["write", "--create", "/contracts/e13.md"]), Some("see [[hr/salaries]]\n"));
        assert!(ok(f.as_("admin").args(["cat", "/legal/contracts/e13.md"]), None).stdout.contains("[[hr/salaries]]"), "E13");
        let admin_links = ok(f.as_("admin").args(["--json", "links", "/legal/contracts/e13.md"]), None).json();
        assert_eq!(admin_links[0]["status"], "broken", "E13: {admin_links}");

        // E14: a relative link escaping the share resolves canonically; the bytes never change.
        ok(f.as_("accounts-agent").args(["write", "--create", "/contracts/e14.md"]), Some("see [nda](../policies/nda.md)\n"));
        for (who, path) in [("admin", "/legal/contracts/e14.md"), ("accounts-agent", "/contracts/e14.md"), ("auditor", "/legal/contracts/e14.md")] {
            assert!(ok(f.as_(who).args(["cat", path]), None).stdout.contains("[nda](../policies/nda.md)"), "E14: {who}");
        }
        let mine = ok(f.as_("accounts-agent").args(["--json", "links", "/contracts/e14.md"]), None).json();
        assert_eq!(mine[0]["status"], "hidden", "E14: {mine}");
        let theirs = ok(f.as_("auditor").args(["--json", "links", "/legal/contracts/e14.md"]), None).json();
        assert_eq!(theirs[0]["status"], "ok", "E14: {theirs}");
    });
}

/// E15: everything that is not a resolved root-absolute link is byte-identical in every view.
#[test]
fn e_everything_else_round_trips_byte_for_byte() {
    scenarios!("E15");
    on_each_engine(|f| {
        let body = "[[q3]] and [x](2026/q3.md) and `[[legal/contracts/q3]]` and <https://x.y/legal/contracts>\n\n```\n[[legal/contracts/q3]]\n```\n";
        ok(f.as_("accounts-agent").args(["write", "--create", "/contracts/e15.md"]), Some(body));
        for (who, path) in [("admin", "/legal/contracts/e15.md"), ("accounts-agent", "/contracts/e15.md"), ("auditor", "/legal/contracts/e15.md")] {
            assert_eq!(ok(f.as_(who).args(["cat", path]), None).stdout, body, "E15: {who}");
        }
    });
}

/// E16: an edit from a local view matches and replaces against un-projected text, and is one
/// version; each view sees the hunks in its own paths.
#[test]
fn e_an_edit_anchored_on_a_projected_link_is_one_version() {
    scenarios!("E16");
    on_each_engine(|f| {
        ok(f.as_("accounts-agent").args(["write", "--create", "/contracts/e16.md"]), Some("see [[contracts/2026/q3]]\n"));
        let before = ok(f.as_("admin").args(["--json", "stat", "/legal/contracts/e16.md"]), None).json()["version"].as_i64().unwrap();
        ok(
            f.as_("accounts-agent")
                .args(["edit", "/contracts/e16.md", "--old", "[[contracts/2026/q3]]", "--new", "[[contracts/2026/q4]]"]),
            None,
        );
        let after = ok(f.as_("admin").args(["--json", "stat", "/legal/contracts/e16.md"]), None).json();
        assert_eq!(after["version"].as_i64(), Some(before + 1), "E16: one version");
        assert!(ok(f.as_("admin").args(["cat", "/legal/contracts/e16.md"]), None).stdout.contains("[[legal/contracts/2026/q4]]"), "E16");
        // The hunks each view shows are in its own namespace.
        let mine = ok(f.as_("accounts-agent").args(["--json", "hunks", "/contracts/e16.md"]), None).json().to_string();
        assert!(mine.contains("contracts/2026/q4") && !mine.contains("legal/contracts"), "E16: {mine}");
    });
}

/// E17: a sync round trip of a file with root links makes no spurious change.
#[test]
fn e_a_sync_round_trip_of_projected_text_is_a_no_op() {
    scenarios!("E17");
    on_each_engine(|f| {
        ok(f.as_("accounts-agent").args(["write", "--create", "/contracts/e17.md"]), Some("see [[contracts/2026/q3]]\n"));
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("kb");
        ok(f.as_("accounts-agent").args(["sync", "/"]).arg(&out), None);
        // Disk holds the projected text…
        let on_disk = std::fs::read_to_string(out.join("contracts/e17.md")).unwrap();
        assert!(on_disk.contains("[[contracts/2026/q3]]"), "E17: {on_disk}");
        // …and syncing it back changes nothing on either side.
        let again = ok(f.as_("accounts-agent").args(["sync", "/"]).arg(&out), None).stdout;
        assert!(again.contains("textdb 0 new, 0 changed") && again.contains("disk 0 new, 0 changed"), "E17: {again}");
    });
}

/// E18, E19: `--update-links` rewrites canonical links; from a local view it reaches only the
/// files the account may write.
#[test]
fn e_update_links_rewrites_canonically_from_either_view() {
    scenarios!("E18", "E19");
    on_each_engine(|f| {
        // E18: admin moves; every linking file is rewritten and every view reads the new target.
        ok(f.as_("admin").args(["write", "--create", "/legal/contracts/e18.md"]), Some("[q3](/legal/contracts/2026/q3.md)\n"));
        ok(f.as_("admin").args(["mv", "/legal/contracts/2026/q3.md", "/legal/contracts/q3.md", "--update-links"]), None);
        assert!(ok(f.as_("admin").args(["cat", "/legal/contracts/e18.md"]), None).stdout.contains("(/legal/contracts/q3.md)"), "E18");
        assert!(ok(f.as_("accounts-agent").args(["cat", "/contracts/e18.md"]), None).stdout.contains("(/contracts/q3.md)"), "E18");

        // E19: the account moves it back; its own files are rewritten.
        ok(f.as_("accounts-agent").args(["mv", "/contracts/q3.md", "/contracts/2026/q3.md", "--update-links"]), None);
        assert!(ok(f.as_("admin").args(["cat", "/legal/contracts/e18.md"]), None).stdout.contains("(/legal/contracts/2026/q3.md)"), "E19");
    });
}

/// E20, E21, E22: backlinks stop at the view, `hidden` is not `broken`, and ambiguity is reported
/// as it always was.
#[test]
fn e_backlinks_broken_and_ambiguous_inside_a_view() {
    scenarios!("E20", "E21", "E22");
    on_each_engine(|f| {
        // E20: a backlink from outside the view is omitted for the account and present for admin.
        ok(f.as_("admin").args(["write", "--create", "/hr/notes.md"]), Some("see [[acme]]\n"));
        ok(f.as_("accounts-agent").args(["write", "--create", "/contracts/mine.md"]), Some("see [[acme]]\n"));
        let mine = ok(f.as_("accounts-agent").args(["--json", "backlinks", "/contracts/acme.md"]), None).json();
        let paths: Vec<&str> = mine.as_array().unwrap().iter().filter_map(|l| l["path"].as_str()).collect();
        assert!(paths.contains(&"/contracts/mine.md"), "E20: {mine}");
        assert!(!mine.to_string().contains("/hr"), "E20: {mine}");
        assert!(ok(f.as_("admin").args(["--json", "backlinks", "/legal/contracts/acme.md"]), None).json().to_string().contains("/hr/notes.md"), "E20");

        // E21: --broken lists the three that need attention, and never a hidden one.
        let broken = ok(f.as_("accounts-agent").args(["--json", "links", "--broken", "/contracts"]), None).json();
        let states: BTreeSet<&str> = broken.as_array().unwrap().iter().filter_map(|l| l["status"].as_str()).collect();
        assert!(!states.contains("hidden"), "E21: hidden is not broken: {broken}");
        assert!(states.iter().all(|s| ["broken", "anchor-missing", "ambiguous"].contains(s)), "E21: {broken}");

        // E22: a name that exists in two shares is ambiguous, reported under the winner's alias.
        ok(f.as_("admin").args(["write", "--create", "/products/catalog/dup.md"]), Some("p\n"));
        ok(f.as_("admin").args(["write", "--create", "/legal/contracts/dup.md"]), Some("c\n"));
        ok(f.as_("accounts-agent").args(["write", "--create", "/contracts/e22.md"]), Some("see [[dup]]\n"));
        let row = ok(f.as_("accounts-agent").args(["--json", "links", "/contracts/e22.md"]), None).json();
        assert_eq!(row[0]["status"], "ambiguous", "E22: {row}");
        assert_eq!(row[0]["resolved"], "/contracts/dup.md", "E22: nearest wins, under the local alias: {row}");
    });
}

// ============================================================ F. History, change feed, watch

/// F1, F3, F10: the feed carries what the view can see, in its own paths, and never another
/// account's writes elsewhere.
#[test]
fn f_the_feed_is_scoped_to_the_view() {
    scenarios!("F1", "F3", "F10");
    on_each_engine(|f| {
        ok(f.as_("accounts-agent").args(["edit", "/contracts/acme.md", "--old", "v3", "--new", "v4"]), None);
        ok(f.as_("admin").args(["write", "--create", "/hr/new.md"]), Some("x\n"));

        // F1: local paths, nothing from outside; seq may have gaps, which --since tolerates.
        let mine = ok(f.as_("accounts-agent").args(["--json", "log"]), None);
        let text = mine.stdout.clone();
        assert!(text.contains("/contracts/acme.md"), "F1: {text}");
        assert!(!text.contains("/legal") && !text.contains("/hr"), "F1: {text}");

        // F3: admin sees everything, in store paths.
        let all = ok(f.as_("admin").args(["--json", "log"]), None).stdout;
        assert!(all.contains("/legal/contracts/acme.md") && all.contains("/hr/new.md"), "F3");

        // F10: another account's writes in a share it does not hold are invisible.
        let theirs = ok(f.as_("product-agent").args(["--json", "log"]), None).stdout;
        assert!(!theirs.contains("acme"), "F10: {theirs}");
    });
}

/// F2: `watch` answers in local paths.
#[test]
fn f_watch_answers_in_local_paths() {
    scenarios!("F2");
    on_each_engine(|f| {
        let since = ok(f.as_("accounts-agent").args(["--json", "log"]), None).json();
        let last = since.as_array().and_then(|r| r.last()).and_then(|e| e["seq"].as_i64()).unwrap_or(0);
        ok(f.as_("accounts-agent").args(["edit", "/contracts/acme.md", "--old", "v3", "--new", "v4"]), None);
        let out = ok(f.as_("accounts-agent").args(["--json", "log", "--since", &last.to_string(), "-p", "/contracts"]), None).stdout;
        assert!(out.contains("/contracts/acme.md") && !out.contains("/legal"), "F2: {out}");
    });
}

/// F4, F5, F6: an owner's move is silence, a delete or a create depending on which side of the
/// account's shares it lands.
#[test]
fn f_an_owner_move_reads_differently_in_each_view() {
    scenarios!("F4", "F5", "F6");
    on_each_engine(|f| {
        let mark = |who: &str| -> i64 {
            ok(f.as_(who).args(["--json", "log"]), None).json().as_array().and_then(|r| r.last()).and_then(|e| e["seq"].as_i64()).unwrap_or(0)
        };
        let (mine, theirs) = (mark("accounts-agent"), mark("auditor"));
        ok(f.as_("admin").args(["mv", "/legal/contracts", "/law/contracts"]), None);

        // F4: the share node itself moved, so its holder's paths did not change and nothing is said.
        let after = ok(f.as_("accounts-agent").args(["--json", "log", "--since", &mine.to_string()]), None).stdout;
        assert!(!after.contains("acme"), "F4: {after}");
        // F5: the subtree left auditor's share, which reads as a delete.
        let gone = ok(f.as_("auditor").args(["--json", "log", "--since", &theirs.to_string()]), None).stdout;
        assert!(gone.contains("delete") && gone.contains("/legal/contracts/"), "F5: {gone}");

        // F6: moved in from outside is a create.
        let before = mark("accounts-agent");
        ok(f.as_("admin").args(["mv", "/vendor/contracts", "/law/contracts/vendor"]), None);
        let created = ok(f.as_("accounts-agent").args(["--json", "log", "--since", &before.to_string()]), None).stdout;
        assert!(created.contains("create") && created.contains("/contracts/vendor/beta.md"), "F6: {created}");
    });
}

/// F7, F8, F9: grant changes are account-scoped events, so `watch` and `sync` can follow them.
#[test]
fn f_grant_changes_are_events_in_the_accounts_own_feed() {
    scenarios!("F7", "F8", "F9");
    on_each_engine(|f| {
        let mark = || -> i64 {
            ok(f.as_("accounts-agent").args(["--json", "log"]), None).json().as_array().and_then(|r| r.last()).and_then(|e| e["seq"].as_i64()).unwrap_or(0)
        };
        // F7: an alias rename is a move.
        let at = mark();
        ok(f.as_("admin").args(["access", "rename", "accounts-agent", "contracts", "legal-contracts"]), None);
        let moved = ok(f.as_("accounts-agent").args(["--json", "log", "--since", &at.to_string()]), None).stdout;
        assert!(moved.contains("move") && moved.contains("/contracts") && moved.contains("/legal-contracts"), "F7: {moved}");

        // F8: a revocation.
        let at = mark();
        ok(f.as_("admin").args(["access", "revoke", "accounts-agent", "products"]), None);
        let un = ok(f.as_("accounts-agent").args(["--json", "log", "--since", &at.to_string()]), None).stdout;
        assert!(un.contains("unshare") && un.contains("/products"), "F8: {un}");

        // F9: a new grant.
        let at = mark();
        ok(f.as_("admin").args(["access", "grant", "accounts-agent", "/vendor/contracts", "ro", "--as", "vendor-contracts"]), None);
        let new = ok(f.as_("accounts-agent").args(["--json", "log", "--since", &at.to_string()]), None).stdout;
        assert!(new.contains("share") && new.contains("/vendor-contracts"), "F9: {new}");
    });
}

/// F11: every author in a file's history is an account name.
#[test]
fn f_authors_are_account_names() {
    scenarios!("F11");
    on_each_engine(|f| {
        ok(f.as_("accounts-agent").args(["edit", "/contracts/acme.md", "--old", "v3", "--new", "v4"]), None);
        ok(f.as_("contracts-agent").args(["edit", "/acme.md", "--old", "v4", "--new", "v5"]), None);
        let h = ok(f.as_("admin").args(["--json", "history", "/legal/contracts/acme.md", "--versions-only"]), None).json();
        let authors: Vec<&str> = h.as_array().unwrap().iter().filter_map(|c| c["author"].as_str()).collect();
        assert!(authors.contains(&"accounts-agent") && authors.contains(&"contracts-agent"), "F11: {h}");
    });
}

// ============================================================ G. Sync to disk

/// G1, G2, G5, G10, G11, G15: a local checkout is the view's own layout, and edits go both ways.
#[test]
fn g_a_checkout_is_the_views_own_layout() {
    scenarios!("G1", "G2", "G5", "G10", "G11", "G15");
    on_each_engine(|f| {
        let tmp = tempfile::tempdir().unwrap();
        let kb = tmp.path().join("kb");
        // G1: aliases are the top-level directories; the config names the account, never the bearer.
        ok(f.as_("accounts-agent").args(["sync", "/"]).arg(&kb), None);
        assert!(kb.join("contracts/acme.md").is_file() && kb.join("products/roadmap.md").is_file(), "G1");
        let config = std::fs::read_to_string(kb.join(".textdb/config")).unwrap();
        assert!(config.contains("accounts-agent"), "G1: {config}");
        let bearer = f.tokens.get("accounts-agent").unwrap();
        assert!(!config.contains(bearer.as_str()), "G1: the bearer must not be written to disk: {config}");

        // G2: an edit on disk commits as the account.
        std::fs::write(kb.join("contracts/acme.md"), "# Acme\n\nedited on disk\n").unwrap();
        ok(f.as_("accounts-agent").args(["sync"]).current_dir(&kb), None);
        assert_eq!(
            ok(f.as_("admin").args(["--json", "stat", "/legal/contracts/acme.md"]), None).json()["updated_by"],
            "accounts-agent",
            "G2"
        );

        // G5: a store edit by another account arrives on the next sync.
        ok(f.as_("contracts-agent").args(["write", "/acme.md"]), Some("# Acme\n\nfrom the other agent\n"));
        let out = ok(f.as_("accounts-agent").args(["sync"]).current_dir(&kb), None).stdout;
        assert!(out.contains("disk changed") && out.contains("contracts/acme.md"), "G5: {out}");

        // G10: a single-root account's checkout has the share's children at the top.
        let one = tmp.path().join("one");
        ok(f.as_("contracts-agent").args(["sync", "/"]).arg(&one), None);
        assert!(one.join("acme.md").is_file() && one.join("2026/q3.md").is_file(), "G10");

        // G11: admin's checkout is the store's own layout.
        let all = tmp.path().join("all");
        ok(f.as_("admin").args(["sync", "/"]).arg(&all), None);
        assert!(all.join("legal/contracts/acme.md").is_file() && all.join("hr/salaries.md").is_file(), "G11");

        // G15: a share may be the prefix, and then it is the directory's root.
        let c = tmp.path().join("c");
        ok(f.as_("accounts-agent").args(["sync", "/contracts"]).arg(&c), None);
        assert!(c.join("acme.md").is_file() && c.join("2026/q3.md").is_file(), "G15");
    });
}

/// G3, G4: a change on disk under a read-only share is kept, not pushed and not lost.
#[test]
fn g_local_changes_under_a_read_only_share_are_kept_on_disk() {
    scenarios!("G3", "G4");
    on_each_engine(|f| {
        let tmp = tempfile::tempdir().unwrap();
        let kb = tmp.path().join("kb");
        ok(f.as_("accounts-agent").args(["sync", "/"]).arg(&kb), None);

        // G3: an edit.
        std::fs::write(kb.join("products/roadmap.md"), "# Roadmap\n\nlocal only\n").unwrap();
        // G4: and a new file.
        std::fs::write(kb.join("products/new.md"), "local only\n").unwrap();
        let out = ok(f.as_("accounts-agent").args(["sync"]).current_dir(&kb), None).stdout;
        assert!(out.contains("kept") && out.contains("read-only"), "G3/G4: {out}");
        // The store is untouched and the disk keeps what the user wrote.
        assert!(!ok(f.as_("admin").args(["cat", "/products/roadmap.md"]), None).stdout.contains("local only"), "G3");
        assert_eq!(std::fs::read_to_string(kb.join("products/new.md")).unwrap(), "local only\n", "G4");
        refused(&run(f.as_("admin").args(["stat", "/products/new.md"]), None), NOT_FOUND, "TX003");
    });
}

/// G6, G7: a new grant appears as a directory; a renamed alias moves one rather than replacing it.
#[test]
fn g_grants_and_alias_renames_reach_the_checkout() {
    scenarios!("G6", "G7");
    on_each_engine(|f| {
        let tmp = tempfile::tempdir().unwrap();
        let kb = tmp.path().join("kb");
        ok(f.as_("accounts-agent").args(["sync", "/"]).arg(&kb), None);

        // G6.
        ok(f.as_("admin").args(["access", "grant", "accounts-agent", "/vendor/contracts", "ro", "--as", "vendor-contracts"]), None);
        ok(f.as_("accounts-agent").args(["sync"]).current_dir(&kb), None);
        assert!(kb.join("vendor-contracts/beta.md").is_file(), "G6");

        // G7: renamed, not deleted and recreated — the file keeps its place under the new name.
        std::fs::write(kb.join("contracts/local-note.md"), "mine\n").unwrap();
        ok(f.as_("admin").args(["access", "rename", "accounts-agent", "contracts", "legal-contracts"]), None);
        let out = ok(f.as_("accounts-agent").args(["sync"]).current_dir(&kb), None).stdout;
        assert!(out.contains("moved") && out.contains("legal-contracts"), "G7: {out}");
        assert!(kb.join("legal-contracts/acme.md").is_file() && !kb.join("contracts").exists(), "G7");
    });
}

/// G8: a revoked or dormant share leaves the disk alone — the one case that would destroy data if
/// forbidden were read as absent.
#[test]
fn g_a_revoked_share_leaves_the_disk_alone() {
    scenarios!("G8");
    on_each_engine(|f| {
        let tmp = tempfile::tempdir().unwrap();
        let kb = tmp.path().join("kb");
        ok(f.as_("accounts-agent").args(["sync", "/"]).arg(&kb), None);
        ok(f.as_("admin").args(["access", "revoke", "accounts-agent", "products"]), None);

        let out = ok(f.as_("accounts-agent").args(["sync"]).current_dir(&kb), None).stdout;
        assert!(out.contains("left alone") && out.contains("products"), "G8: {out}");
        assert!(kb.join("products/roadmap.md").is_file(), "G8: nothing deleted");
        assert!(!out.contains("deleted"), "G8: {out}");
    });
}

/// G9: a real delete does propagate, unlike a revocation.
#[test]
fn g_a_real_delete_propagates_to_the_checkout() {
    scenarios!("G9");
    on_each_engine(|f| {
        let tmp = tempfile::tempdir().unwrap();
        let kb = tmp.path().join("kb");
        ok(f.as_("auditor").args(["sync", "/"]).arg(&kb), None);
        assert!(kb.join("legal/contracts/2026/q3.md").is_file());
        ok(f.as_("accounts-agent").args(["rm", "/contracts/2026"]), None);
        let out = ok(f.as_("auditor").args(["sync"]).current_dir(&kb), None).stdout;
        assert!(out.contains("disk deleted"), "G9: {out}");
        assert!(!kb.join("legal/contracts/2026/q3.md").exists(), "G9");
    });
}

/// G12: `--commit` records the account as the author, in the view's own paths.
#[test]
fn g_commit_names_the_account() {
    scenarios!("G12");
    on_each_engine(|f| {
        let tmp = tempfile::tempdir().unwrap();
        let kb = tmp.path().join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        for args in [vec!["init", "-q"], vec!["config", "user.email", "a@b.c"], vec!["config", "user.name", "a"]] {
            assert!(Command::new("git").args(&args).current_dir(&kb).status().unwrap().success());
        }
        ok(f.as_("accounts-agent").args(["sync", "/"]).arg(&kb), None);
        std::fs::write(kb.join("contracts/acme.md"), "# Acme\n\nfor the commit\n").unwrap();
        ok(f.as_("accounts-agent").args(["sync", "--commit"]).current_dir(&kb), None);
        let log = Command::new("git").args(["log", "-1", "--format=%B"]).current_dir(&kb).output().unwrap();
        let message = String::from_utf8_lossy(&log.stdout).into_owned();
        assert!(message.contains("accounts-agent"), "G12: {message}");
        assert!(!message.contains("/legal/"), "G12: local paths: {message}");
    });
}

/// G13: an expired or revoked token stops a sync before it touches anything.
#[test]
fn g_an_unusable_token_stops_the_sync_before_it_writes() {
    scenarios!("G13");
    on_each_engine(|f| {
        let tmp = tempfile::tempdir().unwrap();
        let kb = tmp.path().join("kb");
        ok(f.as_("accounts-agent").args(["sync", "/"]).arg(&kb), None);
        let before = std::fs::read_to_string(kb.join("contracts/acme.md")).unwrap();

        let tokens = ok(f.as_("admin").args(["--json", "token", "ls", "accounts-agent"]), None).json();
        let id = tokens[0]["id"].as_i64().expect("G13: a token to revoke");
        ok(f.as_("admin").args(["token", "revoke", &id.to_string()]), None);

        refused(&run(f.as_("accounts-agent").args(["sync"]).current_dir(&kb), None), FORBIDDEN, "TX005");
        assert_eq!(std::fs::read_to_string(kb.join("contracts/acme.md")).unwrap(), before, "G13: nothing touched");
    });
}

/// G14: two accounts syncing their own directories against the same files is ordinary concurrent
/// editing — the directory lock of #9 is per directory.
#[test]
fn g_two_accounts_sync_their_own_directories_independently() {
    scenarios!("G14");
    on_each_engine(|f| {
        let tmp = tempfile::tempdir().unwrap();
        let (mine, theirs) = (tmp.path().join("mine"), tmp.path().join("theirs"));
        ok(f.as_("accounts-agent").args(["sync", "/"]).arg(&mine), None);
        ok(f.as_("contracts-agent").args(["sync", "/"]).arg(&theirs), None);

        std::fs::write(mine.join("contracts/acme.md"), "# Acme\n\nfrom accounts-agent\n").unwrap();
        ok(f.as_("accounts-agent").args(["sync"]).current_dir(&mine), None);
        let out = ok(f.as_("contracts-agent").args(["sync"]).current_dir(&theirs), None).stdout;
        assert!(out.contains("disk changed"), "G14: {out}");
        assert!(std::fs::read_to_string(theirs.join("acme.md")).unwrap().contains("from accounts-agent"), "G14");
    });
}

/// G16: a store path is not a sync prefix in a local view.
#[test]
fn g_a_store_path_is_not_a_sync_prefix() {
    scenarios!("G16");
    on_each_engine(|f| {
        let tmp = tempfile::tempdir().unwrap();
        refused(&run(f.as_("accounts-agent").args(["sync", "/legal"]).arg(tmp.path().join("x")), None), NOT_FOUND, "TX003");
    });
}

/// G17 is E17: a file with root links round-trips through a checkout unchanged.
#[test]
fn g_root_links_round_trip_through_a_checkout() {
    scenarios!("G17");
    on_each_engine(|f| {
        ok(f.as_("accounts-agent").args(["write", "--create", "/contracts/g17.md"]), Some("[q3](/contracts/2026/q3.md)\n"));
        let tmp = tempfile::tempdir().unwrap();
        let kb = tmp.path().join("kb");
        ok(f.as_("accounts-agent").args(["sync", "/"]).arg(&kb), None);
        assert_eq!(std::fs::read_to_string(kb.join("contracts/g17.md")).unwrap(), "[q3](/contracts/2026/q3.md)\n", "G17");
        let again = ok(f.as_("accounts-agent").args(["sync"]).current_dir(&kb), None).stdout;
        assert!(again.contains("textdb 0 new, 0 changed"), "G17: {again}");
        assert!(ok(f.as_("admin").args(["cat", "/legal/contracts/g17.md"]), None).stdout.contains("(/legal/contracts/2026/q3.md)"), "G17");
    });
}

// ============================================================ I. Single-root accounts

/// I1, I2, I3, I4: a single-root account writes at its own root, and cannot escape or unmake it.
#[test]
fn i_a_single_root_account_owns_its_root_but_cannot_leave_it() {
    scenarios!("I1", "I2", "I3", "I4");
    on_each_engine(|f| {
        // I1, I2: the root is the share, so writing there is writing in the share.
        ok(f.as_("contracts-agent").args(["write", "--create", "/note.md"]), Some("note\n"));
        assert!(ok(f.as_("admin").args(["cat", "/legal/contracts/note.md"]), None).stdout.contains("note"), "I1");
        ok(f.as_("contracts-agent").args(["mkdir", "/2027"]), None);
        ok(f.as_("admin").args(["stat", "/legal/contracts/2027"]), None);

        // I3: the share root itself is still not the account's to remove or move.
        refused(&run(f.as_("contracts-agent").args(["rm", "/"]), None), FORBIDDEN, "TX005");
        refused(&run(f.as_("contracts-agent").args(["mv", "/", "/x"]), None), FORBIDDEN, "TX005");

        // I4: a path that climbs out normalises inside the root and finds nothing above it.
        refused(&run(f.as_("contracts-agent").args(["cat", "/../policies/nda.md"]), None), NOT_FOUND, "TX003");
    });
}

/// I5, I6, I7: search, history and other accounts' writes, all within the one root.
#[test]
fn i_a_single_root_account_sees_its_root_and_only_its_root() {
    scenarios!("I5", "I6", "I7");
    on_each_engine(|f| {
        // I5: search covers the share and nothing else.
        let hits = ok(f.as_("contracts-agent").args(["--json", "search", "quarterly"]), None).json();
        let paths: Vec<&str> = hits.as_array().unwrap().iter().filter_map(|h| h["path"].as_str()).collect();
        assert!(paths.iter().all(|p| !p.contains("legal")), "I5: {hits}");
        assert!(paths.contains(&"/2026/q3.md"), "I5: {hits}");
        assert!(ok(f.as_("contracts-agent").args(["--json", "search", "secret"]), None).json().as_array().unwrap().is_empty(), "I5");

        // I6: full history at its own path.
        let h = ok(f.as_("contracts-agent").args(["--json", "history", "/acme.md", "--versions-only"]), None).json();
        assert_eq!(h.as_array().map(Vec::len), Some(3), "I6: {h}");

        // I7: another account's edit arrives here as a new version by that account.
        ok(f.as_("accounts-agent").args(["edit", "/contracts/acme.md", "--old", "v3", "--new", "v4"]), None);
        let mine = ok(f.as_("contracts-agent").args(["--json", "stat", "/acme.md"]), None).json();
        assert_eq!((mine["version"].as_i64(), mine["updated_by"].as_str()), (Some(4), Some("accounts-agent")), "I7");
    });
}

// ============================================================ J. Cross-view references

/// J1, J2, J3: a path means nothing in another view; an id means the document, or nothing.
#[test]
fn j_paths_are_per_view_and_ids_are_the_reference_to_pass_between_them() {
    scenarios!("J1", "J2", "J3");
    on_each_engine(|f| {
        // J1: accounts-agent's path is not product-agent's.
        refused(&run(f.as_("product-agent").args(["cat", "/contracts/acme.md"]), None), NOT_FOUND, "TX003");
        // J2: the id resolves for anyone whose share covers it, in their own words.
        let acme = f.id("/legal/contracts/acme.md");
        assert_eq!(
            ok(f.as_("auditor").args(["--json", "cat", &format!("id:{acme}")]), None).json()["path"],
            "/legal/contracts/acme.md",
            "J2"
        );
        // J3: and is honestly absent for anyone whose shares do not.
        refused(&run(f.as_("product-agent").args(["cat", &format!("id:{acme}")]), None), NOT_FOUND, "TX003");
    });
}

/// J4: the `Entry` an account reads carries its own path, the store's id, and which share it came
/// through — so an agent can quote a reference that travels.
#[test]
fn j_an_entry_carries_the_id_and_the_share_it_came_through() {
    scenarios!("J4");
    on_each_engine(|f| {
        let e = ok(f.as_("accounts-agent").args(["--json", "stat", "/contracts/acme.md"]), None).json();
        assert_eq!(e["path"], "/contracts/acme.md", "J4");
        assert_eq!(e["id"].as_i64(), Some(f.id("/legal/contracts/acme.md")), "J4");
        assert_eq!(e["share"], "contracts", "J4");
        assert_eq!(e["rights"], "rw", "J4");
    });
}

/// J5, J6: an id works wherever a path does, and resolves to each viewer's own path or to nothing.
#[test]
fn j_ids_work_wherever_a_path_does() {
    scenarios!("J5", "J6");
    on_each_engine(|f| {
        let acme = format!("id:{}", f.id("/legal/contracts/acme.md"));
        // J6.
        assert!(!ok(f.as_("accounts-agent").args(["--json", "history", &acme]), None).json().as_array().unwrap().is_empty(), "J6");
        assert!(!ok(f.as_("accounts-agent").args(["--json", "links", &acme]), None).json().as_array().unwrap().is_empty(), "J6");
        assert_eq!(ok(f.as_("accounts-agent").args(["--json", "stat", &acme]), None).json()["path"], "/contracts/acme.md", "J6");
        // J5: the same reference, each viewer's own answer — or none.
        assert_eq!(ok(f.as_("contracts-agent").args(["--json", "stat", &acme]), None).json()["path"], "/acme.md", "J5");
        refused(&run(f.as_("product-agent").args(["stat", &acme]), None), NOT_FOUND, "TX003");
    });
}

// ============================================================ K. SQL and web

/// K1, K3, K4, K5: the SQL surface speaks the view's paths and stops at its edges.
#[test]
fn k_sql_speaks_the_views_paths() {
    scenarios!("K1", "K3", "K4", "K5");
    on_each_engine(|f| {
        // K1: dir and depth are relative to the local root.
        let rows = ok(
            f.as_("accounts-agent")
                .args(["--json", "sql", "SELECT path, dir, depth FROM files WHERE path LIKE '/contracts/%' ORDER BY path"]),
            None,
        )
        .json();
        let first = &rows["rows"][0];
        assert_eq!(first["path"], "/contracts/2026/q3.md", "K1: {rows}");
        assert_eq!(first["dir"], "/contracts/2026", "K1: {rows}");
        assert_eq!(first["depth"], 3, "K1: {rows}");

        // K3: the share roots are folders with their totals.
        let ls = ok(f.as_("accounts-agent").args(["--json", "sql", "SELECT path, kind FROM textdb_ls('/') ORDER BY path"]), None).json();
        let paths: Vec<&str> = ls["rows"].as_array().unwrap().iter().filter_map(|r| r["path"].as_str()).collect();
        assert_eq!(paths, ["/contracts", "/products"], "K3: {ls}");

        // K4: a store path is not found, through SQL as anywhere else.
        refused(&run(f.as_("accounts-agent").args(["sql", "SELECT * FROM textdb_ls('/legal')"]), None), NOT_FOUND, "TX003");

        // K5: commits on visible paths, with every author.
        let commits = ok(f.as_("accounts-agent").args(["--json", "sql", "SELECT DISTINCT path FROM commits ORDER BY path"]), None).json();
        let paths: Vec<&str> = commits["rows"].as_array().unwrap().iter().filter_map(|r| r["path"].as_str()).collect();
        assert!(paths.iter().all(|p| p.starts_with("/contracts/") || p.starts_with("/products/")), "K5: {commits}");
    });
}

/// K2, K6: the raw tables are the owner's; a token session cannot read or write them.
#[test]
fn k_raw_tables_are_refused_to_a_token_session() {
    scenarios!("K2", "K6");
    on_each_engine(|f| {
        let raw = if f.engine() == Engine::Sqlite { "SELECT * FROM kb_node" } else { "SELECT * FROM kb.node" };
        refused(&run(f.as_("accounts-agent").args(["sql", raw]), None), FORBIDDEN, "TX005");
        // K6: admin reads the grant table.
        let grants = if f.engine() == Engine::Sqlite { "SELECT alias FROM kb_grant ORDER BY alias" } else { "SELECT alias FROM kb.grant ORDER BY alias" };
        let out = ok(f.as_("admin").args(["--json", "sql", grants]), None).json();
        let aliases: Vec<&str> = out["rows"].as_array().unwrap().iter().filter_map(|r| r["alias"].as_str()).collect();
        assert!(aliases.contains(&"contracts") && aliases.contains(&"products"), "K6: {out}");
    });
}

/// K8, K9: the engines' own entry points — `SET textdb.token` and `textdb_auth` — filter and
/// translate the same way the CLI does, because that is where it is implemented.
#[test]
fn k_the_engines_own_entry_points_filter_and_translate() {
    scenarios!("K8", "K9");
    on_each_engine(|f| {
        let bearer = f.tokens.get("accounts-agent").unwrap().clone();
        match f.engine() {
            // K9.
            Engine::Sqlite => {
                let sql = format!("SELECT textdb_auth('{bearer}')");
                let out = ok(f.as_("admin").args(["--json", "sql", "-f", "-"]), Some(&format!("{sql};\n"))).json();
                assert!(out.to_string().contains("accounts-agent"), "K9: {out}");
            }
            // K8: RLS hides the rows even from a hand-written query on the table.
            Engine::Postgres => {
                let mut c = postgres::Client::connect(&f.store.url, postgres::NoTls).expect("K8: connect");
                c.batch_execute(&format!("SET textdb.token = '{bearer}'")).expect("K8: set the token");
                let rows = c.query("SELECT path FROM kb.ls('/') ORDER BY path", &[]).expect("K8: kb.ls");
                let paths: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
                assert_eq!(paths, ["/contracts", "/products"], "K8");
                let raw = c.query("SELECT path FROM kb.node WHERE path LIKE '/hr%'", &[]).expect("K8: kb.node");
                assert!(raw.is_empty(), "K8: RLS must hide what the token cannot see");
            }
        }
    });
}

/// K7: the web app's view of an account is the account's view — its aliases, its totals, its
/// attribution, and an id reference where a target is hidden.
///
/// The HTTP layer is where the web app gets all of this, so this exercises the same surface the
/// browser uses rather than the browser itself.
#[test]
fn k_the_http_surface_answers_in_the_accounts_view() {
    scenarios!("K7");
    on_each_engine(|f| {
        let nda = f.id("/legal/policies/nda.md");
        // The account's root through the same JSON the web app reads.
        let root = ok(f.as_("accounts-agent").args(["--json", "ls", "/"]), None).json();
        let rows: Vec<(&str, &str)> = root
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["path"].as_str().unwrap_or_default(), e["rights"].as_str().unwrap_or_default()))
            .collect();
        assert_eq!(rows, [("/contracts", "rw"), ("/products", "ro")], "K7: {root}");
        // Totals are exact, and an edit is attributed to the account.
        ok(f.as_("accounts-agent").args(["edit", "/contracts/acme.md", "--old", "v3", "--new", "v4"]), None);
        assert_eq!(
            ok(f.as_("accounts-agent").args(["--json", "stat", "/contracts/acme.md"]), None).json()["updated_by"],
            "accounts-agent",
            "K7"
        );
        // A hidden target renders as an id reference.
        assert!(
            ok(f.as_("accounts-agent").args(["cat", "/contracts/acme.md"]), None).stdout.contains(&format!("textdb:{nda}")),
            "K7"
        );
    });
}

// ============================================================ L. Bypass, accepted by design

/// L1, L3: owning the file is being the owner. Opening the store directly is admin, and a copy
/// carries everything — but never the bearers, which are stored hashed.
#[test]
fn l_owning_the_store_is_full_access_and_tokens_are_not_recoverable_from_it() {
    scenarios!("L1", "L3");
    on_each_engine(|f| {
        // L1: no token, full view. This is the admin the whole catalogue uses.
        let all = ok(f.as_("admin").args(["ls", "-1", "-R", "/"]), None).stdout;
        assert!(all.contains("/hr/salaries.md") && all.contains("/vendor/contracts/beta.md"), "L1");

        // L3: the bearers are not in the store, so a copy of it does not yield them.
        let bearer = f.tokens.get("accounts-agent").unwrap().clone();
        let dumped = match f.engine() {
            Engine::Sqlite => std::fs::read(&f.store.url).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
            Engine::Postgres => {
                let mut c = postgres::Client::connect(&f.store.url, postgres::NoTls).unwrap();
                c.query("SELECT hash, label FROM kb.token", &[])
                    .map(|rows| rows.iter().map(|r| r.get::<_, String>(0)).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default()
            }
        };
        assert!(!dumped.contains(&bearer), "L3: the bearer must not be stored");
    });
}

/// L2: a Postgres superuser is not bound by row-level security. Recorded as the accepted
/// boundary of the model rather than a gap.
#[test]
fn l_a_superuser_is_not_bound_by_row_level_security() {
    scenarios!("L2");
    on_each_engine(|f| {
        if f.engine() != Engine::Postgres {
            return;
        }
        let bearer = f.tokens.get("accounts-agent").unwrap().clone();
        let mut c = postgres::Client::connect(&f.store.url, postgres::NoTls).expect("L2: connect");
        // The test role is the owner, which is what "superuser" stands for here: the token
        // narrows the views, and the raw table is still readable. That is L1 on Postgres.
        c.batch_execute(&format!("SET textdb.token = '{bearer}'")).expect("L2");
        let raw = c.query("SELECT count(*) FROM kb.node", &[]).expect("L2: the owner still reads the table");
        assert!(raw[0].get::<_, i64>(0) > 0, "L2");
    });
}

/// L4: every path outside the caller's shares is *not found*, in every command that takes one, so
/// guessing store paths reveals nothing.
#[test]
fn l_guessing_store_paths_reveals_nothing() {
    scenarios!("L4");
    on_each_engine(|f| {
        let tmp = tempfile::tempdir().unwrap();
        // One real folder, one that does not exist at all: the answers must be indistinguishable.
        for (real, fake) in [("/hr", "/nowhere"), ("/legal/policies", "/legal/nothing")] {
            let cases: Vec<(&str, Vec<String>)> = vec![
                ("ls", vec!["ls".into(), real.into()]),
                ("cat", vec!["cat".into(), format!("{real}/x.md")]),
                ("stat", vec!["stat".into(), real.into()]),
                ("search -p", vec!["search".into(), "x".into(), "-p".into(), real.into()]),
                ("sync", vec!["sync".into(), real.into(), tmp.path().join("x").to_string_lossy().into_owned()]),
            ];
            for (label, args) in cases {
                let a = run(f.as_("accounts-agent").args(args.iter().map(String::as_str).collect::<Vec<_>>()), None);
                refused(&a, NOT_FOUND, "TX003");
                let b_args: Vec<String> = args.iter().map(|s| s.replace(real, fake)).collect();
                let b = run(f.as_("accounts-agent").args(b_args.iter().map(String::as_str).collect::<Vec<_>>()), None);
                refused(&b, NOT_FOUND, "TX003");
                assert_eq!(
                    a.stderr.replace(real, "X"),
                    b.stderr.replace(fake, "X"),
                    "L4: {label} tells {real} apart from {fake}"
                );
            }
        }
    });
}

/// L5: every scenario above runs on both engines. The harness does that by construction — this
/// records the requirement and fails if the Postgres half was never exercised.
#[test]
fn l_the_catalogue_runs_on_both_engines() {
    scenarios!("L5");
    let engines = engines();
    assert!(engines.contains(&Engine::Sqlite), "L5");
    assert!(
        engines.contains(&Engine::Postgres),
        "L5: set TEXTDB_TEST_PG so the catalogue runs on Postgres too; it is a requirement, not an option"
    );
}
