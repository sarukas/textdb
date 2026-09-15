//! `textdb assets migrate-from-git`: the binaries git tracks move to the asset store. Each is
//! pushed (its pointer committed to the store and written next to it), removed from git's index
//! but left on disk, and ignored by the managed `.gitignore` block; the pointers and `.gitignore`
//! are committed in one git commit. Git's history keeps the old blobs: this stops the repository
//! growing, it does not shrink it.

use std::collections::HashSet;
use std::path::Path;

use serde_json::json;

use super::{items, push_run, scan, size_text, vault, write_gitignore_block, Item, PushOptions, VaultCache, SUFFIX};
use crate::store::{Store, StoreError};
use crate::{emit_json, git, out, Result};

pub struct MigrateOptions<'a> {
    pub to: Option<&'a str>,
    pub message: Option<&'a str>,
    pub author: Option<&'a str>,
    pub dry_run: bool,
}

/// The vault's assets whose files git tracks.
fn tracked_assets(st: &mut dyn Store, v: &super::Vault, tracked: &HashSet<String>) -> Result<Vec<Item>> {
    let scan = scan(st, v)?;
    let mut cache = VaultCache::open(v);
    let found = items(v, &scan, &mut cache, &[])?;
    cache.save();
    Ok(found.into_iter().filter(|i| i.file.as_ref().is_some_and(|f| tracked.contains(f))).collect())
}

