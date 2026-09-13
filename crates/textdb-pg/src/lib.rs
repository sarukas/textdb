//! textdb Postgres extension (spec §7.2), built with pgrx.
//!
//! Schema `kb`: tables (`node`, `commit`, `chunk`, `tree_node`, `chunk_ref`, `section`,
//! `link`, `frontmatter`, `checkpoint`), updatable views `kb.file`, `kb.folder`,
//! `kb.file_version` (INSTEAD OF triggers), and the functions `kb.content`, `kb.lines`,
//! `kb.section`, `kb.edit`, `kb.append`, `kb.diff`, `kb.ls`, `kb.search`, `kb.history`,
//! `kb.export`, `kb.checkpoint`, and for live clients the change feed (`kb.change`,
//! `kb.feed`, `kb.last_seq`, NOTIFY channel `textdb_change`), `kb.hunks`, `kb.chunks`,
//! `kb.write`, `kb.replace_lines`, `kb.move`, `kb.remove`. All algorithms come from `textdb-core`; this crate only
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
  deleted_at  timestamptz NULL,
  nwords      bigint,                       -- file: words, as wc -w counts them
  nauthors    bigint,                       -- file: distinct commit authors (kb.file_author rows)
  -- Folder: totals of every live node below it, as of the last kb.compact_folder_totals();
  -- kb.folder_delta holds the changes since. Zero on files.
  t_files      bigint NOT NULL DEFAULT 0,
  t_folders    bigint NOT NULL DEFAULT 0,
  t_bytes      bigint NOT NULL DEFAULT 0,
  t_lines      bigint NOT NULL DEFAULT 0,
  t_words      bigint NOT NULL DEFAULT 0,
  t_versions   bigint NOT NULL DEFAULT 0,
  t_updated_at timestamptz NULL
);
CREATE UNIQUE INDEX node_path ON kb.node(path text_pattern_ops) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX node_parent_name ON kb.node(parent_id, name) WHERE deleted_at IS NULL;
CREATE INDEX node_parent ON kb.node(parent_id);
INSERT INTO kb.node(parent_id, name, kind, path) VALUES (NULL, '', 0, '/');

