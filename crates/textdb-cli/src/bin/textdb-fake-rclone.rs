//! A stand-in for `rclone`, for tests and for working on the rclone driver without an account.
//!
//! Not shipped: it exists so the driver's provider-dependent paths can be exercised. Point
//! `TEXTDB_RCLONE` at it and give it a directory to keep the "remote" in.
//!
//! It is a **fault injector, not a Google Drive emulator.** CI already runs the driver against
//! real rclone on its local backend, which proves the command line and the JSON are right. What
//! that cannot do is misbehave: rclone's local backend always keeps a SHA-256, never rewrites
//! what it stores, and renames atomically. The paths that only run against a real provider —
//! and that carry the most risk — are the ones here:
//!
//! | variable | what it imitates |
//! |---|---|
//! | `TEXTDB_FAKE_RCLONE_NO_SHA256=1` | OneDrive and SharePoint, whose hash is QuickXorHash, so the driver must read the bytes back to hash them |
//! | `TEXTDB_FAKE_RCLONE_REWRITE=1` | SharePoint silently rewriting Office files on upload, so what lands is not what was sent |
//! | `TEXTDB_FAKE_RCLONE_MOVE_GAP=1` | a server-side move that clears the destination and then fails, leaving the asset's path empty |
//! | `TEXTDB_FAKE_RCLONE_LIST_LAG=N` | a provider that does not list a lock file for its first `N` listings |
//!
//! Every one of those is documented behaviour of the real providers, not invention: rclone's
//! OneDrive page records that "Sharepoint … silently modifies uploaded files, mainly Office
//! files (.docx, .xlsx, etc.), causing file size and hash checks to fail" and that QuickXorHash
//! is the default hash; its Drive page records that rclone removes the destination before a
//! server-side move, and that Drive's listings can lag behind writes. What is *not* documented
//! is how often or for how long, so the lag here is a switch to test the code's response with,
//! never a claim about what Drive does.
//!
//! Supported commands, which is exactly what `assets::rclone` calls and no more:
//! `version`, `lsjson` (`--stat`, `--files-only`, `--hash`), `copyto`, `moveto`, `deletefile`,
//! `rcat`, `hashsum sha256 --download`. Exit codes follow rclone: 0 fine, 3 directory not
//! found, 4 file not found, 1 anything else.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sha2::{Digest, Sha256};

/// Where the "remote" lives on disk. Required, so this can never touch a real remote.
const ROOT: &str = "TEXTDB_FAKE_RCLONE_ROOT";
/// Counts of listings that have already hidden a lock file, kept beside the remote.
const STATE: &str = ".textdb-fake-rclone-state.json";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(Fail { code, message }) => {
            if !message.is_empty() {
                eprintln!("{message}");
            }
            ExitCode::from(code)
        }
    }
}

struct Fail {
    code: u8,
    message: String,
}

fn fail(code: u8, message: impl Into<String>) -> Fail {
    Fail { code, message: message.into() }
}

/// rclone's "directory not found", which the driver reads as "nothing is there".
fn no_dir() -> Fail {
    fail(3, "directory not found")
}

/// rclone's "file not found".
fn no_file() -> Fail {
    fail(4, "file not found")
}

fn env_on(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

fn root() -> Result<PathBuf, Fail> {
    match std::env::var_os(ROOT) {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => Err(fail(1, format!("{ROOT} is not set: this stand-in only ever writes into a directory of its own"))),
    }
}

/// Flags that take a separate value, so the value is never mistaken for a path.
const VALUED: &[&str] = &["--contimeout", "--timeout", "--retries", "--low-level-retries", "--hash-type"];

/// Split rclone's arguments into flags and operands, honouring `--`.
fn split(args: &[String]) -> (Vec<&str>, Vec<&str>) {
    let (mut flags, mut operands) = (Vec::new(), Vec::new());
    let (mut after_ddash, mut skip) = (false, false);
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if !after_ddash && a == "--" {
            after_ddash = true;
            continue;
        }
        if !after_ddash && a.starts_with('-') {
            flags.push(a.as_str());
            skip = VALUED.contains(&a.as_str());
            continue;
        }
        operands.push(a.as_str());
    }
    (flags, operands)
}

/// Where an operand lives on disk.
///
/// rclone takes a remote and a local path in one command line (`copyto FILE remote:path`
/// uploads), and tells them apart by a colon before the first slash. So does this.
fn resolve(arg: &str) -> Result<PathBuf, Fail> {
    let head = arg.split('/').next().unwrap_or(arg);
    match head.contains(':').then(|| arg.split_once(':')).flatten() {
        Some((_remote, path)) => Ok(root()?.join(path.trim_start_matches('/'))),
        None => Ok(PathBuf::from(arg)),
    }
}

