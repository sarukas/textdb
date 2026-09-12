//! `manifest.json`: host, kernel, filesystem, versions, seed, harness commit (test spec §9).

use std::process::Command;

fn sh(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unavailable".into())
}

fn first_match(path: &str, key: &str) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .map(|l| l.splitn(2, ':').nth(1).unwrap_or("").trim().to_string())
        })
        .unwrap_or_else(|| "unavailable".into())
}

fn fs_type(dir: &std::path::Path) -> String {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut best: Option<(usize, String)> = None;
    for l in mounts.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() >= 3 && dir.starts_with(f[1]) {
            let len = f[1].len();
            if best.as_ref().map_or(true, |(b, _)| len > *b) {
                best = Some((len, format!("{} ({})", f[2], f[0])));
            }
        }
    }
    best.map(|(_, s)| s).unwrap_or_else(|| "unavailable".into())
}

pub fn manifest(work: &std::path::Path, seed: u64, profile: &str, mode: &str, pg_url: Option<&str>, backends: &[String]) -> serde_json::Value {
    let mem_kb: u64 = first_match("/proc/meminfo", "MemTotal").split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let pg_version = match pg_url {
        Some(u) => sh("psql", &[u, "-Atc", "select version()"]),
        None => "not used".into(),
    };
    serde_json::json!({
        "host": sh("hostname", &[]),
        "kernel": sh("uname", &["-r"]),
        "os": first_match("/etc/os-release", "PRETTY_NAME").trim_matches('"'),
        "cpu": first_match("/proc/cpuinfo", "model name"),
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "ram_gib": mem_kb as f64 / 1048576.0,
        "filesystem": fs_type(work),
        "disk": sh("sh", &["-c", "lsblk -dno MODEL,ROTA 2>/dev/null | head -1"]),
        "postgres": pg_version,
        "sqlite": rusqlite::version(),
        "git": sh("git", &["--version"]),
        "rg": sh("sh", &["-c", "rg --version | head -1"]),
        "rustc": sh("rustc", &["--version"]),
        "harness_commit": sh("git", &["rev-parse", "HEAD"]),
        "seed": seed,
        "profile": profile,
        "mode": mode,
        "backends": backends,
        "started": sh("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]),
        "note": "Container environment: page-cache drop may be unavailable (cache column then says warm); dolt harness N/A (no binary reachable)."
    })
}