CREATE TABLE kb.commit (
  file_id bigint NOT NULL, version bigint NOT NULL, root bytea NOT NULL, parent_root bytea NULL,
  author text, ts timestamptz NOT NULL DEFAULT now(), message text, nbytes bigint, nlines bigint,
  kind text,                -- direct, rebased, merged
  base_version bigint,      -- the version the writer started from (NULL for version 1)
  PRIMARY KEY (file_id, version)
);
-- Change feed: one row per create, commit, mkdir, move and delete, written in the same
-- transaction as the change; each row is also announced with pg_notify('textdb_change', seq).
CREATE TABLE kb.change (
  seq          bigserial PRIMARY KEY,
  ts           timestamptz NOT NULL DEFAULT now(),
  op           text NOT NULL,        -- create, commit, mkdir, move, delete
  node_id      bigint NOT NULL,
  node_kind    smallint NOT NULL,    -- 0 folder, 1 file
  path         text NOT NULL,        -- the path after the change
  old_path     text,                 -- move: the path before
  version      bigint,               -- create, commit: the new version
  base_version bigint,               -- commit: the version the writer started from
  commit_kind  text,                 -- create, commit: direct, rebased, merged
  author       text,
  message      text
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
-- Path history (textdb_core::path): one row per node a rename, move or delete touched — the
-- node it named and everything below a folder — next to the versions in kb.commit.
CREATE TABLE kb.path_event (
  id          bigserial PRIMARY KEY,
  node_id     bigint NOT NULL,
  node_kind   smallint NOT NULL,     -- 0 folder, 1 file
  op          text NOT NULL,         -- rename, move, delete
  ts          timestamptz NOT NULL DEFAULT now(),
  author      text,
  old_path    text NOT NULL,
  new_path    text,                  -- rename, move: the path after
  via         text,                  -- the folder the operation named, when this node went with it
  version     bigint,                -- a file's version when it happened
  change_seq  bigint                 -- the kb.change row of the operation
);
CREATE INDEX path_event_node ON kb.path_event(node_id, id);
-- Store settings, e.g. path_history = on | off. A missing row means the default.
CREATE TABLE kb.setting (key text PRIMARY KEY, value text NOT NULL);
-- Who wrote each file: one row per (file, author) with that author's commits; '' is a commit
-- without an author.
CREATE TABLE kb.file_author (
  file_id  bigint NOT NULL,
  author   text NOT NULL,
  commits  bigint NOT NULL,
  first_ts timestamptz NOT NULL,
  last_ts  timestamptz NOT NULL,
  PRIMARY KEY (file_id, author)
);
-- Changes to folder totals, one row per folder above each commit, mkdir, move and delete.
-- Insert-only, so concurrent writers never wait on a shared ancestor's row (updating the root
-- folder's totals in place would serialize every commit in the store). kb.entry adds them to
-- the node rows; kb.compact_folder_totals() folds them in.
CREATE TABLE kb.folder_delta (
  folder_id bigint NOT NULL,
  files     bigint NOT NULL,
  folders   bigint NOT NULL,
  bytes     bigint NOT NULL,
  lines     bigint NOT NULL,
  words     bigint NOT NULL,
  versions  bigint NOT NULL,
  ts        timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX folder_delta_folder ON kb.folder_delta(folder_id);
-- textdb sync (the CLI): what a store folder and a directory held when they were last
-- reconciled, with the git commit the checkout was at, and each file both sides agreed on.
-- The CLI also creates these IF NOT EXISTS, for stores installed before they were added.
CREATE TABLE kb.sync (
  id bigserial PRIMARY KEY, prefix text NOT NULL, dir text NOT NULL, seq bigint NOT NULL,
  synced_at timestamptz NOT NULL DEFAULT now(), author text,
  git_commit text, git_branch text, git_remote text, git_clean boolean,
  UNIQUE (prefix, dir)
);
CREATE TABLE kb.sync_file (
  sync_id bigint NOT NULL REFERENCES kb.sync(id) ON DELETE CASCADE, rel text NOT NULL,
  version bigint, blob text NOT NULL, disk_size bigint, disk_mtime bigint,
  conflict boolean NOT NULL DEFAULT false,
  PRIMARY KEY (sync_id, rel)
);

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

-- An on/off value: on, true, yes, 1 / off, false, no, 0 in any case; NULL for anything else.
CREATE FUNCTION kb._switch(v text) RETURNS boolean LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT CASE lower(trim(v)) WHEN 'on' THEN true WHEN 'true' THEN true WHEN 'yes' THEN true WHEN '1' THEN true
                             WHEN 'off' THEN false WHEN 'false' THEN false WHEN 'no' THEN false WHEN '0' THEN false END
$$;

-- Store settings. kb.setting(key) is NULL at the default; kb.set_setting(key, NULL) returns a
-- setting to its default. The only setting so far is path_history.
CREATE FUNCTION kb.setting(k text) RETURNS text LANGUAGE sql STABLE AS $$
  SELECT s.value FROM kb.setting s WHERE s.key = k
$$;
CREATE FUNCTION kb.set_setting(k text, v text) RETURNS text LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  IF k IS DISTINCT FROM 'path_history' THEN
    PERFORM kb._raise('TX004', format('unknown setting ''%s'' (known: path_history)', k), NULL);
  END IF;
  IF v IS NULL THEN
    DELETE FROM kb.setting s WHERE s.key = k;
    RETURN NULL;
  END IF;
  IF kb._switch(v) IS NULL THEN
    PERFORM kb._raise('TX004', format('%s is on or off, not ''%s''', k, v), NULL);
  END IF;
  INSERT INTO kb.setting AS s (key, value) VALUES (k, CASE WHEN kb._switch(v) THEN 'on' ELSE 'off' END)
    ON CONFLICT ON CONSTRAINT setting_pkey DO UPDATE SET value = EXCLUDED.value;
  RETURN kb.setting(k);
END $$;

-- Whether renames, moves and deletes are recorded: this session's textdb.path_history
-- (SET textdb.path_history = off, or ALTER ROLE / ALTER DATABASE … SET for a default), else the
-- store's path_history setting, else on.
CREATE FUNCTION kb.path_history_enabled() RETURNS boolean LANGUAGE sql STABLE AS $$
  SELECT coalesce(kb._switch(nullif(current_setting('textdb.path_history', true), '')),
                  kb._switch(kb.setting('path_history')),
                  true)
$$;
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

-- Files and folders as a listing shows them. A folder's size, lines, words and versions are
-- totals over every live file below it: its node row plus the kb.folder_delta rows not yet
-- folded in. `authors`: a file's committers, most commits first.
CREATE VIEW kb.entry AS
  SELECT n.id, n.parent_id, n.path, n.name,
         CASE n.kind WHEN 1 THEN 'file' ELSE 'folder' END AS kind,
         CASE n.kind WHEN 1 THEN n.nbytes ELSE n.t_bytes + coalesce(d.bytes, 0) END AS nbytes,
         CASE n.kind WHEN 1 THEN n.nlines ELSE n.t_lines + coalesce(d.lines, 0) END AS nlines,
         CASE n.kind WHEN 1 THEN n.nwords ELSE n.t_words + coalesce(d.words, 0) END AS nwords,
         CASE n.kind WHEN 1 THEN n.version ELSE n.t_versions + coalesce(d.versions, 0) END AS versions,
         CASE n.kind WHEN 1 THEN n.updated_at ELSE greatest(n.updated_at, n.t_updated_at, d.ts) END AS updated_at,
         n.updated_by, n.created_at,
         CASE n.kind WHEN 0 THEN n.t_files + coalesce(d.files, 0) END AS files,
         CASE n.kind WHEN 0 THEN n.t_folders + coalesce(d.folders, 0) END AS folders,
         CASE n.kind WHEN 1 THEN n.nauthors END AS nauthors,
         CASE n.kind WHEN 1 THEN coalesce(
           (SELECT jsonb_agg(jsonb_build_object('author', nullif(a.author, ''), 'commits', a.commits, 'first_ts', a.first_ts, 'last_ts', a.last_ts)
                             ORDER BY a.commits DESC, a.last_ts DESC)
            FROM kb.file_author a WHERE a.file_id = n.id), '[]'::jsonb)
         ELSE '[]'::jsonb END AS authors
  FROM kb.node n
  LEFT JOIN LATERAL (
    SELECT sum(x.files)::bigint AS files, sum(x.folders)::bigint AS folders, sum(x.bytes)::bigint AS bytes,
           sum(x.lines)::bigint AS lines, sum(x.words)::bigint AS words, sum(x.versions)::bigint AS versions, max(x.ts) AS ts
    FROM kb.folder_delta x WHERE x.folder_id = n.id
  ) d ON n.kind = 0
  WHERE n.deleted_at IS NULL;

-- The files and folders in the folder `path`, by name; with `recursive`, everything below it, by path.
CREATE FUNCTION kb.ls(path text, recursive boolean DEFAULT false) RETURNS SETOF kb.entry LANGUAGE sql STABLE AS $$
  SELECT e.* FROM kb.node d JOIN kb.entry e
    ON CASE WHEN recursive THEN e.path <> '/' AND (d.path = '/' OR e.path LIKE kb._subtree_like(d.path)) ELSE e.parent_id = d.id END
  WHERE d.id = kb._node_id(path)
  ORDER BY CASE WHEN recursive THEN e.path ELSE e.name END
$$;

-- Fold kb.folder_delta into the node rows; returns the rows folded. Safe while others write
-- (one statement: a reader sees the rows before or after, never half); run it from a
-- maintenance job when the table grows.
CREATE FUNCTION kb.compact_folder_totals() RETURNS bigint LANGUAGE sql VOLATILE AS $$
  WITH gone AS (DELETE FROM kb.folder_delta RETURNING *),
       s AS (SELECT folder_id, sum(files) AS files, sum(folders) AS folders, sum(bytes) AS bytes, sum(lines) AS lines,
                    sum(words) AS words, sum(versions) AS versions, max(ts) AS ts, count(*) AS n
             FROM gone GROUP BY folder_id),
       u AS (UPDATE kb.node f SET t_files = f.t_files + s.files, t_folders = f.t_folders + s.folders, t_bytes = f.t_bytes + s.bytes,
                                  t_lines = f.t_lines + s.lines, t_words = f.t_words + s.words, t_versions = f.t_versions + s.versions,
                                  t_updated_at = greatest(f.t_updated_at, s.ts)
             FROM s WHERE f.id = s.folder_id RETURNING s.n)
  SELECT coalesce(sum(n), 0)::bigint FROM u
$$;

-- Recompute every folder's totals from the files below it, discarding kb.folder_delta. A
-- repair for totals that drifted, which a move racing a commit inside the moved folder can do.
CREATE FUNCTION kb.rebuild_folder_totals() RETURNS void LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  LOCK TABLE kb.folder_delta IN EXCLUSIVE MODE;
  DELETE FROM kb.folder_delta;
  UPDATE kb.node f SET t_files = coalesce(s.files, 0), t_folders = coalesce(s.folders, 0), t_bytes = coalesce(s.bytes, 0),
                       t_lines = coalesce(s.lines, 0), t_words = coalesce(s.words, 0), t_versions = coalesce(s.versions, 0),
                       t_updated_at = s.ts
  FROM kb.node f2 LEFT JOIN LATERAL (
    SELECT count(*) FILTER (WHERE n.kind = 1) AS files, count(*) FILTER (WHERE n.kind = 0) AS folders,
           sum(n.nbytes) FILTER (WHERE n.kind = 1) AS bytes, sum(n.nlines) FILTER (WHERE n.kind = 1) AS lines,
           sum(n.nwords) FILTER (WHERE n.kind = 1) AS words, sum(n.version) FILTER (WHERE n.kind = 1) AS versions,
           max(n.updated_at) AS ts
    FROM kb.node n
    WHERE n.deleted_at IS NULL AND n.path <> '/' AND (f2.path = '/' OR n.path LIKE kb._subtree_like(f2.path))
  ) s ON true
  WHERE f.id = f2.id AND f2.kind = 0 AND f2.deleted_at IS NULL;
END $$;

-- Attributed namespace changes (recorded in the change feed with `author`).
CREATE FUNCTION kb.move(from_path text, to_path text, author text DEFAULT NULL) RETURNS void LANGUAGE sql VOLATILE AS $$ SELECT kb._move(from_path, to_path, author) $$;
CREATE FUNCTION kb.remove(path text, author text DEFAULT NULL) RETURNS void LANGUAGE sql VOLATILE AS $$ SELECT kb._remove(path, author) $$;

CREATE FUNCTION kb.file_iud() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    -- Plain INSERT is an upsert: identical content produces no new version
    -- (ON CONFLICT is not available on views with INSTEAD OF triggers).
    PERFORM kb._upsert(NEW.path, coalesce(NEW.content, ''), NEW.updated_by, 'insert');
    RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN
    IF NEW.path IS DISTINCT FROM OLD.path THEN
      PERFORM kb.move(OLD.path, NEW.path, NEW.updated_by);
    END IF;
    IF NEW.base_version IS NOT NULL OR NEW.content IS DISTINCT FROM OLD.content THEN
      PERFORM kb._update_content(NEW.path, coalesce(NEW.content, ''), NEW.base_version, NEW.updated_by, 'update');
    END IF;
    RETURN NEW;
  ELSE
    PERFORM kb.remove(OLD.path, NULL::text);
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
    IF NEW.path IS DISTINCT FROM OLD.path THEN PERFORM kb.move(OLD.path, NEW.path, NULL::text); END IF;
    RETURN NEW;
  ELSE
    PERFORM kb.remove(OLD.path, NULL::text);
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
-- As kb._check, but returns the whole outcome `{"version": n, "kind": "direct|rebased|merged|noop"}`.
CREATE FUNCTION kb._check_j(r jsonb) RETURNS jsonb LANGUAGE plpgsql AS $$
BEGIN
  IF r ? 'code' THEN
    RAISE EXCEPTION USING ERRCODE = r->>'code', MESSAGE = r->>'message', DETAIL = coalesce(r->>'detail', '');
  END IF;
  RETURN r;
END $$;
CREATE FUNCTION kb.write(path text, content text, base_version bigint DEFAULT NULL, author text DEFAULT NULL, message text DEFAULT NULL) RETURNS jsonb LANGUAGE sql VOLATILE AS $$ SELECT kb._check_j(kb._write_j(path, content, base_version, author, message)) $$;
CREATE FUNCTION kb.replace_lines(path text, l_from bigint, l_to bigint, body text, base_version bigint DEFAULT NULL, author text DEFAULT NULL) RETURNS jsonb LANGUAGE sql VOLATILE AS $$ SELECT kb._check_j(kb._replace_lines_j(path, l_from, l_to, body, base_version, author)) $$;
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
    use textdb_core::tree::{leaves, locate_line, materialize, materialize_range, totals};
    use textdb_core::path::ancestors;
    use textdb_core::{
        unified_diff, word_delta, words_at, ChunkParams, Edit, Hash, LineHunk, PathOp, Storage, StructureExtractor, TextdbError,
    };
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

    fn storage_err(e: pgrx::spi::Error) -> TextdbError {
        TextdbError::Storage(e.to_string())
    }

    /// Outcome of a write as JSON for the PL/pgSQL wrappers `kb._check` / `kb._check_j`, which
    /// turn an error object into `RAISE EXCEPTION USING ERRCODE = …` (pgrx re-raises panics as
    /// XX000, so custom SQLSTATEs must be raised from SQL). Success is
    /// `{"version": n, "kind": "direct|rebased|merged|noop"}`.
    fn json_result(r: Result<(i64, CommitKind), TextdbError>) -> pgrx::JsonB {
        pgrx::JsonB(match r {
            Ok((v, k)) => serde_json::json!({ "version": v, "kind": k.as_str() }),
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
        ok(ensure_folder(&path))
    }

    /// `mkdir -p` of a normalized path; each folder it creates gets a `mkdir` feed row.
    fn ensure_folder(path: &str) -> Result<i64, TextdbError> {
        if let Some(n) = node_by_path(path) {
            if n.kind != 0 {
                return Err(TextdbError::InvalidEdit(format!("{} is a file", path)));
            }
            return Ok(n.id);
        }
        if path == "/" {
            return Err(TextdbError::Storage("root folder missing".into()));
        }
        let parent = ensure_folder(parent_of(path))?;
        let id = Spi::get_one_with_args::<i64>(
            "INSERT INTO kb.node(parent_id, name, kind, path) VALUES ($1, $2, 0, $3) RETURNING id",
            &[parent.into(), name_of(path).into(), path.into()],
        )
        .map_err(storage_err)?
        .ok_or_else(|| TextdbError::Storage("folder insert returned no id".into()))?;
        record_change("mkdir", id, 0, path, None, None, None, None, None, None);
        add_to_ancestors(
            path,
            &Totals {
                folders: 1,
                ..Totals::default()
            },
        )?;
        Ok(id)
    }

    /// What a node adds to every folder above it.
    #[derive(Clone, Copy, Debug, Default)]
    struct Totals {
        files: i64,
        folders: i64,
        bytes: i64,
        lines: i64,
        words: i64,
        versions: i64,
    }

    impl Totals {
        fn neg(self) -> Self {
            Totals {
                files: -self.files,
                folders: -self.folders,
                bytes: -self.bytes,
                lines: -self.lines,
                words: -self.words,
                versions: -self.versions,
            }
        }
    }

    /// Record `t` against every live folder above `path`, as kb.folder_delta rows.
    fn add_to_ancestors(path: &str, t: &Totals) -> Result<(), TextdbError> {
        let list = serde_json::to_string(&ancestors(path)).expect("paths serialize");
        Spi::run_with_args(
            "INSERT INTO kb.folder_delta(folder_id, files, folders, bytes, lines, words, versions) \
             SELECT n.id, $2, $3, $4, $5, $6, $7 FROM kb.node n \
             WHERE n.path IN (SELECT jsonb_array_elements_text($1::jsonb)) AND n.deleted_at IS NULL",
            &[
                list.as_str().into(),
                t.files.into(),
                t.folders.into(),
                t.bytes.into(),
                t.lines.into(),
                t.words.into(),
                t.versions.into(),
            ],
        )
        .map_err(storage_err)?;
        Ok(())
    }

    /// What the live node `id` and everything below it add to the folders above: a file counts
    /// itself, a folder its totals plus itself.
    fn subtree_totals(id: i64) -> Result<Totals, TextdbError> {
        Spi::connect(|client| {
            let rows = client
                .select(
                    "SELECT CASE kind WHEN 'file' THEN 1 ELSE files END::bigint, CASE kind WHEN 'file' THEN 0 ELSE folders + 1 END::bigint, \
                     coalesce(nbytes, 0)::bigint, coalesce(nlines, 0)::bigint, coalesce(nwords, 0)::bigint, versions::bigint \
                     FROM kb.entry WHERE id = $1",
                    None,
                    &[id.into()],
                )
                .map_err(storage_err)?;
            for r in rows {
                let get = |i: usize| -> Result<i64, TextdbError> { Ok(r.get::<i64>(i).map_err(storage_err)?.unwrap_or(0)) };
                return Ok(Totals {
                    files: get(1)?,
                    folders: get(2)?,
                    bytes: get(3)?,
                    lines: get(4)?,
                    words: get(5)?,
                    versions: get(6)?,
                });
            }
            Err(TextdbError::NotFound(format!("node {}", id)))
        })
    }

    /// Id of the live node at `path`, for `kb.ls`; TX003 when there is none.
    #[pg_extern(stable)]
    fn _node_id(path: &str) -> i64 {
        let path = ok(normalize_path(path));
        match node_by_path(&path) {
            Some(n) => n.id,
            None => fail(TextdbError::NotFound(path)),
        }
    }

    /// Append one row to the change feed and announce its `seq` on the `textdb_change`
    /// channel; returns the `seq`. Runs inside the calling statement's transaction, so the row
    /// and the notification exist exactly when the change commits.
    #[allow(clippy::too_many_arguments)]
    fn record_change(
        op: &str,
        node_id: i64,
        node_kind: i16,
        path: &str,
        old_path: Option<&str>,
        version: Option<i64>,
        base_version: Option<i64>,
        commit_kind: Option<&str>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> i64 {
        Spi::run_with_args(
            "WITH c AS (INSERT INTO kb.change(op, node_id, node_kind, path, old_path, version, base_version, commit_kind, author, message) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING seq) SELECT pg_notify('textdb_change', seq::text) FROM c",
            &[
                op.into(),
                node_id.into(),
                node_kind.into(),
                path.into(),
                old_path.into(),
                version.into(),
                base_version.into(),
                commit_kind.into(),
                author.into(),
                message.into(),
            ],
        )
        .unwrap_or_else(|e| spi_err(e));
        Spi::get_one::<i64>("SELECT currval(pg_get_serial_sequence('kb.change', 'seq'))")
            .unwrap_or_else(|e| spi_err(e))
            .unwrap_or(0)
    }

    fn path_history_enabled() -> Result<bool, TextdbError> {
        Ok(Spi::get_one::<bool>("SELECT kb.path_history_enabled()").map_err(storage_err)?.unwrap_or(true))
    }

    /// Record `op` in kb.path_event for `node` and, for a folder, every live node below it.
    /// Call before the paths change: the rows keep the paths as they were.
    fn record_path_events(
        op: PathOp,
        node: &NodeRow,
        to: Option<&str>,
        author: Option<&str>,
        change_seq: i64,
    ) -> Result<(), TextdbError> {
        if node.kind == 0 {
            Spi::run_with_args(
                "INSERT INTO kb.path_event(node_id, node_kind, op, author, old_path, new_path, via, version, change_seq) \
                 SELECT id, kind, $1, $2, path, CASE WHEN $3::text IS NULL THEN NULL ELSE $3::text || substr(path, length($4) + 1) END, \
                        $4, CASE WHEN kind = 1 THEN version END, $5 \
                 FROM kb.node WHERE path LIKE kb._subtree_like($4) AND deleted_at IS NULL",
                &[op.as_str().into(), author.into(), to.into(), node.path.as_str().into(), change_seq.into()],
            )
            .map_err(storage_err)?;
        }
        Spi::run_with_args(
            "INSERT INTO kb.path_event(node_id, node_kind, op, author, old_path, new_path, version, change_seq) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                node.id.into(),
                node.kind.into(),
                op.as_str().into(),
                author.into(),
                node.path.as_str().into(),
                to.into(),
                (node.kind == 1).then_some(node.version).into(),
                change_seq.into(),
            ],
        )
        .map_err(storage_err)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn record_commit(
        file_id: i64,
        path: &str,
        c: &Committed,
        parent_root: Option<&Hash>,
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) {
        let st = SpiStorage::new();
        let (nbytes, nlines) = ok(totals(&st, &c.root));
        let (old_bytes, old_lines, old_words) = Spi::connect(|client| {
            let rows = client
                .select("SELECT nbytes, nlines, nwords FROM kb.node WHERE id = $1", None, &[file_id.into()])
                .unwrap_or_else(|e| spi_err(e));
            let mut out = (None, None, None);
            for r in rows {
                out = (
                    r.get::<i64>(1).unwrap_or_else(|e| spi_err(e)),
                    r.get::<i64>(2).unwrap_or_else(|e| spi_err(e)),
                    r.get::<i64>(3).unwrap_or_else(|e| spi_err(e)),
                );
            }
            out
        });
        // Words over the lines that changed since the previous version. That is the previous
        // version's root rather than `parent_root`: with concurrent writers, `commit` may have
        // rebased over a commit that landed after the caller read its head.
        let nwords = match old_words {
            Some(words) if c.version > 1 => words + ok(word_delta(&st, &root_of_version(file_id, c.version - 1), &c.root)),
            _ => ok(words_at(&st, &c.root)) as i64,
        };
        Spi::run_with_args(
            "INSERT INTO kb.commit(file_id, version, root, parent_root, author, message, nbytes, nlines, kind, base_version) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                file_id.into(),
                (c.version as i64).into(),
                c.root.to_vec().into(),
                parent_root.map(|h| h.to_vec()).into(),
                author.into(),
                message.into(),
                (nbytes as i64).into(),
                (nlines as i64).into(),
                c.kind.as_str().into(),
                base_version.into(),
            ],
        )
        .unwrap_or_else(|e| spi_err(e));
        let op = if c.version == 1 { "create" } else { "commit" };
        record_change(op, file_id, 1, path, None, Some(c.version as i64), base_version, Some(c.kind.as_str()), author, message);
        Spi::run_with_args(
            "INSERT INTO kb.file_author AS a (file_id, author, commits, first_ts, last_ts) VALUES ($1, coalesce($2, ''), 1, now(), now()) \
             ON CONFLICT (file_id, author) DO UPDATE SET commits = a.commits + 1, last_ts = EXCLUDED.last_ts",
            &[file_id.into(), author.into()],
        )
        .unwrap_or_else(|e| spi_err(e));
        Spi::run_with_args(
            "UPDATE kb.node SET nbytes = $1, nlines = $2, updated_by = $3, nwords = $5, \
             nauthors = (SELECT count(*) FROM kb.file_author WHERE file_id = $4) WHERE id = $4",
            &[(nbytes as i64).into(), (nlines as i64).into(), author.into(), file_id.into(), nwords.into()],
        )
        .unwrap_or_else(|e| spi_err(e));
        let change = Totals {
            files: (c.version == 1) as i64,
            folders: 0,
            bytes: nbytes as i64 - old_bytes.unwrap_or(0),
            lines: nlines as i64 - old_lines.unwrap_or(0),
            words: nwords - old_words.unwrap_or(0),
            versions: 1,
        };
        ok(add_to_ancestors(path, &change));
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
        ok(create_impl(path, content, author, message))
    }

    fn create_impl(path: &str, content: &str, author: Option<&str>, message: Option<&str>) -> Result<i64, TextdbError> {
        let path = normalize_path(path)?;
        if node_by_path(&path).is_some() {
            return Err(TextdbError::InvalidEdit(format!("{} already exists", path)));
        }
        let parent = ensure_folder(parent_of(&path))?;
        let id = Spi::get_one_with_args::<i64>(
            "INSERT INTO kb.node(parent_id, name, kind, path, updated_by) VALUES ($1, $2, 1, $3, $4) RETURNING id",
            &[parent.into(), name_of(&path).into(), path.as_str().into(), author.into()],
        )
        .map_err(storage_err)?
        .ok_or_else(|| TextdbError::Storage("file insert returned no id".into()))?;
        let mut st = SpiStorage::new();
        let (root, chunks) = textdb_core::build_with_chunks(&mut st, &P, content.as_bytes())?;
        if !st.cas_root(id as u64, None, &root)? {
            return Err(TextdbError::Storage("initial CAS failed".into()));
        }
        let c = Committed {
            version: 1,
            root,
            kind: CommitKind::Direct,
            new_chunks: chunks,
            retries: 0,
        };
        record_commit(id, &path, &c, None, None, author, message);
        Ok(1)
    }

    /// Upsert: create, or update content (identical content → no new version).
    #[pg_extern(volatile)]
    fn _upsert(path: &str, content: &str, author: Option<&str>, message: Option<&str>) -> i64 {
        ok(write_impl(path, content, None, author, message)).0
    }

    /// Create the file, or replace its content exactly as `_update_content_j` does.
    /// JSON outcome `{version, kind}`; `kb.write(...)` (SQL) raises from it.
    #[pg_extern(volatile)]
    fn _write_j(path: &str, content: &str, base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> pgrx::JsonB {
        json_result(write_impl(path, content, base_version, author, message))
    }

    fn write_impl(path: &str, content: &str, base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<(i64, CommitKind), TextdbError> {
        let path = normalize_path(path)?;
        match node_by_path(&path) {
            None => Ok((create_impl(&path, content, author, message)?, CommitKind::Direct)),
            Some(_) => update_content_impl(&path, content, base_version, author, message),
        }
    }

    /// Replace lines `[l_from, l_to]` (1-based, inclusive) with `body`, where the numbers refer
    /// to `base_version` (HEAD when NULL). `l_to = l_from - 1` inserts in front of `l_from`;
    /// `l_from` one past the last line appends. Committed with that base version, so commits
    /// that landed elsewhere in the meantime are rebased over (or TX001 on the same lines).
    /// JSON outcome; `kb.replace_lines(...)` (SQL) raises from it.
    #[pg_extern(volatile)]
    fn _replace_lines_j(path: &str, l_from: i64, l_to: i64, body: &str, base_version: Option<i64>, author: Option<&str>) -> pgrx::JsonB {
        json_result(replace_lines_impl(path, l_from, l_to, body, base_version, author))
    }

    fn replace_lines_impl(path: &str, l_from: i64, l_to: i64, body: &str, base_version: Option<i64>, author: Option<&str>) -> Result<(i64, CommitKind), TextdbError> {
        let path = normalize_path(path)?;
        let from = l_from.max(0) as u64;
        let to = l_to.max(0) as u64;
        if from == 0 || to + 1 < from {
            return Err(TextdbError::InvalidEdit(format!("invalid line range {}-{}", from, to)));
        }
        let n = file_by_path_r(&path)?;
        let base_v = base_version.map_or(n.version, |v| v.max(0));
        let root = match n.root {
            Some(r) if base_v == n.version => r,
            _ => root_of_version_r(n.id, base_v as u64)?,
        };
        let edit = {
            let st = SpiStorage::new();
            let (len, newlines) = totals(&st, &root)?;
            let unterminated = len > 0 && materialize_range(&st, &root, len - 1, len)? != b"\n";
            let nlines = newlines + unterminated as u64;
            if from - 1 > nlines || to > nlines {
                return Err(TextdbError::InvalidEdit(format!(
                    "lines {}-{} are outside {}, which has {} lines at version {}",
                    from, to, path, nlines, base_v
                )));
            }
            let mut replacement = body.as_bytes().to_vec();
            let start = match locate_line(&st, &root, from - 1)? {
                Some(off) => off,
                // Appending after a last line with no newline: supply one, or the new text
                // would run on from that line instead of following it.
                None => {
                    replacement.insert(0, b'\n');
                    len
                }
            };
            let end = if to < from { start } else { locate_line(&st, &root, to)?.unwrap_or(len) };
            Edit::new(start, end, replacement)
        };
        commit_edits_impl(&path, &[edit], Some(base_v), author, Some("replace-lines"))
    }

    /// Commit a byte-range edit set expressed against `base_version` (or HEAD).
    fn commit_edits_impl(path: &str, edits: &[Edit], base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<(i64, CommitKind), TextdbError> {
        let n = file_by_path_r(path)?;
        let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.to_string()))?;
        let base = match base_version {
            Some(v) if v != n.version => root_of_version_r(n.id, v as u64)?,
            _ => cur,
        };
        let mut st = SpiStorage::new();
        let c = commit(&mut st, &P, n.id as u64, path, &base, edits, RETRIES)?;
        if c.kind != CommitKind::NoOp {
            record_commit(n.id, path, &c, Some(&cur), Some(base_version.unwrap_or(n.version)), author, message);
        }
        Ok((c.version as i64, c.kind))
    }

    /// Whole-content update: diff against `base_version` (or HEAD) → edit set → commit with rebase.
    /// Returns a JSON outcome; `kb._update_content` (SQL) raises TX001/TX002/… from it.
    #[pg_extern(volatile)]
    fn _update_content_j(path: &str, content: &str, base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> pgrx::JsonB {
        json_result(update_content_impl(path, content, base_version, author, message))
    }

    fn update_content_impl(path: &str, content: &str, base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<(i64, CommitKind), TextdbError> {
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
            return Ok((n.version, CommitKind::NoOp));
        }
        let c = commit(&mut st, &P, n.id as u64, &path, &base, &edits, RETRIES)?;
        if c.kind != CommitKind::NoOp {
            record_commit(n.id, &path, &c, Some(&cur), Some(base_version.unwrap_or(n.version)), author, message);
        }
        Ok((c.version as i64, c.kind))
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

    fn edit_impl(path: &str, old: &str, new: &str, author: Option<&str>) -> Result<(i64, CommitKind), TextdbError> {
        let path = normalize_path(path)?;
        let n = file_by_path_r(&path)?;
        let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let mut st = SpiStorage::new();
        let content = materialize(&st, &cur)?;
        let pos = find_unique(&content, old.as_bytes())?;
        let edits = [Edit::new(pos as u64, (pos + old.len()) as u64, new.as_bytes().to_vec())];
        let c = commit(&mut st, &P, n.id as u64, &path, &cur, &edits, RETRIES)?;
        if c.kind != CommitKind::NoOp {
            record_commit(n.id, &path, &c, Some(&cur), Some(n.version), author, Some("edit"));
        }
        Ok((c.version as i64, c.kind))
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

    fn append_impl(path: &str, tail: &str, author: Option<&str>) -> Result<(i64, CommitKind), TextdbError> {
        let path = normalize_path(path)?;
        let n = file_by_path_r(&path)?;
        let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let mut st = SpiStorage::new();
        let c = commit_append(&mut st, &P, n.id as u64, &path, tail.as_bytes(), RETRIES)?;
        if c.kind != CommitKind::NoOp {
            record_commit(n.id, &path, &c, Some(&cur), Some(n.version), author, Some("append"));
        }
        Ok((c.version as i64, c.kind))
    }

    /// Rename/move a file or folder: subtree path rewrite in one statement, ids stable.
    #[pg_extern(volatile)]
    fn _rename(from: &str, to: &str) {
        ok(rename_impl(from, to, None))
    }

    /// As `_rename`, attributing the move in the change feed (`kb.move` in SQL).
    #[pg_extern(volatile)]
    fn _move(from_path: &str, to_path: &str, author: Option<&str>) {
        ok(rename_impl(from_path, to_path, author))
    }

    fn rename_impl(from: &str, to: &str, author: Option<&str>) -> Result<(), TextdbError> {
        let from = normalize_path(from)?;
        let to = normalize_path(to)?;
        if from == "/" || to == "/" {
            return Err(TextdbError::InvalidEdit("cannot move the root".into()));
        }
        let src = node_by_path(&from).ok_or_else(|| TextdbError::NotFound(from.clone()))?;
        if to == from || to.starts_with(&format!("{}/", from)) {
            return Err(TextdbError::InvalidEdit(format!("cannot move {} into itself", from)));
        }
        if node_by_path(&to).is_some() {
            return Err(TextdbError::InvalidEdit(format!("{} already exists", to)));
        }
        let parent = ensure_folder(parent_of(&to))?;
        let moved = subtree_totals(src.id)?;
        add_to_ancestors(&from, &moved.neg())?;
        let seq = record_change("move", src.id, src.kind, &to, Some(&from), None, None, None, author, None);
        if path_history_enabled()? {
            record_path_events(PathOp::classify(&from, &to), &src, Some(&to), author, seq)?;
        }
        Spi::run_with_args(
            "UPDATE kb.node SET path = $2 || substr(path, length($1) + 1), updated_at = now() WHERE path LIKE kb._subtree_like($1) AND deleted_at IS NULL",
            &[from.as_str().into(), to.as_str().into()],
        )
        .map_err(storage_err)?;
        Spi::run_with_args(
            "UPDATE kb.node SET path = $1, name = $2, parent_id = $3, updated_at = now() WHERE id = $4",
            &[to.as_str().into(), name_of(&to).into(), parent.into(), src.id.into()],
        )
        .map_err(storage_err)?;
        add_to_ancestors(&to, &moved)?;
        Ok(())
    }

    /// Tombstone a file or folder subtree; content and history retained.
    #[pg_extern(volatile)]
    fn _delete(path: &str) {
        ok(delete_impl(path, None))
    }

    /// As `_delete`, attributing the change in the feed (`kb.remove` in SQL).
    #[pg_extern(volatile)]
    fn _remove(path: &str, author: Option<&str>) {
        ok(delete_impl(path, author))
    }

    fn delete_impl(path: &str, author: Option<&str>) -> Result<(), TextdbError> {
        let path = normalize_path(path)?;
        if path == "/" {
            return Err(TextdbError::InvalidEdit("cannot delete the root".into()));
        }
        let n = node_by_path(&path).ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let gone = subtree_totals(n.id)?;
        add_to_ancestors(&path, &gone.neg())?;
        let seq = record_change("delete", n.id, n.kind, &path, None, None, None, None, author, None);
        if path_history_enabled()? {
            record_path_events(PathOp::Delete, &n, None, author, seq)?;
        }
        Spi::run_with_args(
            "UPDATE kb.node SET deleted_at = now() WHERE (path = $1 OR path LIKE kb._subtree_like($1)) AND deleted_at IS NULL",
            &[path.as_str().into()],
        )
        .map_err(storage_err)?;
        Ok(())
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
    fn history(
        path: &str,
    ) -> TableIterator<
        'static,
        (
            name!(version, i64),
            name!(author, Option<String>),
            name!(ts, pgrx::datum::TimestampWithTimeZone),
            name!(message, Option<String>),
            name!(kind, Option<String>),
            name!(base_version, Option<i64>),
        ),
    > {
        let path = ok(normalize_path(path));
        let n = NodeRow::by_path(&path, true).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let rows: Vec<_> = Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT version, author, ts, message, kind, base_version FROM kb.commit WHERE file_id = $1 ORDER BY version",
                    None,
                    &[n.id.into()],
                )
                .unwrap_or_else(|e| spi_err(e));
            let mut v = Vec::new();
            for r in t {
                v.push((
                    r.get::<i64>(1).unwrap_or_else(|e| spi_err(e)).unwrap_or(0),
                    r.get::<String>(2).unwrap_or_else(|e| spi_err(e)),
                    r.get::<pgrx::datum::TimestampWithTimeZone>(3).unwrap_or_else(|e| spi_err(e)).expect("ts"),
                    r.get::<String>(4).unwrap_or_else(|e| spi_err(e)),
                    r.get::<String>(5).unwrap_or_else(|e| spi_err(e)),
                    r.get::<i64>(6).unwrap_or_else(|e| spi_err(e)),
                ));
            }
            v
        });
        TableIterator::new(rows)
    }

    /// Renames, moves and deletes of the file or folder at `path` (the live one, else the one
    /// most recently deleted there), oldest first. See `kb.path_history_enabled()`.
    #[pg_extern(stable)]
    fn path_history(
        path: &str,
    ) -> TableIterator<
        'static,
        (
            name!(id, i64),
            name!(ts, pgrx::datum::TimestampWithTimeZone),
            name!(op, String),
            name!(old_path, String),
            name!(new_path, Option<String>),
            name!(via, Option<String>),
            name!(version, Option<i64>),
            name!(author, Option<String>),
        ),
    > {
        let path = ok(normalize_path(path));
        let n = NodeRow::by_path(&path, true).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let rows: Vec<_> = Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT id, ts, op, old_path, new_path, via, version, author FROM kb.path_event WHERE node_id = $1 ORDER BY id",
                    None,
                    &[n.id.into()],
                )
                .unwrap_or_else(|e| spi_err(e));
            let mut v = Vec::new();
            for r in t {
                v.push((
                    r.get::<i64>(1).unwrap_or_else(|e| spi_err(e)).unwrap_or(0),
                    r.get::<pgrx::datum::TimestampWithTimeZone>(2).unwrap_or_else(|e| spi_err(e)).expect("ts"),
                    r.get::<String>(3).unwrap_or_else(|e| spi_err(e)).unwrap_or_default(),
                    r.get::<String>(4).unwrap_or_else(|e| spi_err(e)).unwrap_or_default(),
                    r.get::<String>(5).unwrap_or_else(|e| spi_err(e)),
                    r.get::<String>(6).unwrap_or_else(|e| spi_err(e)),
                    r.get::<i64>(7).unwrap_or_else(|e| spi_err(e)),
                    r.get::<String>(8).unwrap_or_else(|e| spi_err(e)),
                ));
            }
            v
        });
        TableIterator::new(rows)
    }

    /// Line hunks turning version `v1` into `v2`, 1-based lines. Defaults: `v2` = HEAD,
    /// `v1` = `v2 - 1`. Version 0 is the empty document (one whole-document hunk).
    #[pg_extern(stable)]
    fn hunks(
        path: &str,
        v1: default!(Option<i64>, "NULL"),
        v2: default!(Option<i64>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(old_from, i64),
            name!(old_count, i64),
            name!(new_from, i64),
            name!(new_count, i64),
            name!(old_text, String),
            name!(new_text, String),
        ),
    > {
        let path = ok(normalize_path(path));
        let n = NodeRow::by_path(&path, true).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let (v1, v2) = match (v1, v2) {
            (Some(a), Some(b)) => (a, b),
            (a, b) => {
                let b = b.unwrap_or(n.version);
                (a.unwrap_or(b - 1), b)
            }
        };
        let rows: Vec<(i64, i64, i64, i64, String, String)> = ok(hunks_between(&n, v1.max(0) as u64, v2.max(0) as u64))
            .into_iter()
            .map(|h| {
                (
                    h.old_from as i64 + 1,
                    h.old_count as i64,
                    h.new_from as i64 + 1,
                    h.new_count as i64,
                    String::from_utf8_lossy(&h.old_text).into_owned(),
                    String::from_utf8_lossy(&h.new_text).into_owned(),
                )
            })
            .collect();
        TableIterator::new(rows)
    }

    /// 0-based hunks between two versions of `n`; version 0 is the empty document, for
    /// which no root is stored (and a read must not write one).
    fn hunks_between(n: &NodeRow, v1: u64, v2: u64) -> Result<Vec<LineHunk>, TextdbError> {
        if v1 == v2 {
            return Ok(Vec::new());
        }
        let st = SpiStorage::new();
        if v1 == 0 || v2 == 0 {
            let root = root_of_version_r(n.id, v1.max(v2))?;
            let text = materialize(&st, &root)?;
            if text.is_empty() {
                return Ok(Vec::new());
            }
            let count = textdb_core::myers::split_lines(&text).len() as u64;
            let (old_count, new_count, old_text, new_text) =
                if v1 == 0 { (0, count, Vec::new(), text) } else { (count, 0, text, Vec::new()) };
            return Ok(vec![LineHunk {
                old_from: 0,
                old_count,
                new_from: 0,
                new_count,
                old_text,
                new_text,
            }]);
        }
        let a = root_of_version_r(n.id, v1)?;
        let b = root_of_version_r(n.id, v2)?;
        textdb_core::line_hunks(&st, &a, &b)
    }

    /// The chunks of a file at `version` (HEAD when NULL) in document order; unchanged
    /// content keeps its hash across versions. `line_from` is 1-based, `hash` is hex.
    #[pg_extern(stable)]
    fn chunks(
        path: &str,
        version: default!(Option<i64>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(ord, i64),
            name!(hash, String),
            name!(byte_from, i64),
            name!(nbytes, i64),
            name!(line_from, i64),
            name!(nlines, i64),
        ),
    > {
        let path = ok(normalize_path(path));
        let root = match version {
            None => {
                let n = file_by_path(&path);
                n.root.unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())))
            }
            Some(v) => {
                let n = NodeRow::by_path(&path, true).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
                root_of_version(n.id, v.max(0) as u64)
            }
        };
        let st = SpiStorage::new();
        let rows: Vec<(i64, String, i64, i64, i64, i64)> = ok(leaves(&st, &root))
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                (
                    i as i64,
                    textdb_core::hash::hex(&l.hash),
                    l.byte_off as i64,
                    l.nbytes as i64,
                    l.line_off as i64 + 1,
                    l.nlines as i64,
                )
            })
            .collect();
        TableIterator::new(rows)
    }

    /// Change-feed rows with `seq > since`, oldest first, at most `lim` of them.
    #[pg_extern(stable)]
    fn feed(
        since: default!(i64, 0),
        lim: default!(i64, 10000),
    ) -> TableIterator<
        'static,
        (
            name!(seq, i64),
            name!(ts, pgrx::datum::TimestampWithTimeZone),
            name!(op, String),
            name!(path, String),
            name!(old_path, Option<String>),
            name!(node_kind, String),
            name!(version, Option<i64>),
            name!(base_version, Option<i64>),
            name!(commit_kind, Option<String>),
            name!(author, Option<String>),
            name!(message, Option<String>),
        ),
    > {
        let rows: Vec<_> = Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT seq, ts, op, path, old_path, node_kind, version, base_version, commit_kind, author, message FROM kb.change WHERE seq > $1 ORDER BY seq LIMIT $2",
                    None,
                    &[since.into(), lim.max(0).into()],
                )
                .unwrap_or_else(|e| spi_err(e));
            let mut v = Vec::new();
            for r in t {
                let kind: i16 = r.get(6).unwrap_or_else(|e| spi_err(e)).unwrap_or(1);
                v.push((
                    r.get::<i64>(1).unwrap_or_else(|e| spi_err(e)).unwrap_or(0),
                    r.get::<pgrx::datum::TimestampWithTimeZone>(2).unwrap_or_else(|e| spi_err(e)).expect("ts"),
                    r.get::<String>(3).unwrap_or_else(|e| spi_err(e)).unwrap_or_default(),
                    r.get::<String>(4).unwrap_or_else(|e| spi_err(e)).unwrap_or_default(),
                    r.get::<String>(5).unwrap_or_else(|e| spi_err(e)),
                    if kind == 1 { "file".to_string() } else { "folder".to_string() },
                    r.get::<i64>(7).unwrap_or_else(|e| spi_err(e)),
                    r.get::<i64>(8).unwrap_or_else(|e| spi_err(e)),
                    r.get::<String>(9).unwrap_or_else(|e| spi_err(e)),
                    r.get::<String>(10).unwrap_or_else(|e| spi_err(e)),
                    r.get::<String>(11).unwrap_or_else(|e| spi_err(e)),
                ));
            }
            v
        });
        TableIterator::new(rows)
    }

    /// Newest `seq` in the change feed, 0 when it is empty.
    #[pg_extern(stable)]
    fn last_seq() -> i64 {
        Spi::get_one::<i64>("SELECT coalesce(max(seq), 0) FROM kb.change")
            .unwrap_or_else(|e| spi_err(e))
            .unwrap_or(0)
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
