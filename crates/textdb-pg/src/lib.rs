//! textdb Postgres extension (spec §7.2), built with pgrx.
//!
//! Schema `kb`: tables (`node`, `commit`, `chunk`, `tree_node`, `chunk_ref`, `section`,
//! `link`, `frontmatter`, `checkpoint`), updatable views `kb.file`, `kb.folder`,
//! `kb.file_version` (INSTEAD OF triggers), and the functions `kb.content`, `kb.lines`,
//! `kb.section`, `kb.edit`, `kb.append`, `kb.diff`, `kb.ls`, `kb.search`, `kb.history`,
//! `kb.export`, `kb.checkpoint`. All algorithms come from `textdb-core`; this crate only
//! persists through SPI and owns the SQL grammar.

use pgrx::prelude::*;

::pgrx::pg_module_magic!();

mod store;

extension_sql!(
    r#"
CREATE SCHEMA IF NOT EXISTS kb;

CREATE TABLE kb.node (
  id          bigserial PRIMARY KEY,
  parent_id   bigint NULL REFERENCES kb.node(id),
  name        text NOT NULL,
  kind        smallint NOT NULL,            -- 0 folder, 1 file
  path        text NOT NULL,
  root        bytea NULL,
  version     bigint NOT NULL DEFAULT 0,
  nbytes      bigint, nlines bigint,
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now(),
  updated_by  text,
  deleted_at  timestamptz NULL
);
CREATE UNIQUE INDEX node_path ON kb.node(path text_pattern_ops) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX node_parent_name ON kb.node(parent_id, name) WHERE deleted_at IS NULL;
CREATE INDEX node_parent ON kb.node(parent_id);
INSERT INTO kb.node(parent_id, name, kind, path) VALUES (NULL, '', 0, '/');

CREATE TABLE kb.commit (
  file_id bigint NOT NULL, version bigint NOT NULL, root bytea NOT NULL, parent_root bytea NULL,
  author text, ts timestamptz NOT NULL DEFAULT now(), message text, nbytes bigint, nlines bigint,
  PRIMARY KEY (file_id, version)
);
CREATE TABLE kb.chunk (
  id bigserial PRIMARY KEY, hash bytea NOT NULL UNIQUE, bytes bytea NOT NULL, nlines int NOT NULL
);
CREATE TABLE kb.tree_node (hash bytea PRIMARY KEY, children bytea NOT NULL);
CREATE TABLE kb.chunk_ref (chunk_id bigint NOT NULL, file_id bigint NOT NULL, version bigint NOT NULL, PRIMARY KEY (chunk_id, file_id));
CREATE TABLE kb.section (file_id bigint NOT NULL, version bigint NOT NULL, heading_path text NOT NULL, level int NOT NULL, line_from bigint NOT NULL, line_to bigint NOT NULL);
CREATE INDEX section_file ON kb.section(file_id, version);
CREATE TABLE kb.link (file_id bigint NOT NULL, version bigint NOT NULL, target_path text NOT NULL, line bigint NOT NULL);
CREATE INDEX link_file ON kb.link(file_id, version);
CREATE TABLE kb.frontmatter (file_id bigint NOT NULL, version bigint NOT NULL, data jsonb, PRIMARY KEY (file_id, version));
CREATE TABLE kb.checkpoint (name text NOT NULL, file_id bigint NOT NULL, path text NOT NULL, root bytea NOT NULL, version bigint NOT NULL, PRIMARY KEY (name, file_id));

-- Custom SQLSTATEs (spec §7.2): TX001 conflict, TX002 contention, TX003 not found, TX004 invalid edit.
CREATE FUNCTION kb._raise(code text, msg text, detail text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION USING ERRCODE = code, MESSAGE = msg, DETAIL = coalesce(detail, ''); END $$;

-- LIKE pattern matching every path strictly under the folder `p`.
--
-- `p || '/%'` on its own is wrong whenever a folder name contains a LIKE wildcard: for
-- `/100%_done` the pattern `/100%_done/%` also matches `/100XXXdone/b.md`, so a folder's
-- byte total counted other folders' files and a search scoped to one folder returned
-- documents from another. Escaping the prefix makes the match literal.
--
-- It stays a prefix test rather than a `>= … < …` range on purpose. `node_path` is a
-- `text_pattern_ops` index, which serves `LIKE 'literal%'` under any database collation,
-- where a range comparison would need the database's collation to agree with byte order.
-- IMMUTABLE so the planner folds it and can still extract the literal prefix.
CREATE FUNCTION kb._subtree_like(p text) RETURNS text LANGUAGE sql IMMUTABLE PARALLEL SAFE AS
$$ SELECT replace(replace(replace(p, '\', '\\'), '%', '\%'), '_', '\_') || '/%' $$;
"#,
    name = "kb_tables",
    bootstrap
);

extension_sql!(
    r#"
-- Full-text index over chunks: insert-only by construction (claim 3).
ALTER TABLE kb.chunk ADD COLUMN tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', kb.chunk_text(bytes))) STORED;
CREATE INDEX chunk_tsv ON kb.chunk USING gin(tsv);

CREATE VIEW kb.folder AS
  SELECT n.id, n.path, n.name, p.path AS parent_path,
         (SELECT count(*) FROM kb.node c WHERE c.parent_id = n.id AND c.deleted_at IS NULL) AS n_children,
         (SELECT coalesce(sum(f.nbytes), 0) FROM kb.node f WHERE f.kind = 1 AND f.deleted_at IS NULL AND f.path LIKE kb._subtree_like(n.path)) AS nbytes_total,
         n.updated_at
  FROM kb.node n LEFT JOIN kb.node p ON p.id = n.parent_id
  WHERE n.kind = 0 AND n.deleted_at IS NULL AND n.path <> '/';

CREATE VIEW kb.file AS
  SELECT n.id, n.path, n.name, p.path AS parent_path,
         kb._materialize(n.root) AS content,
         n.version, n.nbytes, n.nlines,
         (SELECT fm.data FROM kb.frontmatter fm WHERE fm.file_id = n.id AND fm.version = n.version) AS frontmatter,
         n.updated_at, n.updated_by,
         NULL::bigint AS base_version
  FROM kb.node n LEFT JOIN kb.node p ON p.id = n.parent_id
  WHERE n.kind = 1 AND n.deleted_at IS NULL;

CREATE VIEW kb.file_version AS
  SELECT n.id, n.path, c.version, kb._materialize(c.root) AS content,
         CASE WHEN c.version > 1 THEN c.version - 1 END AS parent_version,
         c.author, c.ts, c.message
  FROM kb.commit c JOIN kb.node n ON n.id = c.file_id;

CREATE FUNCTION kb.file_iud() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    -- Plain INSERT is an upsert: identical content produces no new version
    -- (ON CONFLICT is not available on views with INSTEAD OF triggers).
    PERFORM kb._upsert(NEW.path, coalesce(NEW.content, ''), NEW.updated_by, 'insert');
    RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN
    IF NEW.path IS DISTINCT FROM OLD.path THEN
      PERFORM kb._rename(OLD.path, NEW.path);
    END IF;
    IF NEW.base_version IS NOT NULL OR NEW.content IS DISTINCT FROM OLD.content THEN
      PERFORM kb._update_content(NEW.path, coalesce(NEW.content, ''), NEW.base_version, NEW.updated_by, 'update');
    END IF;
    RETURN NEW;
  ELSE
    PERFORM kb._delete(OLD.path);
    RETURN OLD;
  END IF;
END $$;
CREATE TRIGGER file_iud INSTEAD OF INSERT OR UPDATE OR DELETE ON kb.file FOR EACH ROW EXECUTE FUNCTION kb.file_iud();

CREATE FUNCTION kb.folder_iud() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    PERFORM kb._mkdir(NEW.path);
    RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN
    IF NEW.path IS DISTINCT FROM OLD.path THEN PERFORM kb._rename(OLD.path, NEW.path); END IF;
    RETURN NEW;
  ELSE
    PERFORM kb._delete(OLD.path);
    RETURN OLD;
  END IF;
END $$;
CREATE TRIGGER folder_iud INSTEAD OF INSERT OR UPDATE OR DELETE ON kb.folder FOR EACH ROW EXECUTE FUNCTION kb.folder_iud();

-- Write entry points: the Rust functions return a JSON outcome; errors become SQLSTATEs here
-- (TX001 conflict with the JSON payload as DETAIL, TX002 contention, TX003 not found, TX004 invalid edit).
CREATE FUNCTION kb._check(r jsonb) RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
  IF r ? 'code' THEN
    RAISE EXCEPTION USING ERRCODE = r->>'code', MESSAGE = r->>'message', DETAIL = coalesce(r->>'detail', '');
  END IF;
  RETURN (r->>'version')::bigint;
END $$;
CREATE FUNCTION kb._update_content(path text, content text, base_version bigint, author text, message text) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb._check(kb._update_content_j(path, content, base_version, author, message)) $$;
CREATE FUNCTION kb.edit(path text, old text, new text, author text) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb._check(kb._edit_j(path, old, new, author)) $$;
CREATE FUNCTION kb.append(path text, tail text, author text) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb._check(kb._append_j(path, tail, author)) $$;

-- Attribute notation (spec §7.2): f.content is the view column; the rest are wrappers.
CREATE FUNCTION kb.lines(f kb.file, l_from bigint, l_to bigint) RETURNS text LANGUAGE sql STABLE AS $$ SELECT kb.lines(f.path, l_from, l_to) $$;
CREATE FUNCTION kb.section(f kb.file, heading text) RETURNS text LANGUAGE sql STABLE AS $$ SELECT kb.section(f.path, heading) $$;
CREATE FUNCTION kb.edit(f kb.file, old text, new text) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb.edit(f.path, old, new, NULL::text) $$;
CREATE FUNCTION kb.edit(path text, old text, new text) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb.edit(path, old, new, NULL::text) $$;
CREATE FUNCTION kb.append(f kb.file, tail text) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb.append(f.path, tail, NULL::text) $$;
CREATE FUNCTION kb.append(path text, tail text) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb.append(path, tail, NULL::text) $$;
CREATE FUNCTION kb.diff(f kb.file, v1 bigint, v2 bigint) RETURNS text LANGUAGE sql STABLE AS $$ SELECT kb.diff(f.path, v1, v2) $$;
CREATE FUNCTION kb.content(f kb.file) RETURNS text LANGUAGE sql STABLE AS $$ SELECT f.content $$;
CREATE FUNCTION kb.content(path text) RETURNS text LANGUAGE sql STABLE AS $$ SELECT kb.content(path, NULL::bigint) $$;
"#,
    name = "kb_views",
    finalize
);

#[pg_schema]
mod kb {
    use pgrx::prelude::*;
    use pgrx::datum::DatumWithOid;
    use textdb_core::commit::{commit, commit_append, CommitKind, Committed};
    use textdb_core::myers::byte_edits;
    use textdb_core::tree::{leaves, materialize, totals};
    use textdb_core::{unified_diff, ChunkParams, Edit, Hash, Storage, StructureExtractor, TextdbError};
    use textdb_md::MarkdownExtractor;

    use crate::store::{normalize_path, parent_of, name_of, raise, spi_err, to_hash, NodeRow, SpiStorage};

    const P: ChunkParams = ChunkParams::DEFAULT;
    const RETRIES: usize = textdb_core::DEFAULT_RETRIES;

    fn fail(e: TextdbError) -> ! {
        match &e {
            TextdbError::Conflict(c) => raise("TX001", &e.to_string(), &serde_json::to_string(c).unwrap_or_default()),
            TextdbError::Contention => raise("TX002", &e.to_string(), ""),
            TextdbError::NotFound(_) => raise("TX003", &e.to_string(), ""),
            TextdbError::InvalidEdit(_) => raise("TX004", &e.to_string(), ""),
            _ => raise("TX000", &e.to_string(), ""),
        }
    }

    fn ok<T>(r: Result<T, TextdbError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => fail(e),
        }
    }

    /// Outcome of a write as JSON for the PL/pgSQL wrapper `kb._check`, which turns an error
    /// object into `RAISE EXCEPTION USING ERRCODE = …` (pgrx re-raises panics as XX000, so
    /// custom SQLSTATEs must be raised from SQL).
    fn json_result(r: Result<i64, TextdbError>) -> pgrx::JsonB {
        pgrx::JsonB(match r {
            Ok(v) => serde_json::json!({ "version": v }),
            Err(e) => {
                let detail = match &e {
                    TextdbError::Conflict(c) => serde_json::to_string(c).unwrap_or_default(),
                    _ => String::new(),
                };
                serde_json::json!({ "code": e.code(), "message": e.to_string(), "detail": detail })
            }
        })
    }

    fn file_by_path_r(path: &str) -> Result<NodeRow, TextdbError> {
        match node_by_path(path) {
            Some(n) if n.kind == 1 => Ok(n),
            Some(_) => Err(TextdbError::InvalidEdit(format!("{} is a folder", path))),
            None => Err(TextdbError::NotFound(path.to_string())),
        }
    }

    fn root_of_version_r(file_id: i64, version: u64) -> Result<Hash, TextdbError> {
        let root = Spi::get_one_with_args::<Vec<u8>>(
            "SELECT root FROM kb.commit WHERE file_id = $1 AND version = $2",
            &[file_id.into(), (version as i64).into()],
        )
        .map_err(|e| TextdbError::Storage(e.to_string()))?;
        match root {
            Some(r) => to_hash(&r),
            None => Err(TextdbError::NotFound(format!("version {} of file {}", version, file_id))),
        }
    }

    /// Lossy UTF-8 view of chunk bytes, used by the generated `tsv` column.
    #[pg_extern(immutable, parallel_safe, strict)]
    fn chunk_text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Materialize a Merkle root to text (view column `content`).
    #[pg_extern(stable, parallel_safe)]
    fn _materialize(root: Option<&[u8]>) -> Option<String> {
        let root = root?;
        let h = to_hash(root).ok()?;
        let st = SpiStorage::new();
        let bytes = ok(materialize(&st, &h));
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn node_by_path(path: &str) -> Option<NodeRow> {
        NodeRow::by_path(path, false)
    }

    fn file_by_path(path: &str) -> NodeRow {
        match node_by_path(path) {
            Some(n) if n.kind == 1 => n,
            Some(_) => fail(TextdbError::InvalidEdit(format!("{} is a folder", path))),
            None => fail(TextdbError::NotFound(path.to_string())),
        }
    }

    /// `mkdir -p`; returns the folder id.
    #[pg_extern(volatile)]
    fn _mkdir(path: &str) -> i64 {
        let path = ok(normalize_path(path));
        ensure_folder(&path)
    }

    fn ensure_folder(path: &str) -> i64 {
        if let Some(n) = node_by_path(path) {
            if n.kind != 0 {
                fail(TextdbError::InvalidEdit(format!("{} is a file", path)));
            }
            return n.id;
        }
        if path == "/" {
            fail(TextdbError::Storage("root folder missing".into()));
        }
        let parent = ensure_folder(parent_of(path));
        Spi::get_one_with_args::<i64>(
            "INSERT INTO kb.node(parent_id, name, kind, path) VALUES ($1, $2, 0, $3) RETURNING id",
            &[parent.into(), name_of(path).into(), path.into()],
        )
        .unwrap_or_else(|e| spi_err(e))
        .expect("id")
    }

    fn record_commit(file_id: i64, path: &str, c: &Committed, parent_root: Option<&Hash>, author: Option<&str>, message: Option<&str>) {
        let st = SpiStorage::new();
        let (nbytes, nlines) = ok(totals(&st, &c.root));
        Spi::run_with_args(
            "INSERT INTO kb.commit(file_id, version, root, parent_root, author, message, nbytes, nlines) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                file_id.into(),
                (c.version as i64).into(),
                c.root.to_vec().into(),
                parent_root.map(|h| h.to_vec()).into(),
                author.into(),
                message.into(),
                (nbytes as i64).into(),
                (nlines as i64).into(),
            ],
        )
        .unwrap_or_else(|e| spi_err(e));
        Spi::run_with_args(
            "UPDATE kb.node SET nbytes = $1, nlines = $2, updated_by = $3 WHERE id = $4",
            &[(nbytes as i64).into(), (nlines as i64).into(), author.into(), file_id.into()],
        )
        .unwrap_or_else(|e| spi_err(e));
        let mut seen = std::collections::HashSet::new();
        for h in &c.new_chunks {
            if !seen.insert(*h) {
                continue;
            }
            Spi::run_with_args(
                "INSERT INTO kb.chunk_ref(chunk_id, file_id, version) SELECT id, $2, $3 FROM kb.chunk WHERE hash = $1 ON CONFLICT DO NOTHING",
                &[h.to_vec().into(), file_id.into(), (c.version as i64).into()],
            )
            .unwrap_or_else(|e| spi_err(e));
        }
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".md") || lower.ends_with(".markdown") {
            let bytes = ok(materialize(&st, &c.root));
            let s = MarkdownExtractor.extract(&bytes);
            // HEAD-only structure rows (ADR 0007).
            for t in ["kb.section", "kb.link", "kb.frontmatter"] {
                Spi::run_with_args(&format!("DELETE FROM {} WHERE file_id = $1", t), &[file_id.into()]).unwrap_or_else(|e| spi_err(e));
            }
            for sec in &s.sections {
                Spi::run_with_args(
                    "INSERT INTO kb.section(file_id, version, heading_path, level, line_from, line_to) VALUES ($1, $2, $3, $4, $5, $6)",
                    &[
                        file_id.into(),
                        (c.version as i64).into(),
                        sec.heading_path.as_str().into(),
                        (sec.level as i32).into(),
                        (sec.line_from as i64).into(),
                        (sec.line_to as i64).into(),
                    ],
                )
                .unwrap_or_else(|e| spi_err(e));
            }
            for l in &s.links {
                Spi::run_with_args(
                    "INSERT INTO kb.link(file_id, version, target_path, line) VALUES ($1, $2, $3, $4)",
                    &[file_id.into(), (c.version as i64).into(), l.target_path.as_str().into(), (l.line as i64).into()],
                )
                .unwrap_or_else(|e| spi_err(e));
            }
            if let Some(fm) = &s.frontmatter {
                Spi::run_with_args(
                    "INSERT INTO kb.frontmatter(file_id, version, data) VALUES ($1, $2, $3::jsonb) ON CONFLICT (file_id, version) DO UPDATE SET data = EXCLUDED.data",
                    &[file_id.into(), (c.version as i64).into(), fm.to_string().as_str().into()],
                )
                .unwrap_or_else(|e| spi_err(e));
            }
        }
    }

    /// Create a file (parents created), commit version 1.
    #[pg_extern(volatile)]
    fn _create(path: &str, content: &str, author: Option<&str>, message: Option<&str>) -> i64 {
        let path = ok(normalize_path(path));
        if node_by_path(&path).is_some() {
            fail(TextdbError::InvalidEdit(format!("{} already exists", path)));
        }
        let parent = ensure_folder(parent_of(&path));
        let id = Spi::get_one_with_args::<i64>(
            "INSERT INTO kb.node(parent_id, name, kind, path, updated_by) VALUES ($1, $2, 1, $3, $4) RETURNING id",
            &[parent.into(), name_of(&path).into(), path.as_str().into(), author.into()],
        )
        .unwrap_or_else(|e| spi_err(e))
        .expect("id");
        let mut st = SpiStorage::new();
        let (root, chunks) = ok(textdb_core::build_with_chunks(&mut st, &P, content.as_bytes()));
        if !ok(st.cas_root(id as u64, None, &root)) {
            fail(TextdbError::Storage("initial CAS failed".into()));
        }
        let c = Committed {
            version: 1,
            root,
            kind: CommitKind::Direct,
            new_chunks: chunks,
            retries: 0,
        };
        record_commit(id, &path, &c, None, author, message);
        1
    }

    /// Upsert: create, or update content (identical content → no new version).
    #[pg_extern(volatile)]
    fn _upsert(path: &str, content: &str, author: Option<&str>, message: Option<&str>) -> i64 {
        let path = ok(normalize_path(path));
        match node_by_path(&path) {
            None => _create(&path, content, author, message),
            Some(_) => match update_content_impl(&path, content, None, author, message) {
                Ok(v) => v,
                Err(e) => fail(e),
            },
        }
    }

    /// Whole-content update: diff against `base_version` (or HEAD) → edit set → commit with rebase.
    /// Returns a JSON outcome; `kb._update_content` (SQL) raises TX001/TX002/… from it.
    #[pg_extern(volatile)]
    fn _update_content_j(path: &str, content: &str, base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> pgrx::JsonB {
        json_result(update_content_impl(path, content, base_version, author, message))
    }

    fn update_content_impl(path: &str, content: &str, base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<i64, TextdbError> {
        let path = normalize_path(path)?;
        let n = file_by_path_r(&path)?;
        let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let base = match base_version {
            Some(v) if v != n.version => root_of_version_r(n.id, v as u64)?,
            _ => cur,
        };
        let mut st = SpiStorage::new();
        let old = materialize(&st, &base)?;
        let edits = byte_edits(&old, content.as_bytes());
        if edits.is_empty() && base == cur {
            return Ok(n.version);
        }
        let c = commit(&mut st, &P, n.id as u64, &path, &base, &edits, RETRIES)?;
        if c.kind != CommitKind::NoOp {
            record_commit(n.id, &path, &c, Some(&cur), author, message);
        }
        Ok(c.version as i64)
    }

    fn root_of_version(file_id: i64, version: u64) -> Hash {
        let root = Spi::get_one_with_args::<Vec<u8>>(
            "SELECT root FROM kb.commit WHERE file_id = $1 AND version = $2",
            &[file_id.into(), (version as i64).into()],
        )
        .unwrap_or_else(|e| spi_err(e));
        match root {
            Some(r) => ok(to_hash(&r)),
            None => fail(TextdbError::NotFound(format!("version {} of file {}", version, file_id))),
        }
    }

    /// Strict replace: `old` must occur exactly once in the current content (spec §7.2 `edit`).
    /// JSON outcome; `kb.edit(...)` (SQL) raises from it.
    #[pg_extern(volatile)]
    fn _edit_j(path: &str, old: &str, new: &str, author: Option<&str>) -> pgrx::JsonB {
        json_result(edit_impl(path, old, new, author))
    }

    fn edit_impl(path: &str, old: &str, new: &str, author: Option<&str>) -> Result<i64, TextdbError> {
        let path = normalize_path(path)?;
        let n = file_by_path_r(&path)?;
        let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let mut st = SpiStorage::new();
        let content = materialize(&st, &cur)?;
        let pos = find_unique(&content, old.as_bytes())?;
        let edits = [Edit::new(pos as u64, (pos + old.len()) as u64, new.as_bytes().to_vec())];
        let c = commit(&mut st, &P, n.id as u64, &path, &cur, &edits, RETRIES)?;
        if c.kind != CommitKind::NoOp {
            record_commit(n.id, &path, &c, Some(&cur), author, Some("edit"));
        }
        Ok(c.version as i64)
    }

    fn find_unique(content: &[u8], old: &[u8]) -> Result<usize, TextdbError> {
        if old.is_empty() {
            return Err(TextdbError::InvalidEdit("old text is empty".into()));
        }
        let mut found = None;
        let mut i = 0;
        while i + old.len() <= content.len() {
            if &content[i..i + old.len()] == old {
                if found.is_some() {
                    return Err(TextdbError::InvalidEdit("old text is not unique".into()));
                }
                found = Some(i);
                i += old.len();
            } else {
                i += 1;
            }
        }
        found.ok_or_else(|| TextdbError::InvalidEdit("old text not found".into()))
    }

    /// Append at the current end; never conflicts. JSON outcome; `kb.append(...)` (SQL) raises.
    #[pg_extern(volatile)]
    fn _append_j(path: &str, tail: &str, author: Option<&str>) -> pgrx::JsonB {
        json_result(append_impl(path, tail, author))
    }

    fn append_impl(path: &str, tail: &str, author: Option<&str>) -> Result<i64, TextdbError> {
        let path = normalize_path(path)?;
        let n = file_by_path_r(&path)?;
        let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let mut st = SpiStorage::new();
        let c = commit_append(&mut st, &P, n.id as u64, &path, tail.as_bytes(), RETRIES)?;
        if c.kind != CommitKind::NoOp {
            record_commit(n.id, &path, &c, Some(&cur), author, Some("append"));
        }
        Ok(c.version as i64)
    }

    /// Rename/move a file or folder: subtree path rewrite in one statement, ids stable.
    #[pg_extern(volatile)]
    fn _rename(from: &str, to: &str) {
        let from = ok(normalize_path(from));
        let to = ok(normalize_path(to));
        if from == "/" || to == "/" {
            fail(TextdbError::InvalidEdit("cannot move the root".into()));
        }
        let src = node_by_path(&from).unwrap_or_else(|| fail(TextdbError::NotFound(from.clone())));
        if to == from || to.starts_with(&format!("{}/", from)) {
            fail(TextdbError::InvalidEdit(format!("cannot move {} into itself", from)));
        }
        if node_by_path(&to).is_some() {
            fail(TextdbError::InvalidEdit(format!("{} already exists", to)));
        }
        let parent = ensure_folder(parent_of(&to));
        Spi::run_with_args(
            "UPDATE kb.node SET path = $2 || substr(path, length($1) + 1), updated_at = now() WHERE path LIKE kb._subtree_like($1) AND deleted_at IS NULL",
            &[from.as_str().into(), to.as_str().into()],
        )
        .unwrap_or_else(|e| spi_err(e));
        Spi::run_with_args(
            "UPDATE kb.node SET path = $1, name = $2, parent_id = $3, updated_at = now() WHERE id = $4",
            &[to.as_str().into(), name_of(&to).into(), parent.into(), src.id.into()],
        )
        .unwrap_or_else(|e| spi_err(e));
    }

    /// Tombstone a file or folder subtree; content and history retained.
    #[pg_extern(volatile)]
    fn _delete(path: &str) {
        let path = ok(normalize_path(path));
        if path == "/" {
            fail(TextdbError::InvalidEdit("cannot delete the root".into()));
        }
        node_by_path(&path).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        Spi::run_with_args(
            "UPDATE kb.node SET deleted_at = now() WHERE (path = $1 OR path LIKE kb._subtree_like($1)) AND deleted_at IS NULL",
            &[path.as_str().into()],
        )
        .unwrap_or_else(|e| spi_err(e));
    }

    /// Content of a file at HEAD or at `version` (works for tombstoned files).
    #[pg_extern(stable)]
    fn content(path: &str, version: Option<i64>) -> String {
        let path = ok(normalize_path(path));
        let st = SpiStorage::new();
        let bytes = match version {
            None => {
                let n = file_by_path(&path);
                ok(materialize(&st, &n.root.unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())))))
            }
            Some(v) => {
                let n = NodeRow::by_path(&path, true).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
                ok(materialize(&st, &root_of_version(n.id, v as u64)))
            }
        };
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Lines `[l_from, l_to]`, 1-based inclusive.
    #[pg_extern(stable)]
    fn lines(path: &str, l_from: i64, l_to: i64) -> String {
        let path = ok(normalize_path(path));
        let n = file_by_path(&path);
        if l_from <= 0 || l_to < l_from {
            return String::new();
        }
        let st = SpiStorage::new();
        let root = n.root.unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let bytes = ok(textdb_core::lines(&st, &root, (l_from - 1) as u64, (l_to - 1) as u64));
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Text of the section whose heading matches (exact heading path, then last component).
    #[pg_extern(stable)]
    fn section(path: &str, heading: &str) -> Option<String> {
        let path = ok(normalize_path(path));
        let n = file_by_path(&path);
        let span = Spi::get_two_with_args::<i64, i64>(
            "SELECT line_from, line_to FROM kb.section WHERE file_id = $1 AND version = $2 AND (heading_path = $3 OR lower(heading_path) = lower($3) OR lower(heading_path) LIKE '%/ ' || lower($3)) ORDER BY CASE WHEN heading_path = $3 THEN 0 ELSE 1 END LIMIT 1",
            &[n.id.into(), n.version.into(), heading.trim().into()],
        )
        .unwrap_or_else(|e| spi_err(e));
        match span {
            (Some(a), Some(b)) => Some(lines(&path, a, b)),
            _ => None,
        }
    }

    #[pg_extern(stable)]
    fn diff(path: &str, v1: i64, v2: i64) -> String {
        let path = ok(normalize_path(path));
        let n = NodeRow::by_path(&path, true).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let a = root_of_version(n.id, v1 as u64);
        let b = root_of_version(n.id, v2 as u64);
        let st = SpiStorage::new();
        let body = ok(unified_diff(&st, &a, &b, 3));
        if body.is_empty() {
            return String::new();
        }
        format!("--- {p}@{v1}\n+++ {p}@{v2}\n{body}", p = path, v1 = v1, v2 = v2, body = body)
    }

    #[pg_extern(stable)]
    fn ls(
        path: &str,
    ) -> TableIterator<'static, (name!(name, String), name!(kind, String), name!(nbytes, Option<i64>), name!(nlines, Option<i64>), name!(updated_at, pgrx::datum::TimestampWithTimeZone))> {
        let path = ok(normalize_path(path));
        let dir = node_by_path(&path).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let rows: Vec<_> = Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT name, kind, nbytes, nlines, updated_at FROM kb.node WHERE parent_id = $1 AND deleted_at IS NULL ORDER BY name",
                    None,
                    &[dir.id.into()],
                )
                .unwrap_or_else(|e| spi_err(e));
            let mut v = Vec::new();
            for r in t {
                let kind: i16 = r.get(2).unwrap_or_else(|e| spi_err(e)).unwrap_or(1);
                v.push((
                    r.get::<String>(1).unwrap_or_else(|e| spi_err(e)).unwrap_or_default(),
                    if kind == 1 { "file".to_string() } else { "folder".to_string() },
                    r.get::<i64>(3).unwrap_or_else(|e| spi_err(e)),
                    r.get::<i64>(4).unwrap_or_else(|e| spi_err(e)),
                    r.get::<pgrx::datum::TimestampWithTimeZone>(5).unwrap_or_else(|e| spi_err(e)).expect("updated_at"),
                ));
            }
            v
        });
        TableIterator::new(rows)
    }

    #[pg_extern(stable)]
    fn history(
        path: &str,
    ) -> TableIterator<'static, (name!(version, i64), name!(author, Option<String>), name!(ts, pgrx::datum::TimestampWithTimeZone), name!(message, Option<String>))> {
        let path = ok(normalize_path(path));
        let n = NodeRow::by_path(&path, true).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let rows: Vec<_> = Spi::connect(|client| {
            let t = client
                .select("SELECT version, author, ts, message FROM kb.commit WHERE file_id = $1 ORDER BY version", None, &[n.id.into()])
                .unwrap_or_else(|e| spi_err(e));
            let mut v = Vec::new();
            for r in t {
                v.push((
                    r.get::<i64>(1).unwrap_or_else(|e| spi_err(e)).unwrap_or(0),
                    r.get::<String>(2).unwrap_or_else(|e| spi_err(e)),
                    r.get::<pgrx::datum::TimestampWithTimeZone>(3).unwrap_or_else(|e| spi_err(e)).expect("ts"),
                    r.get::<String>(4).unwrap_or_else(|e| spi_err(e)),
                ));
            }
            v
        });
        TableIterator::new(rows)
    }

    #[pg_extern(stable)]
    fn export(prefix: default!(&str, "'/'")) -> TableIterator<'static, (name!(path, String), name!(content, String))> {
        let prefix = ok(normalize_path(prefix));
        let files = NodeRow::files_under(&prefix);
        let st = SpiStorage::new();
        let mut out = Vec::new();
        for n in files {
            if let Some(root) = n.root {
                out.push((n.path, String::from_utf8_lossy(&ok(materialize(&st, &root))).into_owned()));
            }
        }
        TableIterator::new(out)
    }

    /// Record all current roots under a name (spec §8.6).
    #[pg_extern(volatile)]
    fn checkpoint(name: &str) -> i64 {
        Spi::get_one_with_args::<i64>(
            "WITH i AS (INSERT INTO kb.checkpoint(name, file_id, path, root, version) SELECT $1, id, path, root, version FROM kb.node WHERE kind = 1 AND deleted_at IS NULL AND root IS NOT NULL ON CONFLICT (name, file_id) DO UPDATE SET root = EXCLUDED.root, version = EXCLUDED.version, path = EXCLUDED.path RETURNING 1) SELECT count(*) FROM i",
            &[name.into()],
        )
        .unwrap_or_else(|e| spi_err(e))
        .unwrap_or(0)
    }

    /// Tree depth and leaf count of a file (benchmark hook).
    #[pg_extern(stable)]
    fn tree_stats(path: &str) -> TableIterator<'static, (name!(depth, i64), name!(leaves, i64))> {
        let path = ok(normalize_path(path));
        let n = file_by_path(&path);
        let st = SpiStorage::new();
        let root = n.root.unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let d = ok(textdb_core::tree::depth(&st, &root)) as i64;
        let l = ok(leaves(&st, &root)).len() as i64;
        TableIterator::once((d, l))
    }

    /// Full-text search: terms ANDed at document level, `"a b"` phrase, `foo*` prefix.
    /// Chunk hits → `chunk_ref` → live files under `prefix` → line via the tree.
    #[pg_extern(stable)]
    fn search(
        query: &str,
        prefix: default!(&str, "'/'"),
        lim: default!(i64, 100),
    ) -> TableIterator<'static, (name!(path, String), name!(line, i64), name!(snippet, String), name!(rank, f32))> {
        let prefix = ok(normalize_path(prefix));
        let terms = query_terms(query);
        if terms.is_empty() {
            return TableIterator::new(Vec::new());
        }
        let limit = lim.max(1) as usize;
        // Per term: file_id → (chunk_id, rank).
        let mut per_term: Vec<std::collections::HashMap<i64, (i64, f32)>> = Vec::new();
        for t in &terms {
            let tsq = tsquery_term(t);
            let files: std::collections::HashMap<i64, (i64, f32)> = Spi::connect(|client| {
                let t = client
                    .select(
                        "SELECT r.file_id, c.id, ts_rank(c.tsv, q) FROM kb.chunk c JOIN kb.chunk_ref r ON r.chunk_id = c.id JOIN kb.node n ON n.id = r.file_id, to_tsquery('simple', $1) q WHERE c.tsv @@ q AND n.deleted_at IS NULL AND n.kind = 1 AND ($2 = '/' OR n.path LIKE kb._subtree_like($2)) ORDER BY 3 DESC LIMIT $3",
                        None,
                        &[tsq.as_str().into(), prefix.as_str().into(), ((limit.saturating_mul(50)).min(500_000) as i64).into()],
                    )
                    .unwrap_or_else(|e| spi_err(e));
                let mut m = std::collections::HashMap::new();
                for r in t {
                    let fid: i64 = r.get(1).unwrap_or_else(|e| spi_err(e)).unwrap_or(0);
                    let cid: i64 = r.get(2).unwrap_or_else(|e| spi_err(e)).unwrap_or(0);
                    let rank: f32 = r.get(3).unwrap_or_else(|e| spi_err(e)).unwrap_or(0.0);
                    m.entry(fid).or_insert((cid, rank));
                }
                m
            });
            per_term.push(files);
        }
        let mut candidates: Vec<(i64, i64, f32)> = per_term[0]
            .iter()
            .filter(|(id, _)| per_term[1..].iter().all(|m| m.contains_key(id)))
            .map(|(id, (c, r))| (*id, *c, *r))
            .collect();
        candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        let st = SpiStorage::new();
        let mut hits = Vec::new();
        for (file_id, chunk_id, rank) in candidates {
            let (path, root) = match Spi::get_two_with_args::<String, Vec<u8>>(
                "SELECT path, root FROM kb.node WHERE id = $1 AND deleted_at IS NULL",
                &[file_id.into()],
            )
            .unwrap_or_else(|e| spi_err(e))
            {
                (Some(p), Some(r)) => (p, r),
                _ => continue,
            };
            let (hash, bytes) = match Spi::get_two_with_args::<Vec<u8>, Vec<u8>>("SELECT hash, bytes FROM kb.chunk WHERE id = $1", &[chunk_id.into()])
                .unwrap_or_else(|e| spi_err(e))
            {
                (Some(h), Some(b)) => (h, b),
                _ => continue,
            };
            let root = ok(to_hash(&root));
            let hash = ok(to_hash(&hash));
            let leaf = ok(leaves(&st, &root)).into_iter().find(|l| l.hash == hash);
            let (line, snippet) = match leaf {
                Some(l) => {
                    let (li, sn) = locate_terms(&bytes, &terms[..1]);
                    (l.line_off as i64 + li as i64 + 1, sn)
                }
                None => {
                    let body = ok(materialize(&st, &root));
                    let (li, sn) = locate_terms(&body, &terms[..1]);
                    if sn.is_empty() {
                        continue;
                    }
                    (li as i64 + 1, sn)
                }
            };
            hits.push((path, line, snippet, rank));
            if hits.len() >= limit {
                break;
            }
        }
        TableIterator::new(hits)
    }

    pub fn query_terms(q: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_q = false;
        for ch in q.chars() {
            match ch {
                '"' => {
                    in_q = !in_q;
                    if !in_q && !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c if c.is_whitespace() && !in_q => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out.into_iter().filter(|t| !t.eq_ignore_ascii_case("and")).collect()
    }

    fn quote_lexeme(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    fn tsquery_term(t: &str) -> String {
        if let Some(stem) = t.strip_suffix('*') {
            format!("{}:*", quote_lexeme(stem))
        } else if t.contains(' ') {
            t.split_whitespace().map(quote_lexeme).collect::<Vec<_>>().join(" <-> ")
        } else {
            quote_lexeme(t)
        }
    }

    fn locate_terms(bytes: &[u8], terms: &[String]) -> (usize, String) {
        let text = String::from_utf8_lossy(bytes).to_lowercase();
        let mut best: Option<usize> = None;
        for t in terms {
            let t = t.trim_end_matches('*').to_lowercase();
            if t.is_empty() {
                continue;
            }
            if let Some(p) = text.find(&t) {
                best = Some(best.map_or(p, |b| b.min(p)));
            }
        }
        let pos = best.unwrap_or(0);
        let line = text[..pos].matches('\n').count();
        let raw = String::from_utf8_lossy(bytes);
        let snippet = raw.lines().nth(line).unwrap_or("").chars().take(200).collect();
        (line, snippet)
    }

    #[allow(dead_code)]
    fn _unused(_: DatumWithOid) {}
}