pub fn migrate_from_git(st: &mut dyn Store, path: Option<&str>, dir: Option<&Path>, o: MigrateOptions, json: bool) -> Result<()> {
    let v = vault(st, path, dir)?;
    if git::repo(&v.dir).is_none() {
        return Err(StoreError::invalid(format!("{} is not in a git checkout", v.dir.display())));
    }
    if !o.dry_run && !git::nothing_staged(&v.dir) {
        return Err(StoreError::invalid(
            "changes are staged in git already: commit or unstage them first, so the migration's commit holds the migration only",
        ));
    }
    let tracked: HashSet<String> = git::tracked(&v.dir)?.into_iter().collect();
    let before = tracked_assets(st, &v, &tracked)?;
    let (mut to_push, mut blocked) = (Vec::new(), Vec::new());
    for i in &before {
        match i.state {
            "new" | "modified" => to_push.push(i.path.clone()),
            "ok" => {}
            _ => blocked.push(format!("{}: {}{}", i.path, i.state, i.note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default())),
        }
    }
    let bytes: u64 = before.iter().filter_map(|i| i.size).sum();

    if o.dry_run {
        if json {
            return emit_json(&json!({
                "dry_run": true,
                "tracked_assets": before.len(),
                "bytes": bytes,
                "to_push": to_push,
                "in_store_already": before.iter().filter(|i| i.state == "ok").map(|i| &i.path).collect::<Vec<_>>(),
                "blocked": blocked,
            }));
        }
        let mut s = format!(
            "would move {} binaries git tracks ({}) to the asset store: {} to push, {} in the asset store already\n",
            before.len(),
            size_text(bytes),
            to_push.len(),
            before.len() - to_push.len() - blocked.len()
        );
        for p in &to_push {
            s.push_str(&format!("  push       {p}\n"));
        }
        for b in &blocked {
            s.push_str(&format!("  blocked    {b}\n"));
        }
        return out(s.as_bytes());
    }

    let message = o.message.unwrap_or("assets migrate-from-git");
    let pushed = if to_push.is_empty() {
        super::PushReport::default()
    } else {
        push_run(st, &v, &to_push, &PushOptions { to: o.to, message: Some(message), author: o.author, dry_run: false, force: false })?
    };

    // What is in the asset store now, with its pointer next to it, leaves git's index.
    let after = tracked_assets(st, &v, &tracked)?;
    let ready: Vec<&Item> = after.iter().filter(|i| i.state == "ok" && v.dir.join(format!("{}{SUFFIX}", i.rel)).is_file()).collect();
    let gi = write_gitignore_block(st, &v.dir, false)?;
    // A pointer the user's own .gitignore lines ignore cannot be committed: its file stays in git.
    let candidates: Vec<String> = ready.iter().map(|i| format!("{}{SUFFIX}", i.rel)).collect();
    let ignored = git::ignored(&v.dir, &candidates);
    let migrated: Vec<&Item> = ready.into_iter().filter(|i| !ignored.contains(&format!("{}{SUFFIX}", i.rel))).collect();
    let files: Vec<String> = migrated.iter().filter_map(|i| i.file.clone()).collect();
    let mut stage: Vec<String> = migrated.iter().map(|i| format!("{}{SUFFIX}", i.rel)).collect();
    stage.push(".gitignore".to_string());
    git::untrack(&v.dir, &files)?;
    git::stage(&v.dir, &stage)?;
    let migrated_bytes: u64 = migrated.iter().filter_map(|i| i.size).sum();
    let commit = if git::nothing_staged(&v.dir) {
        None
    } else {
        let stores: HashSet<&str> = migrated.iter().filter_map(|i| i.store.as_deref()).collect();
        let mut stores: Vec<&str> = stores.into_iter().collect();
        stores.sort();
        let text = format!(
            "{message}: {} binaries ({}) moved to the asset store\n\nTheir bytes are in the asset store {}; their pointers (*.tdbasset) are committed here.\nThe files stay on disk, ignored by the textdb block in .gitignore.\n",
            migrated.len(),
            size_text(migrated_bytes),
            stores.join(", ")
        );
        match git::commit_staged(&v.dir, &text) {
            Ok(c) => Some(c),
            Err(e) => {
                // Nothing half done is left staged: the next run starts from the same place.
                let touched: Vec<String> = files.iter().chain(stage.iter()).cloned().collect();
                let what = match git::unstage(&v.dir, &touched) {
                    Ok(()) => "what was staged for it is unstaged again (the assets stay pushed); fix that and run it again",
                    Err(_) => "what was staged for it is still staged: commit it, or `git reset` it",
                };
                return Err(StoreError::other(format!("the git commit failed ({}); {what}", e.message)));
            }
        }
    };

    if json {
        emit_json(&json!({
            "dry_run": false,
            "tracked_assets": before.len(),
            "pushed": pushed.pushed,
            "migrated": migrated.iter().map(|i| &i.path).collect::<Vec<_>>(),
            "bytes": migrated_bytes,
            "pointers_ignored": ignored,
            "blocked": blocked,
            "conflicts": pushed.conflicts,
            "failed": pushed.failed,
            "gitignore_changed": gi.changed,
            "commit": commit,
        }))?;
    } else {
        let mut s = format!("moved {} binaries git tracked ({}) to the asset store\n", migrated.len(), size_text(migrated_bytes));
        for i in &migrated {
            s.push_str(&format!("  migrated   {}\n", i.path));
        }
        for p in &ignored {
            s.push_str(&format!("  ignored    {p}: your .gitignore ignores this pointer, so its file stays in git\n"));
        }
        for b in &blocked {
            s.push_str(&format!("  blocked    {b}\n"));
        }
        for c in &pushed.conflicts {
            s.push_str(&format!("  conflict   {c}\n"));
        }
        for f in &pushed.failed {
            s.push_str(&format!("  failed     {f}\n"));
        }
        match &commit {
            Some(c) => s.push_str(&format!("git: committed {}\n", git::short(c))),
            None => s.push_str("git: nothing to commit\n"),
        }
        out(s.as_bytes())?;
    }
    if !pushed.failed.is_empty() {
        return Err(StoreError::other(format!("{} binaries could not be pushed and stay in git", pushed.failed.len())));
    }
    if !pushed.conflicts.is_empty() || !blocked.is_empty() {
        return Err(StoreError::conflict(format!(
            "{} binaries stay in git: resolve what is listed, then run it again",
            pushed.conflicts.len() + blocked.len()
        )));
    }
    Ok(())
}