fn sha256_of(path: &Path) -> Result<String, Fail> {
    let mut file = std::fs::File::open(path).map_err(|_| no_file())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf).map_err(|e| fail(1, format!("reading {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    // Lower-case hex, as `assets::pointer::hex` writes it. Spelled out again because this is a
    // separate binary and the package has no library to share it from.
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// A `lsjson` entry, with only the fields the driver reads.
fn entry(path: &Path, want_hash: bool) -> Result<serde_json::Value, Fail> {
    let meta = std::fs::metadata(path).map_err(|_| no_file())?;
    let is_dir = meta.is_dir();
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    // A Google document has no bytes of its own, and rclone lists it at size -1.
    let size = if is_dir || name.ends_with(".gdoc") { -1 } else { meta.len() as i64 };
    let mut e = serde_json::json!({ "Name": name, "Size": size, "IsDir": is_dir, "Path": name });
    if want_hash {
        let mut hashes = serde_json::Map::new();
        if !is_dir && size >= 0 && !env_on("TEXTDB_FAKE_RCLONE_NO_SHA256") {
            hashes.insert("sha256".into(), sha256_of(path)?.into());
        }
        e["Hashes"] = serde_json::Value::Object(hashes);
    }
    Ok(e)
}

/// Has this lock file been hidden fewer than `LIST_LAG` times already?
///
/// Only lock files: the assumption under test is the one the lock protocol rests on — that the
/// provider lists what was just written. Hiding everything would only stop the driver finding
/// the store's root, which tests nothing.
fn hidden_by_lag(path: &Path) -> bool {
    let lag: u32 = std::env::var("TEXTDB_FAKE_RCLONE_LIST_LAG").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    if lag == 0 || path.extension().is_none_or(|e| e != "lock") {
        return false;
    }
    let Ok(root) = root() else { return false };
    let state = root.join(STATE);
    let mut seen: BTreeMap<String, u32> =
        std::fs::read_to_string(&state).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let key = path.to_string_lossy().into_owned();
    let count = seen.entry(key).or_insert(0);
    if *count >= lag {
        return false;
    }
    *count += 1;
    if let Ok(text) = serde_json::to_string(&seen) {
        let _ = std::fs::write(&state, text);
    }
    true
}

fn run(args: &[String]) -> Result<String, Fail> {
    let (flags, operands) = split(args);
    let command = *operands.first().ok_or_else(|| fail(1, "no command"))?;
    let rest = &operands[1..];
    let want_hash = flags.iter().any(|f| f.starts_with("--hash"));

    match command {
        "version" => Ok("rclone v0.0.0-textdb-fake\n".into()),

        "lsjson" => {
            let target = resolve(rest.first().ok_or_else(|| fail(1, "lsjson needs a path"))?)?;
            if flags.contains(&"--stat") {
                if !target.exists() || hidden_by_lag(&target) {
                    return Err(no_dir());
                }
                return Ok(format!("{}\n", entry(&target, want_hash)?));
            }
            if !target.is_dir() {
                return Err(no_dir());
            }
            let mut names: Vec<PathBuf> =
                std::fs::read_dir(&target).map_err(|_| no_dir())?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            names.sort();
            let mut out = Vec::new();
            for p in names {
                if p.file_name().is_some_and(|n| n == STATE) || hidden_by_lag(&p) {
                    continue;
                }
                if flags.contains(&"--files-only") && p.is_dir() {
                    continue;
                }
                out.push(entry(&p, want_hash)?);
            }
            Ok(format!("{}\n", serde_json::Value::Array(out)))
        }

        "copyto" | "moveto" => {
            let (from, to) = (rest.first().ok_or_else(|| fail(1, "no source"))?, rest.get(1).ok_or_else(|| fail(1, "no destination"))?);
            let (src, dest) = (resolve(from)?, resolve(to)?);
            let mut bytes = std::fs::read(&src).map_err(|_| no_file())?;
            // SharePoint rewrites Office files as it stores them, so what lands is not what was
            // sent and its hash no longer matches the pointer's.
            let office = matches!(dest.extension().and_then(|e| e.to_str()), Some("docx" | "xlsx" | "pptx"));
            if office && env_on("TEXTDB_FAKE_RCLONE_REWRITE") {
                bytes.extend_from_slice(b"\0sharepoint-rewrote-this");
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| fail(1, format!("making {}: {e}", parent.display())))?;
            }
            if command == "moveto" && env_on("TEXTDB_FAKE_RCLONE_MOVE_GAP") {
                // rclone clears the destination before a server-side move; this one then fails,
                // which is the moment the asset is missing from its own path.
                let _ = std::fs::remove_file(&dest);
                return Err(fail(1, "move failed after clearing the destination"));
            }
            std::fs::write(&dest, &bytes).map_err(|e| fail(1, format!("writing {}: {e}", dest.display())))?;
            if command == "moveto" {
                std::fs::remove_file(&src).map_err(|e| fail(1, format!("removing {}: {e}", src.display())))?;
            }
            Ok(String::new())
        }

        "deletefile" => {
            let path = resolve(rest.first().ok_or_else(|| fail(1, "deletefile needs a path"))?)?;
            if !path.exists() {
                return Err(no_file());
            }
            std::fs::remove_file(&path).map_err(|e| fail(1, format!("removing {}: {e}", path.display())))?;
            Ok(String::new())
        }

        "rcat" => {
            let path = resolve(rest.first().ok_or_else(|| fail(1, "rcat needs a path"))?)?;
            let mut body = Vec::new();
            std::io::stdin().read_to_end(&mut body).map_err(|e| fail(1, format!("reading stdin: {e}")))?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| fail(1, format!("making {}: {e}", parent.display())))?;
            }
            std::fs::write(&path, body).map_err(|e| fail(1, format!("writing {}: {e}", path.display())))?;
            Ok(String::new())
        }

        // `hashsum sha256 --download`: what the driver falls back to when the provider keeps no
        // SHA-256 of its own. Reads the bytes, as rclone does, so this answers even under
        // TEXTDB_FAKE_RCLONE_NO_SHA256.
        "hashsum" => {
            let path = resolve(rest.get(1).ok_or_else(|| fail(1, "hashsum needs a path"))?)?;
            if !path.is_file() {
                return Err(no_file());
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            Ok(format!("{}  {name}\n", sha256_of(&path)?))
        }

        other => Err(fail(1, format!("this stand-in does not carry out `{other}`; the driver is not meant to call it"))),
    }
}
