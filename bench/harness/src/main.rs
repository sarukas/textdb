//! textdb-bench: run the comparative matrix (test spec, issue #1) and render the report.
//!
//! ```text
//! textdb-bench run  [--tests DIR] [--out DIR] [--backends a,b] [--profile poc|spec]
//!                   [--mode fast|durable] [--filter RT,XL-01] [--seed N] [--pg URL] [--drop-caches]
//! textdb-bench report [--out DIR]
//! ```

use std::path::PathBuf;

use textdb_bench::backend::Mode;
use textdb_bench::metrics::Sink;
use textdb_bench::runner::{load_tests, run_all, RunOpts};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("run");
    let out = PathBuf::from(arg(&args, "--out").unwrap_or_else(|| "bench/out".into()));
    match cmd {
        "run" => {
            let tests_dir = PathBuf::from(arg(&args, "--tests").unwrap_or_else(|| "bench/harness/tests".into()));
            let pg_url = arg(&args, "--pg").or_else(|| std::env::var("TEXTDB_PG_URL").ok());
            let backends: Vec<String> = arg(&args, "--backends")
                .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
                .unwrap_or_else(|| textdb_bench::backends::ALL.iter().map(|s| s.to_string()).collect());
            let profile = arg(&args, "--profile").unwrap_or_else(|| "poc".into());
            let mode = match arg(&args, "--mode").as_deref() {
                Some("durable") => Mode::Durable,
                _ => Mode::Fast,
            };
            let seed: u64 = arg(&args, "--seed").and_then(|s| s.parse().ok()).unwrap_or(20260912);
            let filter: Vec<String> = arg(&args, "--filter")
                .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
                .unwrap_or_default();
            let work = PathBuf::from(arg(&args, "--work").unwrap_or_else(|| "bench/data".into()));
            std::fs::create_dir_all(&work)?;
            std::fs::create_dir_all(&out)?;
            let tests = load_tests(&tests_dir)?;
            let manifest = textdb_bench::manifest::manifest(&work, seed, &profile, mode.name(), pg_url.as_deref(), &backends);
            std::fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;
            eprintln!("{} tests, backends {:?}, profile {}, mode {}", tests.len(), backends, profile, mode.name());
            let sink = Sink::new(&out)?;
            let opts = RunOpts {
                profile,
                mode,
                seed,
                work,
                filter,
                pg_url,
                verbose: args.iter().any(|a| a == "-v"),
                drop_caches: args.iter().any(|a| a == "--drop-caches"),
            };
            run_all(&tests, &backends, &sink, &opts)?;
            let md = textdb_bench::report::render(&sink.rows(), &manifest, &backends);
            std::fs::write(out.join("report.md"), md)?;
            eprintln!("wrote {}/results.jsonl and report.md", out.display());
        }
        "report" => {
            let text = std::fs::read_to_string(out.join("results.jsonl"))?;
            let rows: Vec<textdb_bench::metrics::Row> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
            let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json"))?)?;
            let backends: Vec<String> = arg(&args, "--backends")
                .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                .unwrap_or_else(|| {
                    manifest["backends"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                        .unwrap_or_default()
                });
            let md = textdb_bench::report::render(&rows, &manifest, &backends);
            std::fs::write(out.join("report.md"), &md)?;
            println!("{}", md);
        }
        _ => {
            eprintln!("usage: textdb-bench run|report [options]");
        }
    }
    Ok(())
}
