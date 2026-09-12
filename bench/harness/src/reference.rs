//! In-memory reference model (test spec §9): one `Vec<u8>` per document plus a version log.
//! Oracles compare backends against this, never against each other.

use std::collections::HashMap;

#[derive(Default)]
pub struct Reference {
    pub docs: HashMap<String, Vec<u8>>,
    /// All bytes ever committed per path, in order (index 0 = version 1).
    pub history: HashMap<String, Vec<Vec<u8>>>,
}

impl Reference {
    pub fn create(&mut self, path: &str, body: &[u8]) {
        self.docs.insert(path.to_string(), body.to_vec());
        self.history.entry(path.to_string()).or_default().push(body.to_vec());
    }

    pub fn set(&mut self, path: &str, body: Vec<u8>) {
        self.history.entry(path.to_string()).or_default().push(body.clone());
        self.docs.insert(path.to_string(), body);
    }

    pub fn get(&self, path: &str) -> &[u8] {
        self.docs.get(path).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Apply a unique `old → new` replacement; returns false if `old` is not unique.
    pub fn replace(&mut self, path: &str, old: &[u8], new: &[u8]) -> bool {
        let cur = self.docs.get(path).cloned().unwrap_or_default();
        match find_unique(&cur, old) {
            Some(pos) => {
                let mut next = Vec::with_capacity(cur.len() + new.len());
                next.extend_from_slice(&cur[..pos]);
                next.extend_from_slice(new);
                next.extend_from_slice(&cur[pos + old.len()..]);
                self.set(path, next);
                true
            }
            None => false,
        }
    }

    pub fn append(&mut self, path: &str, tail: &[u8]) {
        let mut cur = self.docs.get(path).cloned().unwrap_or_default();
        cur.extend_from_slice(tail);
        self.set(path, cur);
    }

    pub fn rename_prefix(&mut self, from: &str, to: &str) {
        let keys: Vec<String> = self.docs.keys().cloned().collect();
        for k in keys {
            if k == from || k.starts_with(&format!("{}/", from)) {
                let nk = format!("{}{}", to, &k[from.len()..]);
                let v = self.docs.remove(&k).unwrap();
                self.docs.insert(nk.clone(), v);
                if let Some(h) = self.history.remove(&k) {
                    self.history.insert(nk, h);
                }
            }
        }
    }

    /// Does `bytes` equal some committed version of `path`? (torn-read oracle)
    pub fn is_some_version(&self, path: &str, bytes: &[u8]) -> bool {
        self.history.get(path).map_or(false, |h| h.iter().any(|v| v == bytes))
    }
}

pub fn find_unique(content: &[u8], old: &[u8]) -> Option<usize> {
    if old.is_empty() {
        return None;
    }
    let mut found = None;
    let mut i = 0;
    while i + old.len() <= content.len() {
        if &content[i..i + old.len()] == old {
            if found.is_some() {
                return None;
            }
            found = Some(i);
            i += old.len();
        } else {
            i += 1;
        }
    }
    found
}

/// Apply `old → new` (unique) to `content`, or `None` when not unique.
pub fn splice(content: &[u8], old: &[u8], new: &[u8]) -> Option<Vec<u8>> {
    let pos = find_unique(content, old)?;
    let mut next = Vec::with_capacity(content.len() + new.len());
    next.extend_from_slice(&content[..pos]);
    next.extend_from_slice(new);
    next.extend_from_slice(&content[pos + old.len()..]);
    Some(next)
}
