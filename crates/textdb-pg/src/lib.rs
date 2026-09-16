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

mod bulk;
mod links;
mod property;
mod sections;
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
  -- What the structure extractor found, counted once at commit so a listing can say
  -- "12 headings, 4 properties, 2 links, 1 broken" without a query per row.
  title           text,
  nsections       bigint NOT NULL DEFAULT 0,
  nprops          bigint NOT NULL DEFAULT 0,
  nlinks          bigint NOT NULL DEFAULT 0,
  nlinks_broken   bigint NOT NULL DEFAULT 0,
  -- Folder: totals of every live node below it, as of the last kb.compact_folder_totals();
  -- kb.folder_delta holds the changes since. Zero on files.
  t_files      bigint NOT NULL DEFAULT 0,
  t_folders    bigint NOT NULL DEFAULT 0,
  t_bytes      bigint NOT NULL DEFAULT 0,
  t_lines      bigint NOT NULL DEFAULT 0,
  t_words      bigint NOT NULL DEFAULT 0,
  t_versions   bigint NOT NULL DEFAULT 0,
  t_sections     bigint NOT NULL DEFAULT 0,
  t_props        bigint NOT NULL DEFAULT 0,
  t_links        bigint NOT NULL DEFAULT 0,
  t_links_broken bigint NOT NULL DEFAULT 0,
  t_updated_at timestamptz NULL,
  t_updated_by text NULL                      -- and who made that change, so a folder names an author
);
CREATE UNIQUE INDEX node_path ON kb.node(path text_pattern_ops) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX node_parent_name ON kb.node(parent_id, name) WHERE deleted_at IS NULL;
CREATE INDEX node_parent ON kb.node(parent_id);
-- Link resolution looks files up by path and name, ignoring case.
CREATE INDEX node_lower_path ON kb.node(lower(path)) WHERE deleted_at IS NULL;
CREATE INDEX node_lower_name ON kb.node(lower(name)) WHERE deleted_at IS NULL;
INSERT INTO kb.node(parent_id, name, kind, path) VALUES (NULL, '', 0, '/');

CREATE TABLE kb.commit (
  file_id bigint NOT NULL, version bigint NOT NULL, root bytea NOT NULL, parent_root bytea NULL,
  author text, ts timestamptz NOT NULL DEFAULT now(), message text, nbytes bigint, nlines bigint,
  kind text,                -- direct, rebased, merged
  base_version bigint,      -- the version the writer started from (NULL for version 1)
  batch text,               -- the batch (session setting textdb.batch) the commit belongs to
  PRIMARY KEY (file_id, version)
, nwords bigint);
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
  message      text,
  batch        text                  -- the batch (session setting textdb.batch) the change belongs to
);
CREATE INDEX change_node ON kb.change(node_id);
CREATE INDEX change_batch ON kb.change(batch) WHERE batch IS NOT NULL;
CREATE TABLE kb.chunk (
  id bigserial PRIMARY KEY, hash bytea NOT NULL UNIQUE, bytes bytea NOT NULL, nlines int NOT NULL
);
CREATE TABLE kb.tree_node (hash bytea PRIMARY KEY, children bytea NOT NULL);
CREATE TABLE kb.chunk_ref (chunk_id bigint NOT NULL, file_id bigint NOT NULL, version bigint NOT NULL, PRIMARY KEY (chunk_id, file_id));
CREATE TABLE kb.section (file_id bigint NOT NULL, version bigint NOT NULL, heading_path text NOT NULL, level int NOT NULL, line_from bigint NOT NULL, line_to bigint NOT NULL,
  heading text NOT NULL DEFAULT '',            -- the last component of heading_path, as written
  heading_lc text NOT NULL DEFAULT '',         -- folded, so a match is an index seek
  -- The section's own lines, and it plus everything nested under it. Words never span a line
  -- and sections partition by line, so both are exact and the totals compose.
  nwords bigint, nwords_total bigint);
CREATE INDEX section_file ON kb.section(file_id, version);
CREATE INDEX section_heading ON kb.section(heading_lc text_pattern_ops);
CREATE INDEX section_level ON kb.section(level);
-- Links as written (target without anchor or alias), and what each resolves to by the rules the
-- stores share (textdb_md::resolve). target_name (last segment, lower case, without .md) finds
-- the rows a created, moved or deleted file can change.
CREATE TABLE kb.link (
  id          bigserial PRIMARY KEY,
  file_id     bigint NOT NULL,
  version     bigint NOT NULL,
  target_path text NOT NULL,
  line        bigint NOT NULL,
  kind        text,                          -- wiki, embed, md, image
  anchor      text,                          -- heading or ^block after #
  alias       text,
  external    boolean NOT NULL DEFAULT false,-- URL, email, query, numbered reference
  target_name text,
  resolved_id bigint,                        -- the file it points to
  status      text                           -- ok, ambiguous, anchor-missing, folder, broken, not-in-store, external
);
CREATE INDEX link_file ON kb.link(file_id, version);
CREATE INDEX link_target_name ON kb.link(target_name);
CREATE INDEX link_resolved ON kb.link(resolved_id);
CREATE TABLE kb.frontmatter (file_id bigint NOT NULL, version bigint NOT NULL, data jsonb, PRIMARY KEY (file_id, version));
-- One row per (document, property path, value), derived from `frontmatter` at every commit.
-- jsonb can be searched without this, but only by scanning: containment ran 9-12 ms over
-- 50,000 notes and `jsonb_object_keys` — the query behind "what properties does this vault
-- use" — took 1,007 ms. These rows make both an index seek, and give the same shape as the
-- SQLite binding so one query text means the same thing on either.
CREATE TABLE kb.property (
  file_id bigint NOT NULL,
  version bigint NOT NULL,
  key     text NOT NULL,                 -- dotted path as written: `project.name`
  key_lc  text NOT NULL,                 -- and folded, which is what the indexes carry
  val_txt text NULL,
  val_lc  text NULL,
  val_num double precision NULL,
  ord     bigint NOT NULL DEFAULT 0
);
CREATE INDEX property_kv ON kb.property(key_lc, val_lc);
CREATE INDEX property_kn ON kb.property(key_lc, val_num);
CREATE INDEX property_file ON kb.property(file_id);
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
  sections      bigint NOT NULL DEFAULT 0,
  props         bigint NOT NULL DEFAULT 0,
  links         bigint NOT NULL DEFAULT 0,
  links_broken  bigint NOT NULL DEFAULT 0,
  ts        timestamptz NOT NULL DEFAULT now(),
  updated_by text NULL                        -- the author of this change, carried with its ts
);
CREATE INDEX folder_delta_folder ON kb.folder_delta(folder_id);
-- textdb sync (the CLI): what a store folder and a directory held when they were last
-- reconciled, with the git commit the checkout was at, and each file both sides agreed on.
-- The CLI also creates these IF NOT EXISTS, for stores installed before they were added.
CREATE TABLE kb.sync (
  id bigserial PRIMARY KEY, prefix text NOT NULL, dir text NOT NULL, seq bigint NOT NULL,
  synced_at timestamptz NOT NULL DEFAULT now(), author text,
  git_commit text, git_branch text, git_remote text, git_clean boolean,
  rules text,               -- the include rules of that sync, as JSON
  UNIQUE (prefix, dir)
);
CREATE TABLE kb.sync_file (
  sync_id bigint NOT NULL REFERENCES kb.sync(id) ON DELETE CASCADE, rel text NOT NULL,
  version bigint, blob text NOT NULL, disk_size bigint, disk_mtime bigint,
  conflict boolean NOT NULL DEFAULT false,
  PRIMARY KEY (sync_id, rel)
);
-- Asset stores (docs/assets.md): where the bytes of assets are kept. Each computer binds a store
-- to where it reaches it (a folder, an rclone remote); `root` is the store-side identity. The CLI
-- also creates this IF NOT EXISTS.
CREATE TABLE kb.asset_store (
  name text PRIMARY KEY, driver text NOT NULL, root text NOT NULL, options text,
  created_at timestamptz NOT NULL DEFAULT now()
);

-- Accounts, bearer tokens and folder-scoped grants (#12). The same three tables as SQLite's
-- kb_account / kb_token / kb_grant, so one access model serves both engines.
CREATE TABLE kb.account (
  id           bigserial PRIMARY KEY,
  name         text NOT NULL UNIQUE,
  kind         text NOT NULL,                        -- 'agent' | 'person'
  root_node_id bigint NULL REFERENCES kb.node(id),   -- single-root accounts only
  created_at   timestamptz NOT NULL DEFAULT now(),
  disabled_at  timestamptz NULL
);
CREATE TABLE kb.token (
  id           bigserial PRIMARY KEY,
  account_id   bigint NOT NULL REFERENCES kb.account(id),
  hash         text NOT NULL UNIQUE,                 -- sha256 of the bearer, hex
  label        text,
  created_at   timestamptz NOT NULL DEFAULT now(),
  expires_at   timestamptz NULL,
  revoked_at   timestamptz NULL,
  last_used_at timestamptz NULL
);
CREATE INDEX token_account ON kb.token(account_id);
CREATE TABLE kb.grant (
  account_id  bigint NOT NULL REFERENCES kb.account(id),
  node_id     bigint NOT NULL REFERENCES kb.node(id),
  alias       text NOT NULL,                         -- '' for a single-root account
  rights      text NOT NULL,                         -- 'ro' | 'rw'
  granted_by  text,
  granted_at  timestamptz NOT NULL DEFAULT now(),
  -- A revoked grant keeps its row: an account whose checkout still holds the files must be told
  -- `forbidden`, not `not found`, or its next sync deletes them.
  revoked_at  timestamptz NULL,
  PRIMARY KEY (account_id, node_id)
);
CREATE UNIQUE INDEX grant_alias ON kb.grant(account_id, alias);

-- The connection's account, from the `textdb.token` GUC. NULL when no token is set, which is the
-- owner: whoever can connect to the database directly already has everything (#12 L1/L2), so
-- "no token" meaning owner costs nothing and claims nothing.
--
-- STABLE, not IMMUTABLE: it reads a table and a GUC, and the planner must not fold it across a
-- `SET textdb.token` inside one statement batch.
CREATE FUNCTION kb.current_account() RETURNS bigint LANGUAGE sql STABLE AS $$
  SELECT t.account_id
    FROM kb.token t JOIN kb.account a ON a.id = t.account_id
   WHERE t.hash = encode(sha256(convert_to(nullif(current_setting('textdb.token', true), ''), 'UTF8')), 'hex')
     AND t.revoked_at IS NULL
     AND (t.expires_at IS NULL OR t.expires_at > now())
     AND a.disabled_at IS NULL
$$;

-- Set the connection's token, the counterpart of SQLite's textdb_auth(). Returns the account
-- name, or raises TX005 if the bearer is not usable — one message for unknown, expired and
-- revoked alike, because telling them apart is free information for whoever is guessing.
CREATE FUNCTION kb.auth(bearer text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE who text;
BEGIN
  PERFORM set_config('textdb.token', coalesce(bearer, ''), false);
  SELECT a.name INTO who FROM kb.account a WHERE a.id = kb.current_account();
  IF who IS NULL THEN
    PERFORM set_config('textdb.token', '', false);
    PERFORM kb._raise('TX005', 'this token is not usable: it is unknown, expired or revoked', NULL);
  END IF;
  UPDATE kb.token SET last_used_at = now()
   WHERE hash = encode(sha256(convert_to(bearer, 'UTF8')), 'hex');
  RETURN who;
END $$;

-- This connection's grants, with each share root's *current* path: a grant binds to the node, so
-- the owner renaming or moving the shared folder leaves the account's paths untouched. A share
-- root in the trash comes back dormant — its alias disappears, but the grant is still there, and
-- that is what keeps `sync` from deleting a checkout.
CREATE VIEW kb.my_grant AS
  SELECT g.account_id, g.node_id, g.alias, g.rights, n.path AS store_path,
         -- `dormant` here means "you hold it and may not use it", whichever way: its folder is
         -- in the trash, or the grant was taken away. Both answer forbidden, never not-found.
         (n.deleted_at IS NOT NULL OR g.revoked_at IS NOT NULL) AS dormant,
         (g.revoked_at IS NOT NULL) AS revoked
    FROM kb.grant g JOIN kb.node n ON n.id = g.node_id
   WHERE g.account_id = kb.current_account();

-- Path translation and the visibility test, the two questions every surface asks (#12 §2.1).
--
-- **Read the `(SELECT kb.current_account())` carefully.** Written bare, `kb.current_account()` is
-- a STABLE function the planner may call once per row, and these are used in the target list and
-- the WHERE clause of every listing — so the owner, who delegates nothing, would pay a function
-- call per row for a feature they do not use. As an uncorrelated scalar subquery it becomes an
-- InitPlan, evaluated once for the whole statement, and the OR then short-circuits on a boolean
-- constant. That is the difference between "costs nothing if you don't use it" as a claim and as
-- a measured fact.

-- Can this connection see this store path at all?
CREATE FUNCTION kb.visible(p text) RETURNS boolean LANGUAGE sql STABLE AS $$
  SELECT (SELECT kb.current_account()) IS NULL
      OR EXISTS (SELECT 1 FROM kb.my_grant g
                  WHERE NOT g.dormant AND (p = g.store_path OR p LIKE kb._subtree_like(g.store_path)))
$$;

-- The same, for an operation that writes.
CREATE FUNCTION kb.writable(p text) RETURNS boolean LANGUAGE sql STABLE AS $$
  SELECT (SELECT kb.current_account()) IS NULL
      OR EXISTS (SELECT 1 FROM kb.my_grant g
                  WHERE NOT g.dormant AND g.rights = 'rw'
                    AND (p = g.store_path OR p LIKE kb._subtree_like(g.store_path)))
$$;

-- A store path as this connection sees it; NULL when it sees nothing there. A single-root
-- account's share root is its `/`; an aliased account's is `/<alias>`.
CREATE FUNCTION kb.to_view(p text) RETURNS text LANGUAGE sql STABLE AS $$
  SELECT CASE WHEN (SELECT kb.current_account()) IS NULL THEN p ELSE (
    SELECT CASE WHEN g.alias = ''
                THEN coalesce(nullif(substr(p, length(g.store_path) + 1), ''), '/')
                ELSE '/' || g.alias || substr(p, length(g.store_path) + 1) END
      FROM kb.my_grant g
     WHERE NOT g.dormant AND (p = g.store_path OR p LIKE kb._subtree_like(g.store_path))
     LIMIT 1
  ) END
$$;

-- A path as this connection wrote it, as a store path. NULL for anything it cannot address,
-- which the caller turns into TX003 or TX005 — `kb.resolve` below makes that choice once.
CREATE FUNCTION kb.to_store(p text) RETURNS text LANGUAGE sql STABLE AS $$
  SELECT CASE WHEN (SELECT kb.current_account()) IS NULL THEN p
              -- An aliased account's root is the store root as far as addressing goes: it is
              -- where its shares hang, and a listing of it is a listing of them. Nothing can be
              -- written there, which `kb.resolve(p, true)` refuses separately.
              WHEN p = '/' AND NOT EXISTS (SELECT 1 FROM kb.my_grant WHERE alias = '') THEN '/'
              ELSE (
    SELECT CASE
      -- A single-root account: its root *is* the share.
      WHEN g.alias = '' THEN CASE WHEN p = '/' THEN g.store_path ELSE g.store_path || p END
      -- An aliased account: the first segment is the alias, the rest is the real subtree.
      WHEN p = '/' || g.alias THEN g.store_path
      ELSE g.store_path || substr(p, length(g.alias) + 2)
    END
      FROM kb.my_grant g
     WHERE NOT g.dormant
       AND (g.alias = '' OR p = '/' || g.alias OR p LIKE (replace(replace(replace('/' || g.alias, '\', '\\'), '%', '\%'), '_', '\_') || '/%'))
     LIMIT 1
  ) END
$$;

-- Resolve a caller's path, raising the right refusal when it does not resolve.
--
-- The whole point of TX005 lives here: a path under an alias the account *has* — one whose share
-- root is in the trash — is forbidden, while a path under no alias of theirs is simply not found.
-- `sync` deletes what is absent from the store and leaves alone what it is merely forbidden, so
-- conflating the two is what would empty a checkout when a share is revoked.
CREATE FUNCTION kb.resolve(p text, need_write boolean DEFAULT false) RETURNS text LANGUAGE plpgsql STABLE AS $$
DECLARE sp text; a text;
BEGIN
  IF kb.current_account() IS NULL THEN RETURN p; END IF;
  sp := kb.to_store(p);
  IF sp IS NOT NULL THEN
    IF need_write AND NOT kb.writable(sp) THEN
      PERFORM kb._raise('TX005', 'you have read-only access to ' || p, '');
    END IF;
    RETURN sp;
  END IF;
  -- Not addressable. Is it under a dormant share of this account's, or under nothing at all?
  SELECT g.alias INTO a FROM kb.my_grant g
   WHERE g.dormant AND (g.alias = '' OR p = '/' || g.alias OR p LIKE ('/' || g.alias || '/%')) LIMIT 1;
  IF FOUND THEN
    PERFORM kb._raise('TX005', p || ': its folder is in the trash', '');
  END IF;
  PERFORM kb._raise('TX003', 'not found: ' || p, '');
  RETURN NULL;
END $$;

-- Custom SQLSTATEs (spec §7.2): TX001 conflict, TX002 contention, TX003 not found, TX004 invalid edit,
-- TX005 forbidden (#12): it is in your view and you may not do this, as distinct from not being there.
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
  SELECT CASE lower(btrim(v, E' \t\r\n')) WHEN 'on' THEN true WHEN 'true' THEN true WHEN 'yes' THEN true WHEN '1' THEN true
                             WHEN 'off' THEN false WHEN 'false' THEN false WHEN 'no' THEN false WHEN '0' THEN false END
$$;

-- Store settings. kb.setting(key) is NULL at the default; kb.set_setting(key, NULL) returns a
-- setting to its default. path_history is on or off; link_updates (what a move does to links
-- that pointed at what moved) is off, report or rewrite; asset_sync (what `textdb sync` does
-- with assets) is off, push, pull or both; asset_pull (which assets it pulls) is linked or all.
CREATE FUNCTION kb.setting(k text) RETURNS text LANGUAGE plpgsql STABLE AS $$
BEGIN
  IF k IS NULL OR k NOT IN ('path_history', 'link_updates', 'asset_sync', 'asset_pull') THEN
    PERFORM kb._raise('TX004', format('unknown setting ''%s'' (known: path_history, link_updates, asset_sync, asset_pull)', k), NULL);
  END IF;
  RETURN (SELECT s.value FROM kb.setting s WHERE s.key = k);
END $$;
CREATE FUNCTION kb.set_setting(k text, v text) RETURNS text LANGUAGE plpgsql VOLATILE AS $$
DECLARE norm text;
BEGIN
  IF k IS NULL OR k NOT IN ('path_history', 'link_updates', 'asset_sync', 'asset_pull') THEN
    PERFORM kb._raise('TX004', format('unknown setting ''%s'' (known: path_history, link_updates, asset_sync, asset_pull)', k), NULL);
  END IF;
  IF v IS NULL THEN
    DELETE FROM kb.setting s WHERE s.key = k;
    RETURN NULL;
  END IF;
  IF k = 'link_updates' THEN
    norm := lower(btrim(v, E' \t\r\n'));
    IF norm NOT IN ('off', 'report', 'rewrite') THEN
      PERFORM kb._raise('TX004', format('%s is off, report or rewrite, not ''%s''', k, v), NULL);
    END IF;
  ELSIF k = 'asset_sync' THEN
    -- What sync does with assets: off (lists them), push, pull or both.
    norm := lower(btrim(v, E' \t\r\n'));
    IF norm NOT IN ('off', 'push', 'pull', 'both') THEN
      PERFORM kb._raise('TX004', format('%s is off, push, pull, both, not ''%s''', k, v), NULL);
    END IF;
  ELSIF k = 'asset_pull' THEN
    -- Which assets sync pulls: linked (the ones notes link to) or all.
    norm := lower(btrim(v, E' \t\r\n'));
    IF norm NOT IN ('linked', 'all') THEN
      PERFORM kb._raise('TX004', format('%s is linked, all, not ''%s''', k, v), NULL);
    END IF;
  ELSE
    IF kb._switch(v) IS NULL THEN
      PERFORM kb._raise('TX004', format('%s is on or off, not ''%s''', k, v), NULL);
    END IF;
    norm := CASE WHEN kb._switch(v) THEN 'on' ELSE 'off' END;
  END IF;
  INSERT INTO kb.setting AS s (key, value) VALUES (k, norm)
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

CREATE VIEW kb.file AS
  -- The minimal listing tier plus what only a file has: its content and parsed front matter.
  -- `parent_path` is now `dir`, the one name for it across every surface.
  SELECT n.path, n.name, 'file'::text AS kind, n.version,
         coalesce(n.nbytes, 0) AS nbytes, coalesce(n.nlines, 0) AS nlines,
         n.updated_at, n.updated_by,
         n.id,
         CASE WHEN strpos(reverse(n.path), '/') = length(n.path) THEN '/'
              ELSE left(n.path, length(n.path) - strpos(reverse(n.path), '/')) END AS dir,
         kb._materialize(n.root) AS content,
         (SELECT fm.data FROM kb.frontmatter fm WHERE fm.file_id = n.id AND fm.version = n.version) AS frontmatter,
         NULL::bigint AS base_version
  FROM kb.node n
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
  -- The canonical listing record: the same twenty-four columns in the same order as
  -- textdb_ls and textdb_entry on SQLite, so one query text reads on either engine.
  -- `v.p` is the path in the *caller's* namespace: the store's own for the owner, and for an
  -- account the one under its alias. Computed once per row in the LATERAL below and used for
  -- `path`, `name`, `dir` and `depth`, so a listing never quotes a store path at an account and
  -- an account's own path is what comes back everywhere (#12 §2.1).
  SELECT v.p AS path,
         CASE WHEN v.p = '/' THEN '/' ELSE right(v.p, strpos(reverse(v.p), '/') - 1) END AS name,
         CASE n.kind WHEN 1 THEN 'file' ELSE 'folder' END AS kind,
         CASE n.kind WHEN 1 THEN n.version END AS version,
         CASE n.kind WHEN 1 THEN coalesce(n.nbytes, 0) ELSE n.t_bytes + coalesce(d.bytes, 0) END AS nbytes,
         CASE n.kind WHEN 1 THEN coalesce(n.nlines, 0) ELSE n.t_lines + coalesce(d.lines, 0) END AS nlines,
         CASE n.kind WHEN 1 THEN n.updated_at ELSE greatest(n.updated_at, n.t_updated_at, d.ts) END AS updated_at,
         -- Whoever made the newest change below the folder: the journal's newest row, the folded
         -- total, or the folder's own row, in that order of recency.
         CASE WHEN n.kind = 1 THEN n.updated_by
              WHEN d.updated_by IS NOT NULL AND d.ts >= greatest(n.updated_at, coalesce(n.t_updated_at, n.updated_at)) THEN d.updated_by
              WHEN n.t_updated_at IS NOT NULL AND n.t_updated_at >= n.updated_at THEN n.t_updated_by
              ELSE n.updated_by END AS updated_by,
         n.id,
         -- One name for the parent, on both engines and every surface.
         CASE WHEN v.p = '/' THEN NULL
              WHEN strpos(reverse(v.p), '/') = length(v.p) THEN '/'
              ELSE left(v.p, length(v.p) - strpos(reverse(v.p), '/')) END AS dir,
         CASE WHEN v.p = '/' THEN 0 ELSE length(v.p) - length(replace(v.p, '/', '')) END::bigint AS depth,
         CASE WHEN n.kind = 1 AND strpos(reverse(n.name), '.') > 1 AND strpos(reverse(n.name), '.') < length(n.name)
              THEN lower(right(n.name, strpos(reverse(n.name), '.') - 1)) END AS ext,
         n.title,
         CASE n.kind WHEN 1 THEN coalesce(n.nwords, 0) ELSE n.t_words + coalesce(d.words, 0) END AS nwords,
         CASE n.kind WHEN 1 THEN n.nsections ELSE n.t_sections + coalesce(d.sections, 0) END AS nsections,
         CASE n.kind WHEN 1 THEN n.nprops ELSE n.t_props + coalesce(d.props, 0) END AS nprops,
         CASE n.kind WHEN 1 THEN n.nlinks ELSE n.t_links + coalesce(d.links, 0) END AS nlinks,
         CASE n.kind WHEN 1 THEN n.nlinks_broken ELSE n.t_links_broken + coalesce(d.links_broken, 0) END AS nlinks_broken,
         CASE n.kind WHEN 1 THEN n.version ELSE n.t_versions + coalesce(d.versions, 0) END AS versions,
         n.created_at,
         CASE n.kind WHEN 0 THEN n.t_files + coalesce(d.files, 0) END AS files,
         CASE n.kind WHEN 0 THEN n.t_folders + coalesce(d.folders, 0) END AS folders,
         CASE n.kind WHEN 1 THEN coalesce(n.nauthors, 0) ELSE (
           SELECT count(DISTINCT a.author) FROM kb.file_author a JOIN kb.node f ON f.id = a.file_id
            WHERE f.deleted_at IS NULL AND f.kind = 1
              AND f.path >= CASE n.path WHEN '/' THEN '/' ELSE n.path || '/' END
              AND f.path < CASE n.path WHEN '/' THEN '0' ELSE n.path || '0' END) END AS nauthors,
         CASE n.kind WHEN 1 THEN coalesce(
           (SELECT jsonb_agg(jsonb_build_object('author', nullif(a.author, ''), 'commits', a.commits, 'first_ts', a.first_ts, 'last_ts', a.last_ts)
                             ORDER BY a.commits DESC, a.last_ts DESC)
            FROM kb.file_author a WHERE a.file_id = n.id), '[]'::jsonb)
         ELSE '[]'::jsonb END AS authors,
         -- The access tier (#12). NULL for the owner, who reaches everything directly; for an
         -- account, the share this row came through and its rights.
         (SELECT g.alias FROM kb.my_grant g
           WHERE NOT g.dormant AND (n.path = g.store_path OR n.path LIKE kb._subtree_like(g.store_path)) LIMIT 1) AS share,
         (SELECT g.rights FROM kb.my_grant g
           WHERE NOT g.dormant AND (n.path = g.store_path OR n.path LIKE kb._subtree_like(g.store_path)) LIMIT 1) AS rights
  FROM kb.node n
  CROSS JOIN LATERAL (SELECT CASE WHEN (SELECT kb.current_account()) IS NULL THEN n.path ELSE kb.to_view(n.path) END AS p) v
  LEFT JOIN LATERAL (
    SELECT sum(x.files)::bigint AS files, sum(x.folders)::bigint AS folders, sum(x.bytes)::bigint AS bytes,
           sum(x.lines)::bigint AS lines, sum(x.words)::bigint AS words, sum(x.versions)::bigint AS versions,
           sum(x.sections)::bigint AS sections, sum(x.props)::bigint AS props, sum(x.links)::bigint AS links,
           sum(x.links_broken)::bigint AS links_broken, max(x.ts) AS ts,
           (array_agg(x.updated_by ORDER BY x.ts DESC) FILTER (WHERE x.updated_by IS NOT NULL))[1] AS updated_by
    FROM kb.folder_delta x WHERE x.folder_id = n.id
  ) d ON n.kind = 0
  -- Every listing surface is built on this view, so filtering here is what makes "the extension
  -- enforces it, not the caller" true: `kb.ls`, `kb.file`, `kb.folder`, `textdb sql`'s `files`
  -- and `folders`, and the web app all inherit it. The subquery is an InitPlan for the owner,
  -- evaluated once, so a store that delegates nothing plans exactly as it did before.
  WHERE n.deleted_at IS NULL AND ((SELECT kb.current_account()) IS NULL OR kb.visible(n.path));

-- An aliased account's root: the list of its shares, not a node. `kb.entry` is built on
-- `kb.node` and so has no row for it, and `stat /` then answered "not found" on a path the
-- account uses constantly. The totals are its shares' added up, and `kind` is `root` — neither
-- file nor folder, because nothing can be written there.
CREATE VIEW kb.root_entry AS
  SELECT '/'::text AS path, '/'::text AS name, 'root'::text AS kind, NULL::bigint AS version,
         coalesce(sum(e.nbytes), 0)::bigint AS nbytes, coalesce(sum(e.nlines), 0)::bigint AS nlines,
         max(e.updated_at) AS updated_at, NULL::text AS updated_by, 0::bigint AS id,
         NULL::text AS dir, 0::bigint AS depth, NULL::text AS ext, NULL::text AS title,
         coalesce(sum(e.nwords), 0)::bigint AS nwords, coalesce(sum(e.nsections), 0)::bigint AS nsections,
         coalesce(sum(e.nprops), 0)::bigint AS nprops, coalesce(sum(e.nlinks), 0)::bigint AS nlinks,
         coalesce(sum(e.nlinks_broken), 0)::bigint AS nlinks_broken,
         coalesce(sum(e.versions), 0)::bigint AS versions, min(e.created_at) AS created_at,
         coalesce(sum(e.files), 0)::bigint AS files, coalesce(sum(e.folders) + count(*), 0)::bigint AS folders,
         0::bigint AS nauthors, '[]'::jsonb AS authors, NULL::text AS share, NULL::text AS rights
    FROM kb.entry e
   WHERE (SELECT kb.current_account()) IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM kb.my_grant WHERE alias = '')
     AND e.dir = '/'
  -- An aggregate with no GROUP BY returns one row of NULLs over an empty set, so without this
  -- the owner — for whom the WHERE matches nothing — still got a row here, and `stat /` came
  -- back with two.
  HAVING count(*) > 0;

CREATE VIEW kb.folder AS
  -- The full listing record, folders only. `n_children` and `nbytes_total` are gone: a folder
  -- row now carries `files`, `folders` and `nbytes` like every other listing surface, and the
  -- totals come off the node row rather than a subtree scan per row.
  SELECT * FROM kb.entry WHERE kind = 'folder' AND path <> '/';

-- The files and folders in the folder `path`, by name; with `recursive`, everything below it, by path.
-- The listing of a folder, in the *caller's* namespace.
--
-- `kb.entry` speaks view paths, so the folder has to be named in view paths too: comparing
-- `e.dir` against the node's own `path` worked only while the two were the same string, which
-- they stopped being the moment an account could see the store under an alias. `kb._node_id`
-- still does the existence check and the refusal, so a path outside the caller's shares is
-- refused there rather than quietly listing nothing.
CREATE FUNCTION kb.ls(path text, recursive boolean DEFAULT false) RETURNS SETOF kb.entry LANGUAGE sql STABLE AS $$
  SELECT e.* FROM (SELECT kb._node_id($1) AS id) d
  JOIN LATERAL (SELECT CASE WHEN (SELECT kb.current_account()) IS NULL THEN n.path
                            ELSE coalesce(kb.to_view(n.path), '/') END AS p
                  FROM kb.node n WHERE n.id = d.id) v ON true
  JOIN kb.entry e
    ON CASE WHEN recursive THEN e.path <> '/' AND (v.p = '/' OR e.path LIKE kb._subtree_like(v.p)) ELSE e.dir IS NOT DISTINCT FROM v.p END
  ORDER BY CASE WHEN recursive THEN e.path ELSE e.name END
$$;

-- Fold kb.folder_delta into the node rows; returns the rows folded. Safe while others write
-- (one statement: a reader sees the rows before or after, never half); run it from a
-- maintenance job when the table grows.
CREATE FUNCTION kb.compact_folder_totals() RETURNS bigint LANGUAGE sql VOLATILE AS $$
  WITH gone AS (DELETE FROM kb.folder_delta RETURNING *),
       s AS (SELECT folder_id, sum(files) AS files, sum(folders) AS folders, sum(bytes) AS bytes, sum(lines) AS lines,
                    sum(words) AS words, sum(versions) AS versions, sum(sections) AS sections, sum(props) AS props,
                    sum(links) AS links, sum(links_broken) AS links_broken, max(ts) AS ts, count(*) AS n,
                    (array_agg(updated_by ORDER BY ts DESC) FILTER (WHERE updated_by IS NOT NULL))[1] AS updated_by
             FROM gone GROUP BY folder_id),
       u AS (UPDATE kb.node f SET t_files = f.t_files + s.files, t_folders = f.t_folders + s.folders, t_bytes = f.t_bytes + s.bytes,
                                  t_lines = f.t_lines + s.lines, t_words = f.t_words + s.words, t_versions = f.t_versions + s.versions,
                                  t_sections = f.t_sections + s.sections, t_props = f.t_props + s.props,
                                  t_links = f.t_links + s.links, t_links_broken = f.t_links_broken + s.links_broken,
                                  t_updated_by = CASE WHEN s.ts >= coalesce(f.t_updated_at, s.ts)
                                                      THEN coalesce(s.updated_by, f.t_updated_by) ELSE f.t_updated_by END,
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
                       t_sections = coalesce(s.sections, 0), t_props = coalesce(s.props, 0),
                       t_links = coalesce(s.links, 0), t_links_broken = coalesce(s.links_broken, 0),
                       t_updated_at = s.ts
  FROM kb.node f2 LEFT JOIN LATERAL (
    SELECT count(*) FILTER (WHERE n.kind = 1) AS files, count(*) FILTER (WHERE n.kind = 0) AS folders,
           sum(n.nbytes) FILTER (WHERE n.kind = 1) AS bytes, sum(n.nlines) FILTER (WHERE n.kind = 1) AS lines,
           sum(n.nwords) FILTER (WHERE n.kind = 1) AS words, sum(n.version) FILTER (WHERE n.kind = 1) AS versions,
           sum(n.nsections) FILTER (WHERE n.kind = 1) AS sections, sum(n.nprops) FILTER (WHERE n.kind = 1) AS props,
           sum(n.nlinks) FILTER (WHERE n.kind = 1) AS links, sum(n.nlinks_broken) FILTER (WHERE n.kind = 1) AS links_broken,
           max(n.updated_at) AS ts
    FROM kb.node n
    WHERE n.deleted_at IS NULL AND n.path <> '/' AND (f2.path = '/' OR n.path LIKE kb._subtree_like(f2.path))
  ) s ON true
  WHERE f.id = f2.id AND f2.kind = 0 AND f2.deleted_at IS NULL;
END $$;

-- Attributed namespace changes (recorded in the change feed with `author` and `message`).
CREATE FUNCTION kb.move(from_path text, to_path text, author text DEFAULT NULL, message text DEFAULT NULL) RETURNS void LANGUAGE sql VOLATILE AS $$ SELECT kb._move(from_path, to_path, author, message) $$;
CREATE FUNCTION kb.remove(path text, author text DEFAULT NULL, message text DEFAULT NULL) RETURNS void LANGUAGE sql VOLATILE AS $$ SELECT kb._remove(path, author, message) $$;
-- A move that returns the links that pointed at what moved and no longer reach it:
-- {"links": [{path, line, kind, target, now_at, version}]}. `links` is off, report or rewrite
-- (rewrite commits each linking file, version set), or NULL for the store's link_updates setting.
CREATE FUNCTION kb.move_links(from_path text, to_path text, author text DEFAULT NULL, message text DEFAULT NULL, links text DEFAULT NULL) RETURNS jsonb LANGUAGE sql VOLATILE AS $$ SELECT kb._check_j(kb._move_links_j(from_path, to_path, author, message, links)) $$;

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
CREATE FUNCTION kb.replace_lines(path text, l_from bigint, l_to bigint, body text, base_version bigint DEFAULT NULL, author text DEFAULT NULL, message text DEFAULT NULL) RETURNS jsonb LANGUAGE sql VOLATILE AS $$ SELECT kb._check_j(kb._replace_lines_j(path, l_from, l_to, body, base_version, author, message)) $$;
-- Several line ranges [{"from": 3, "to": 4, "text": "…"}, …], all numbered as in base_version, in one commit.
CREATE FUNCTION kb.replace_ranges(path text, ranges jsonb, base_version bigint DEFAULT NULL, author text DEFAULT NULL, message text DEFAULT NULL) RETURNS jsonb LANGUAGE sql VOLATILE AS $$ SELECT kb._check_j(kb._replace_ranges_j(path, ranges, base_version, author, message)) $$;
CREATE FUNCTION kb._update_content(path text, content text, base_version bigint, author text, message text) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb._check(kb._update_content_j(path, content, base_version, author, message)) $$;
CREATE FUNCTION kb.edit(path text, old text, new text, author text, message text DEFAULT NULL) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb._check(kb._edit_j(path, old, new, author, message)) $$;
CREATE FUNCTION kb.append(path text, tail text, author text, message text DEFAULT NULL) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb._check(kb._append_j(path, tail, author, message)) $$;
-- Every occurrence of `old` becomes `new`, as one version. With expected_count the file must hold
-- exactly that many occurrences, else at least one; otherwise TX004 and nothing changes.
CREATE FUNCTION kb.replace(path text, old text, new text, expected_count bigint DEFAULT NULL, author text DEFAULT NULL, message text DEFAULT NULL) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb._check(kb._replace_many_j(path, jsonb_build_array(jsonb_build_array(old, new, expected_count)), author, message)) $$;
-- Replacements applied in order, one version: [[old, new], [old, new, expected_count], {"old", "new", "count"}, …].
CREATE FUNCTION kb.replace_many(path text, replacements jsonb, author text DEFAULT NULL, message text DEFAULT NULL) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT kb._check(kb._replace_many_j(path, replacements, author, message)) $$;

-- Batches: commits, moves and deletes made while the session setting textdb.batch names a batch
-- (SELECT set_config('textdb.batch', 'id', true) for the rest of a transaction) are recorded
-- under it, so they can be listed and reverted together.
CREATE FUNCTION kb.batch() RETURNS text LANGUAGE sql STABLE AS $$ SELECT nullif(current_setting('textdb.batch', true), '') $$;
-- What changed after change number `seq`, or under `batch`: [{op, path, old_path, from_version, to_version, diff}],
-- a file's commits folded into one item with a unified diff.
CREATE FUNCTION kb.changes_after(seq bigint) RETURNS jsonb LANGUAGE sql STABLE AS $$ SELECT kb._check_j(kb._changes_j(seq, NULL)) $$;
CREATE FUNCTION kb.batch_changes(batch text) RETURNS jsonb LANGUAGE sql STABLE AS $$ SELECT kb._check_j(kb._changes_j(NULL, batch)) $$;
-- Undo a batch: {restored: [{path, version}], removed, moved_back: [{from, to}], recreated, skipped}. Anything
-- changed since is skipped; unless skip_changed, that fails the whole revert (TX004).
CREATE FUNCTION kb.revert_batch(batch text, author text DEFAULT NULL, skip_changed boolean DEFAULT false) RETURNS jsonb LANGUAGE sql VOLATILE AS $$ SELECT kb._check_j(kb._revert_batch_j(batch, author, skip_changed)) $$;

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
    use textdb_core::tree::{leaves, locate_line, materialize_range, totals};
    use textdb_core::path::ancestors;
    use textdb_core::{
        unified_diff, word_delta, words_at, ChunkParams, Edit, Hash, LineHunk, PathOp, Storage, StructureExtractor, TextdbError,
    };
    use textdb_md::MarkdownExtractor;

    use crate::store::{materialize_all, normalize_path, parent_of, name_of, raise, spi_err, to_hash, NodeRow, SpiStorage};

    const P: ChunkParams = ChunkParams::DEFAULT;
    const RETRIES: usize = textdb_core::DEFAULT_RETRIES;

    /// Raise `e` with its textdb SQLSTATE.
    ///
    /// Known gap, and not specific to any one caller: from a **set-returning** function the
    /// custom code is lost and Postgres reports `XX000` with the message intact. `kb.content`
    /// raises `TX003` correctly; `kb.leaf_hashes` and `kb.prop_find`, which return
    /// `TableIterator`, report the same failure as `XX000`. Clients that switch on the code
    /// — the CLI's exit status, the SDKs' error classes — therefore see a generic error from
    /// those functions where SQLite gives them a specific one. The message is unaffected, so
    /// nothing is silently wrong, only less precise. Fixing it means raising without going
    /// through SPI, which is a change to the extension's whole error path rather than to one
    /// function, so it is recorded here rather than bolted onto a caller.
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

    pub(crate) fn file_by_path_r(path: &str) -> Result<NodeRow, TextdbError> {
        match node_by_path(path) {
            Some(n) if n.kind == 1 => Ok(n),
            Some(_) => Err(TextdbError::InvalidEdit(format!("{} is a folder", path))),
            None => Err(TextdbError::NotFound(path.to_string())),
        }
    }

    pub(crate) fn root_of_version_r(file_id: i64, version: u64) -> Result<Hash, TextdbError> {
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
        let bytes = ok(materialize_all(&st, &h));
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub(crate) fn node_by_path(path: &str) -> Option<NodeRow> {
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
        let path = resolve(&path, true);
        ok(ensure_folder(&path))
    }

    /// `mkdir -p` of a normalized path; each folder it creates gets a `mkdir` feed row.
    pub(crate) fn ensure_folder(path: &str) -> Result<i64, TextdbError> {
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
            None,
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
        /// Structure counts, maintained like the word count: computed at commit from what the
        /// extractor returned, stored on the node row, rolled up here. `links_broken` is the
        /// one that also moves without a commit, when a link's target is deleted.
        sections: i64,
        props: i64,
        links: i64,
        links_broken: i64,
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
                sections: -self.sections,
                props: -self.props,
                links: -self.links,
                links_broken: -self.links_broken,
            }
        }
    }

    /// Record `t` against every live folder above `path`, as kb.folder_delta rows.
    ///
    /// `by` rides along with the timestamp, so a folder row can name who made the newest change
    /// below it without a query over the subtree. `None` where there is no author to name — a
    /// folder created, a move, a delete, a link status re-resolved.
    fn add_to_ancestors(path: &str, t: &Totals, by: Option<&str>) -> Result<(), TextdbError> {
        let list = serde_json::to_string(&ancestors(path)).expect("paths serialize");
        Spi::run_with_args(
            "INSERT INTO kb.folder_delta(folder_id, files, folders, bytes, lines, words, versions, sections, props, links, links_broken, updated_by) \
             SELECT n.id, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12 FROM kb.node n \
             WHERE n.path IN (SELECT jsonb_array_elements_text($1::jsonb)) AND n.deleted_at IS NULL",
            &[
                list.as_str().into(),
                t.files.into(),
                t.folders.into(),
                t.bytes.into(),
                t.lines.into(),
                t.words.into(),
                t.versions.into(),
                t.sections.into(),
                t.props.into(),
                t.links.into(),
                t.links_broken.into(),
                by.into(),
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
                     coalesce(nbytes, 0)::bigint, coalesce(nlines, 0)::bigint, coalesce(nwords, 0)::bigint, versions::bigint, \
                     nsections::bigint, nprops::bigint, nlinks::bigint, nlinks_broken::bigint \
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
                    sections: get(7)?,
                    props: get(8)?,
                    links: get(9)?,
                    links_broken: get(10)?,
                });
            }
            Err(TextdbError::NotFound(format!("node {}", id)))
        })
    }

    /// Id of the live node at `path`, for `kb.ls`; TX003 when there is none.
    ///
    /// The one place `kb.ls` turns a caller's path into a node, so it is also where a caller's
    /// path becomes a store path. `kb.resolve` decides between "not there" and "you may not",
    /// and is a no-op for the owner.
    #[pg_extern(stable)]
    fn _node_id(path: &str) -> i64 {
        let path = ok(normalize_path(path));
        let path = resolve(&path, false);
        match node_by_path(&path) {
            Some(n) => n.id,
            None => fail(TextdbError::NotFound(path)),
        }
    }

    /// A caller's path as a store path, raising TX003 or TX005 as `kb.resolve` decides. Returns
    /// the path unchanged for the owner without touching the database.
    fn resolve(path: &str, need_write: bool) -> String {
        if account_id_now().is_none() {
            return path.to_string();
        }
        match Spi::get_one_with_args::<String>("SELECT kb.resolve($1, $2)", &[path.into(), need_write.into()]) {
            Ok(Some(p)) => p,
            // `kb.resolve` raises rather than returning NULL, so this is the shape of a bug
            // rather than a refusal; reported as not-found is the safe reading.
            _ => fail(TextdbError::NotFound(path.to_string())),
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
            "WITH c AS (INSERT INTO kb.change(op, node_id, node_kind, path, old_path, version, base_version, commit_kind, author, message, batch) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, kb.batch()) RETURNING seq) SELECT pg_notify('textdb_change', seq::text) FROM c",
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
            "INSERT INTO kb.commit(file_id, version, root, parent_root, author, message, nbytes, nlines, nwords, kind, base_version, batch) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $11, $9, $10, kb.batch())",
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
                nwords.into(),
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
            // The structure counts move in `write_structure_counts`, which knows what the
            // extractor found; it rolls its own delta up so this one stays about content.
            ..Totals::default()
        };
        ok(add_to_ancestors(path, &change, author));
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
            let bytes = ok(materialize_all(&st, &c.root));
            let s = MarkdownExtractor.extract(&bytes);
            ok(write_structure(file_id, c.version as i64, &s));
            ok(write_structure_counts(file_id, path, &s));
        }
        if c.version == 1 {
            // Links elsewhere may have been waiting for a file of this name.
            ok(crate::links::relink(&[textdb_md::resolve::name_key(path)], &[]));
        }
    }

    /// Store what the extractor found on the node row and roll the change into the folders.
    ///
    /// The same pattern the word count uses, so a listing reads "12 headings, 4 properties,
    /// 2 links" off the row rather than running three queries per file.
    fn write_structure_counts(file_id: i64, path: &str, s: &textdb_core::structure::Structure) -> Result<(), TextdbError> {
        let title = document_title(s);
        let sections = s.sections.len() as i64;
        // Top-level front matter keys: `project.name` and `project.phase` are one property to
        // a reader, the way `meta get project` shows it.
        let props = s.frontmatter.as_ref().and_then(|v| v.as_object()).map_or(0, |o| o.len()) as i64;
        let links = s.links.len() as i64;
        let broken = broken_links_of(file_id)?;
        let old = Spi::connect(|client| {
            let rows = client
                .select(
                    "SELECT nsections, nprops, nlinks, nlinks_broken FROM kb.node WHERE id = $1",
                    None,
                    &[file_id.into()],
                )
                .map_err(storage_err)?;
            let mut out = (0i64, 0i64, 0i64, 0i64);
            for r in rows {
                let g = |i: usize| r.get::<i64>(i).unwrap_or(None).unwrap_or(0);
                out = (g(1), g(2), g(3), g(4));
            }
            Ok::<_, TextdbError>(out)
        })?;
        Spi::run_with_args(
            "UPDATE kb.node SET title = $1, nsections = $2, nprops = $3, nlinks = $4, nlinks_broken = $5 WHERE id = $6",
            &[title.as_deref().into(), sections.into(), props.into(), links.into(), broken.into(), file_id.into()],
        )
        .map_err(storage_err)?;
        let change = Totals {
            sections: sections - old.0,
            props: props - old.1,
            links: links - old.2,
            links_broken: broken - old.3,
            ..Totals::default()
        };
        add_to_ancestors(path, &change, None)
    }

    /// Recount `file_id`'s broken links, store the number and move the folders above it.
    ///
    /// Called after a relink rather than at commit, because a link breaks when its *target*
    /// is deleted — nothing about the file holding it has changed.
    pub(crate) fn refresh_broken_links(file_id: i64, path: &str) -> Result<(), TextdbError> {
        let now = broken_links_of(file_id)?;
        let before = Spi::get_one_with_args::<i64>("SELECT nlinks_broken FROM kb.node WHERE id = $1", &[file_id.into()])
            .map_err(storage_err)?
            .unwrap_or(0);
        if now == before {
            return Ok(());
        }
        Spi::run_with_args(
            "UPDATE kb.node SET nlinks_broken = $1 WHERE id = $2",
            &[now.into(), file_id.into()],
        )
        .map_err(storage_err)?;
        add_to_ancestors(
            path,
            &Totals {
                links_broken: now - before,
                ..Totals::default()
            },
            None,
        )
    }

    /// Links of `file_id` whose status means the link does not reach a document.
    fn broken_links_of(file_id: i64) -> Result<i64, TextdbError> {
        Ok(Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM kb.link WHERE file_id = $1 AND status IN ('broken', 'anchor-missing', 'ambiguous')",
            &[file_id.into()],
        )
        .map_err(storage_err)?
        .unwrap_or(0))
    }

    /// A document's title: its front matter `title`, else its first level-1 heading, else none.
    ///
    /// Front matter wins because it is the one a writer set deliberately; the heading is what
    /// a reader sees when they did not.
    fn document_title(s: &textdb_core::structure::Structure) -> Option<String> {
        s.frontmatter
            .as_ref()
            .and_then(|v| v.get("title"))
            .and_then(|t| match t {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Null => None,
                other => Some(other.to_string()),
            })
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .or_else(|| s.sections.iter().find(|x| x.level == 1).map(|x| x.heading.clone()))
    }

    /// Replace a file's HEAD-only structure rows (ADR 0007) and resolve its links. When the
    /// headings, links and front matter are what the rows already hold — most edits — only the
    /// rows' version moves: no rows are rewritten and no link is resolved again.
    fn write_structure(file_id: i64, version: i64, s: &textdb_core::structure::Structure) -> Result<(), TextdbError> {
        let diff = structure_diff(file_id, s)?;
        if diff != StructureDiff::Different {
            // Word counts move on nearly every body edit while the heading tree stays put, so
            // they are refreshed on their own rather than forcing the rewrite below. One
            // statement for the document, keyed on `line_from`: a heading owns its line.
            if diff == StructureDiff::CountsOnly {
                let lines: Vec<i64> = s.sections.iter().map(|x| x.line_from as i64).collect();
                let own: Vec<i64> = s.sections.iter().map(|x| x.nwords as i64).collect();
                let tot: Vec<i64> = s.sections.iter().map(|x| x.nwords_total as i64).collect();
                Spi::run_with_args(
                    "UPDATE kb.section t SET nwords = v.own, nwords_total = v.tot \
                       FROM unnest($2::bigint[], $3::bigint[], $4::bigint[]) AS v(ln, own, tot) \
                      WHERE t.file_id = $1 AND t.line_from = v.ln",
                    &[file_id.into(), lines.into(), own.into(), tot.into()],
                )
                .map_err(storage_err)?;
            }
            for t in ["kb.section", "kb.link", "kb.frontmatter", "kb.property"] {
                Spi::run_with_args(&format!("UPDATE {t} SET version = $1 WHERE file_id = $2 AND version <> $1"), &[version.into(), file_id.into()])
                    .map_err(storage_err)?;
            }
            return Ok(());
        }
        for t in ["kb.section", "kb.frontmatter"] {
            Spi::run_with_args(&format!("DELETE FROM {} WHERE file_id = $1", t), &[file_id.into()]).map_err(storage_err)?;
        }
        // Unnested arrays rather than a statement per heading: a 1 MiB document with 339 of
        // them would otherwise pay 339 round trips through SPI on every structural change.
        if !s.sections.is_empty() {
            let paths: Vec<&str> = s.sections.iter().map(|x| x.heading_path.as_str()).collect();
            let heads: Vec<&str> = s.sections.iter().map(|x| x.heading.as_str()).collect();
            let heads_lc: Vec<String> = s.sections.iter().map(|x| x.heading.to_lowercase()).collect();
            let levels: Vec<i32> = s.sections.iter().map(|x| x.level as i32).collect();
            let from: Vec<i64> = s.sections.iter().map(|x| x.line_from as i64).collect();
            let to: Vec<i64> = s.sections.iter().map(|x| x.line_to as i64).collect();
            let own: Vec<i64> = s.sections.iter().map(|x| x.nwords as i64).collect();
            let tot: Vec<i64> = s.sections.iter().map(|x| x.nwords_total as i64).collect();
            Spi::run_with_args(
                "INSERT INTO kb.section(file_id, version, heading_path, level, line_from, line_to, heading, heading_lc, nwords, nwords_total) \
                 SELECT $1, $2, * FROM unnest($3::text[], $4::int[], $5::bigint[], $6::bigint[], $7::text[], $8::text[], $9::bigint[], $10::bigint[])",
                &[
                    file_id.into(),
                    version.into(),
                    paths.into(),
                    levels.into(),
                    from.into(),
                    to.into(),
                    heads.into(),
                    heads_lc.into(),
                    own.into(),
                    tot.into(),
                ],
            )
            .map_err(storage_err)?;
        }
        crate::links::write_rows(file_id, version, &s.links)?;
        if let Some(fm) = &s.frontmatter {
            Spi::run_with_args(
                "INSERT INTO kb.frontmatter(file_id, version, data) VALUES ($1, $2, $3::jsonb) ON CONFLICT (file_id, version) DO UPDATE SET data = EXCLUDED.data",
                &[file_id.into(), version.into(), fm.to_string().as_str().into()],
            )
            .map_err(storage_err)?;
        }
        // The indexed form of the same front matter. Always called, including with `None`, so
        // a document that loses its front matter loses its property rows with it.
        crate::property::write_rows(file_id, version, s.frontmatter.as_ref())?;
        // This file's links, and links to its headings, which may have changed.
        crate::links::relink_file(file_id)
    }

    /// What a commit changed about a document's structure rows.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum StructureDiff {
        /// Nothing; only the `version` column has to move forward.
        Same,
        /// The heading tree is intact but word counts under it moved — the common case for a
        /// body edit, and much cheaper to serve than a rewrite.
        CountsOnly,
        /// Headings, links or front matter moved; the rows are rebuilt.
        Different,
    }

    /// How the stored rows for `file_id` differ from `s`, version aside.
    ///
    /// Three outcomes rather than two because the two kinds of change cost very differently:
    /// a heading tree that has not moved needs no delete, no re-insert and no relink, even
    /// when every word count under it has changed.
    fn structure_diff(file_id: i64, s: &textdb_core::structure::Structure) -> Result<StructureDiff, TextdbError> {
        let (same_sections, counts_differ) = Spi::connect(|client| {
            let rows = client
                .select(
                    "SELECT heading_path, level, line_from, line_to, nwords, nwords_total FROM kb.section WHERE file_id = $1 ORDER BY line_from, level",
                    None,
                    &[file_id.into()],
                )
                .map_err(storage_err)?;
            if rows.len() != s.sections.len() {
                return Ok::<_, TextdbError>((false, false));
            }
            let mut counts = false;
            for (r, sec) in rows.zip(&s.sections) {
                let stored = (
                    r.get::<String>(1).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<i32>(2).map_err(storage_err)?.unwrap_or(0) as i64,
                    r.get::<i64>(3).map_err(storage_err)?.unwrap_or(0),
                    r.get::<i64>(4).map_err(storage_err)?.unwrap_or(0),
                );
                if stored != (sec.heading_path.clone(), sec.level as i64, sec.line_from as i64, sec.line_to as i64) {
                    return Ok((false, false));
                }
                let words = (r.get::<i64>(5).map_err(storage_err)?, r.get::<i64>(6).map_err(storage_err)?);
                if words != (Some(sec.nwords as i64), Some(sec.nwords_total as i64)) {
                    counts = true;
                }
            }
            Ok((true, counts))
        })?;
        if !same_sections {
            return Ok(StructureDiff::Different);
        }
        let same_links = Spi::connect(|client| {
            let rows = client
                .select(
                    "SELECT target_path, line, coalesce(kind, ''), anchor, alias, external FROM kb.link WHERE file_id = $1 ORDER BY id",
                    None,
                    &[file_id.into()],
                )
                .map_err(storage_err)?;
            if rows.len() != s.links.len() {
                return Ok::<_, TextdbError>(false);
            }
            for (r, l) in rows.zip(&s.links) {
                let stored = textdb_core::Link {
                    // Not read back: this comparison asks whether the structure changed, and a
                    // span moves whenever anything before it does.
                    span: l.span,
                    target_path: r.get::<String>(1).map_err(storage_err)?.unwrap_or_default(),
                    line: r.get::<i64>(2).map_err(storage_err)?.unwrap_or(0) as u64,
                    kind: r.get::<String>(3).map_err(storage_err)?.unwrap_or_default(),
                    anchor: r.get::<String>(4).map_err(storage_err)?,
                    alias: r.get::<String>(5).map_err(storage_err)?,
                    external: r.get::<bool>(6).map_err(storage_err)?.unwrap_or(false),
                };
                if &stored != l {
                    return Ok(false);
                }
            }
            Ok(true)
        })?;
        if !same_links {
            return Ok(StructureDiff::Different);
        }
        // Front matter compared as jsonb, which ignores key order and spacing.
        let want = s.frontmatter.as_ref().map(|fm| fm.to_string());
        let stored = Spi::get_one_with_args::<bool>(
            "SELECT (SELECT CASE WHEN $2::text IS NULL THEN false ELSE data = $2::jsonb END FROM kb.frontmatter WHERE file_id = $1 LIMIT 1)",
            &[file_id.into(), want.as_deref().into()],
        )
        .map_err(storage_err)?;
        let same_fm = match (stored, want) {
            (None, None) => true,
            (Some(same), Some(_)) => same,
            _ => false,
        };
        if !same_fm {
            return Ok(StructureDiff::Different);
        }
        Ok(if counts_differ { StructureDiff::CountsOnly } else { StructureDiff::Same })
    }

    /// Create a file (parents created), commit version 1.
    #[pg_extern(volatile)]
    fn _create(path: &str, content: &str, author: Option<&str>, message: Option<&str>) -> i64 {
        ok(create_impl(path, content, author, message))
    }

    pub(crate) fn create_impl(path: &str, content: &str, author: Option<&str>, message: Option<&str>) -> Result<i64, TextdbError> {
        let path = normalize_path(path)?;
        let path = resolve(&path, true);
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
        let path = resolve(&path, true);
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
    fn _replace_lines_j(
        path: &str,
        l_from: i64,
        l_to: i64,
        body: &str,
        base_version: Option<i64>,
        author: Option<&str>,
        message: default!(Option<&str>, "NULL"),
    ) -> pgrx::JsonB {
        let ranges = [(l_from.max(0) as u64, l_to.max(0) as u64, body.as_bytes().to_vec())];
        json_result(replace_ranges_impl(path, &ranges, base_version, author, message))
    }

    /// Several line ranges `[{"from", "to", "text"}]` in one commit (`kb.replace_ranges` in SQL).
    #[pg_extern(volatile)]
    fn _replace_ranges_j(path: &str, ranges: pgrx::JsonB, base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> pgrx::JsonB {
        let usage = || TextdbError::InvalidEdit("ranges must be a JSON array of {\"from\": n, \"to\": n, \"text\": \"…\"}".into());
        let parsed: Result<Vec<(u64, u64, Vec<u8>)>, TextdbError> = match ranges.0.as_array() {
            None => Err(usage()),
            Some(items) => items
                .iter()
                .map(|r| {
                    let from = r.get("from").and_then(serde_json::Value::as_i64).ok_or_else(usage)?;
                    let to = r.get("to").and_then(serde_json::Value::as_i64).ok_or_else(usage)?;
                    let text = r.get("text").and_then(serde_json::Value::as_str).ok_or_else(usage)?;
                    Ok((from.max(0) as u64, to.max(0) as u64, text.as_bytes().to_vec()))
                })
                .collect(),
        };
        json_result(parsed.and_then(|ranges| replace_ranges_impl(path, &ranges, base_version, author, message)))
    }

    /// Replace line ranges `(from, to, text)` (1-based, inclusive), all numbered as in
    /// `base_version` (HEAD when NULL), in one commit. `to = from - 1` inserts in front of `from`;
    /// `from` one past the last line appends. They may come in any order but must not overlap or
    /// start at the same line. Committed with that base version, so commits that landed elsewhere
    /// in the meantime are rebased over (or TX001 on the same lines).
    pub(crate) fn replace_ranges_impl(
        path: &str,
        ranges: &[(u64, u64, Vec<u8>)],
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<(i64, CommitKind), TextdbError> {
        let path = normalize_path(path)?;
        let path = resolve(&path, true);
        let mut sorted: Vec<&(u64, u64, Vec<u8>)> = ranges.iter().collect();
        sorted.sort_by_key(|r| r.0);
        if sorted.is_empty() {
            return Err(TextdbError::InvalidEdit("no line ranges".into()));
        }
        for &&(from, to, _) in &sorted {
            if from == 0 || to + 1 < from {
                return Err(TextdbError::InvalidEdit(format!("invalid line range {}-{}", from, to)));
            }
        }
        for w in sorted.windows(2) {
            if w[1].0 <= w[0].1 || w[1].0 == w[0].0 {
                return Err(TextdbError::InvalidEdit(format!("line ranges {}-{} and {}-{} overlap", w[0].0, w[0].1, w[1].0, w[1].1)));
            }
        }
        let n = file_by_path_r(&path)?;
        let base_v = base_version.map_or(n.version, |v| v.max(0));
        let root = match n.root {
            Some(r) if base_v == n.version => r,
            _ => root_of_version_r(n.id, base_v as u64)?,
        };
        let mut edits = Vec::with_capacity(sorted.len());
        {
            let st = SpiStorage::new();
            let (len, newlines) = totals(&st, &root)?;
            let unterminated = len > 0 && materialize_range(&st, &root, len - 1, len)? != b"\n";
            let nlines = newlines + unterminated as u64;
            for &&(from, to, ref body) in &sorted {
                if from - 1 > nlines || to > nlines {
                    return Err(TextdbError::InvalidEdit(format!(
                        "lines {}-{} are outside {}, which has {} lines at version {}",
                        from, to, path, nlines, base_v
                    )));
                }
                let mut replacement = body.clone();
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
                edits.push(Edit::new(start, end, replacement));
            }
        }
        commit_edits_impl(&path, &edits, Some(base_v), author, message.or(Some("replace-lines")))
    }

    /// Text replacements as one version (`kb.replace`, `kb.replace_many` in SQL). JSON outcome.
    #[pg_extern(volatile)]
    fn _replace_many_j(path: &str, replacements: pgrx::JsonB, author: Option<&str>, message: Option<&str>) -> pgrx::JsonB {
        json_result(crate::bulk::parse_replacements(&replacements.0).and_then(|r| replace_text_impl(path, &r, author, message)))
    }

    /// Apply `replacements` in order to the current content and commit the result as one version.
    /// A count that does not match (or none found, without an expected count) fails the whole call.
    pub(crate) fn replace_text_impl(
        path: &str,
        replacements: &[crate::bulk::Replacement],
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<(i64, CommitKind), TextdbError> {
        let path = normalize_path(path)?;
        let path = resolve(&path, true);
        if replacements.is_empty() {
            return Err(TextdbError::InvalidEdit("no replacements given".into()));
        }
        let n = file_by_path_r(&path)?;
        let root = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let text = crate::bulk::apply_replacements(&path, materialize_all(&SpiStorage::new(), &root)?, replacements)?;
        let text = String::from_utf8(text).map_err(|_| TextdbError::InvalidEdit(format!("{path}: the replaced content is not valid UTF-8")))?;
        update_content_impl(&path, &text, Some(n.version), author, message.or(Some("replace")))
    }

    /// Commit a byte-range edit set expressed against `base_version` (or HEAD).
    pub(crate) fn commit_edits_impl(path: &str, edits: &[Edit], base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<(i64, CommitKind), TextdbError> {
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

    pub(crate) fn update_content_impl(path: &str, content: &str, base_version: Option<i64>, author: Option<&str>, message: Option<&str>) -> Result<(i64, CommitKind), TextdbError> {
        let path = normalize_path(path)?;
        let path = resolve(&path, true);
        let n = file_by_path_r(&path)?;
        let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let base = match base_version {
            Some(v) if v != n.version => root_of_version_r(n.id, v as u64)?,
            _ => cur,
        };
        let mut st = SpiStorage::new();
        let old = materialize_all(&st, &base)?;
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
    fn _edit_j(path: &str, old: &str, new: &str, author: Option<&str>, message: default!(Option<&str>, "NULL")) -> pgrx::JsonB {
        json_result(edit_impl(path, old, new, author, message))
    }

    fn edit_impl(path: &str, old: &str, new: &str, author: Option<&str>, message: Option<&str>) -> Result<(i64, CommitKind), TextdbError> {
        let path = normalize_path(path)?;
        let path = resolve(&path, true);
        let n = file_by_path_r(&path)?;
        let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let mut st = SpiStorage::new();
        let content = materialize_all(&st, &cur)?;
        let pos = find_unique(&content, old.as_bytes())?;
        let edits = [Edit::new(pos as u64, (pos + old.len()) as u64, new.as_bytes().to_vec())];
        let c = commit(&mut st, &P, n.id as u64, &path, &cur, &edits, RETRIES)?;
        if c.kind != CommitKind::NoOp {
            record_commit(n.id, &path, &c, Some(&cur), Some(n.version), author, message.or(Some("edit")));
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
    fn _append_j(path: &str, tail: &str, author: Option<&str>, message: default!(Option<&str>, "NULL")) -> pgrx::JsonB {
        json_result(append_impl(path, tail, author, message))
    }

    fn append_impl(path: &str, tail: &str, author: Option<&str>, message: Option<&str>) -> Result<(i64, CommitKind), TextdbError> {
        let path = normalize_path(path)?;
        let path = resolve(&path, true);
        let n = file_by_path_r(&path)?;
        let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let mut st = SpiStorage::new();
        let c = commit_append(&mut st, &P, n.id as u64, &path, tail.as_bytes(), RETRIES)?;
        if c.kind != CommitKind::NoOp {
            record_commit(n.id, &path, &c, Some(&cur), Some(n.version), author, message.or(Some("append")));
        }
        Ok((c.version as i64, c.kind))
    }

    /// Rename/move a file or folder: subtree path rewrite in one statement, ids stable.
    #[pg_extern(volatile)]
    fn _rename(from: &str, to: &str) {
        ok(rename_impl(from, to, None, None, None, false));
    }

    /// As `_rename`, attributing the move in the change feed (`kb.move` in SQL). Links follow
    /// the store's link_updates setting, rewritten only when it says rewrite.
    #[pg_extern(volatile)]
    fn _move(from_path: &str, to_path: &str, author: Option<&str>, message: default!(Option<&str>, "NULL")) {
        ok(rename_impl(from_path, to_path, author, message, None, false));
    }

    /// As `_move`, returning the links that pointed at what moved (`kb.move_links` in SQL).
    #[pg_extern(volatile)]
    fn _move_links_j(from_path: &str, to_path: &str, author: Option<&str>, message: Option<&str>, links: Option<&str>) -> pgrx::JsonB {
        pgrx::JsonB(match rename_impl(from_path, to_path, author, message, links, true) {
            Ok(changes) => serde_json::json!({ "links": changes }),
            Err(e) => error_json(&e),
        })
    }

    /// Move `from` to `to`; then the links that pointed at what moved and no longer reach it are
    /// rewritten (mode `rewrite`) and, with `report`, returned. `links` chooses the mode, else the
    /// store's link_updates setting; a caller that does not read the result (`report = false`)
    /// skips reporting.
    pub(crate) fn rename_impl(
        from: &str,
        to: &str,
        author: Option<&str>,
        message: Option<&str>,
        links: Option<&str>,
        report: bool,
    ) -> Result<Vec<serde_json::Value>, TextdbError> {
        let from = normalize_path(from)?;
        let from = resolve(&from, true);
        let to = normalize_path(to)?;
        let to = resolve(&to, true);
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
        let before = crate::links::files_at(&from)?;
        let mode = crate::links::mode(links)?;
        let pointing = match mode {
            textdb_md::resolve::LinkUpdates::Off => Vec::new(),
            textdb_md::resolve::LinkUpdates::Report if !report => Vec::new(),
            _ => crate::links::links_into(&before)?,
        };
        let moved = subtree_totals(src.id)?;
        add_to_ancestors(&from, &moved.neg(), None)?;
        let seq = record_change("move", src.id, src.kind, &to, Some(&from), None, None, None, author, message);
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
        add_to_ancestors(&to, &moved, None)?;
        let after = crate::links::files_at(&to)?;
        let names = crate::links::names_of(&before.iter().chain(&after).cloned().collect::<Vec<_>>());
        crate::links::relink(&names, &after.iter().map(|(id, _)| *id).collect::<Vec<_>>())?;
        crate::links::follow_move(pointing, &from, &to, mode, |path, edits, msg| {
            commit_edits_impl(path, edits, None, author, Some(msg)).map(|(version, _)| version)
        })
    }

    /// The changes after change number `seq`, or under `batch` (`kb.changes_after`, `kb.batch_changes`).
    #[pg_extern(stable)]
    fn _changes_j(seq: Option<i64>, batch: Option<&str>) -> pgrx::JsonB {
        pgrx::JsonB(crate::bulk::changes(seq, batch).unwrap_or_else(|e| error_json(&e)))
    }

    /// Undo a batch (`kb.revert_batch` in SQL).
    #[pg_extern(volatile)]
    fn _revert_batch_j(batch: &str, author: Option<&str>, skip_changed: bool) -> pgrx::JsonB {
        pgrx::JsonB(crate::bulk::revert(batch, author, skip_changed).unwrap_or_else(|e| error_json(&e)))
    }

    /// An error as the JSON the PL/pgSQL wrappers raise from.
    fn error_json(e: &TextdbError) -> serde_json::Value {
        serde_json::json!({ "code": e.code(), "message": e.to_string(), "detail": "" })
    }

    /// Tombstone a file or folder subtree; content and history retained.
    #[pg_extern(volatile)]
    fn _delete(path: &str) {
        ok(delete_impl(path, None, None))
    }

    /// As `_delete`, attributing the change in the feed (`kb.remove` in SQL).
    #[pg_extern(volatile)]
    fn _remove(path: &str, author: Option<&str>, message: default!(Option<&str>, "NULL")) {
        ok(delete_impl(path, author, message))
    }

    pub(crate) fn delete_impl(path: &str, author: Option<&str>, message: Option<&str>) -> Result<(), TextdbError> {
        let path = normalize_path(path)?;
        let path = resolve(&path, true);
        if path == "/" {
            return Err(TextdbError::InvalidEdit("cannot delete the root".into()));
        }
        let n = node_by_path(&path).ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let files = crate::links::files_at(&path)?;
        let gone = subtree_totals(n.id)?;
        add_to_ancestors(&path, &gone.neg(), None)?;
        let seq = record_change("delete", n.id, n.kind, &path, None, None, None, None, author, message);
        if path_history_enabled()? {
            record_path_events(PathOp::Delete, &n, None, author, seq)?;
        }
        // One timestamp for the whole subtree, distinct from other deletes in the same
        // transaction: reverting a batch finds what one delete took by it. Read once, since
        // clock_timestamp() in the UPDATE itself may be evaluated again for every row.
        let at = Spi::get_one::<String>("SELECT clock_timestamp()::text").map_err(storage_err)?.unwrap_or_default();
        Spi::run_with_args(
            "UPDATE kb.node SET deleted_at = $2::timestamptz \
             WHERE (path = $1 OR path LIKE kb._subtree_like($1)) AND deleted_at IS NULL",
            &[path.as_str().into(), at.as_str().into()],
        )
        .map_err(storage_err)?;
        crate::links::relink(&crate::links::names_of(&files), &files.iter().map(|(id, _)| *id).collect::<Vec<_>>())?;
        Ok(())
    }

    /// Content of a file at HEAD or at `version` (works for tombstoned files).
    #[pg_extern(stable)]
    fn content(path: &str, version: Option<i64>) -> String {
        let path = ok(normalize_path(path));
        let path = resolve(&path, false);
        let st = SpiStorage::new();
        let bytes = match version {
            None => {
                let n = file_by_path(&path);
                ok(materialize_all(&st, &n.root.unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())))))
            }
            Some(v) => {
                let n = NodeRow::by_path(&path, true).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
                ok(materialize_all(&st, &root_of_version(n.id, v as u64)))
            }
        };
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Lines `[l_from, l_to]`, 1-based inclusive.
    #[pg_extern(stable)]
    fn lines(path: &str, l_from: i64, l_to: i64) -> String {
        let path = ok(normalize_path(path));
        let path = resolve(&path, false);
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
        let path = resolve(&path, false);
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
        let path = resolve(&path, false);
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
            // The same order as SQLite's `textdb_history` and the `commits` view. The two
            // engines used to return the same seven names in a different order, so `SELECT *`
            // consumed positionally silently swapped `nbytes` and `kind`.
            name!(version, i64),
            name!(author, Option<String>),
            name!(ts, pgrx::datum::TimestampWithTimeZone),
            name!(message, Option<String>),
            name!(kind, Option<String>),
            name!(base_version, Option<i64>),
            name!(nbytes, Option<i64>),
            name!(nlines, Option<i64>),
            name!(nwords, Option<i64>),
        ),
    > {
        let path = ok(normalize_path(path));
        let path = resolve(&path, false);
        let n = NodeRow::by_path(&path, true).unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let rows: Vec<_> = Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT version, author, ts, message, kind, base_version, nbytes, nlines, nwords FROM kb.commit WHERE file_id = $1 ORDER BY version",
                    None,
                    &[n.id.into()],
                )
                .unwrap_or_else(|e| spi_err(e));
            let mut v = Vec::new();
            for r in t {
                v.push((
                    // SPI columns are 1-based and in the SELECT's order:
                    // version, author, ts, message, kind, base_version, nbytes, nlines, nwords.
                    r.get::<i64>(1).unwrap_or_else(|e| spi_err(e)).unwrap_or(0),
                    r.get::<String>(2).unwrap_or_else(|e| spi_err(e)),
                    r.get::<pgrx::datum::TimestampWithTimeZone>(3).unwrap_or_else(|e| spi_err(e)).expect("ts"),
                    r.get::<String>(4).unwrap_or_else(|e| spi_err(e)),
                    r.get::<String>(5).unwrap_or_else(|e| spi_err(e)),
                    r.get::<i64>(6).unwrap_or_else(|e| spi_err(e)),
                    r.get::<i64>(7).unwrap_or_else(|e| spi_err(e)),
                    r.get::<i64>(8).unwrap_or_else(|e| spi_err(e)),
                    r.get::<i64>(9).unwrap_or_else(|e| spi_err(e)),
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
        let path = resolve(&path, false);
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
        let path = resolve(&path, false);
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
            let text = materialize_all(&st, &root)?;
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
        let path = resolve(&path, false);
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
        let prefix = resolve(&prefix, false);
        let files = NodeRow::files_under(&prefix);
        let st = SpiStorage::new();
        let mut out = Vec::new();
        for n in files {
            if let Some(root) = n.root {
                out.push((n.path, String::from_utf8_lossy(&ok(materialize_all(&st, &root))).into_owned()));
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
        let path = resolve(&path, false);
        let n = file_by_path(&path);
        let st = SpiStorage::new();
        let root = n.root.unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let d = ok(textdb_core::tree::depth(&st, &root)) as i64;
        let l = ok(leaves(&st, &root)).len() as i64;
        TableIterator::once((d, l))
    }

    /// Leaf chunk hashes of a file at HEAD, in order (benchmark hook).
    ///
    /// `tree_stats` gives the leaf *count*, which cannot answer "how many leaves did this
    /// edit change" — that needs the identities. Without it the harness could compute
    /// ME-04's `leaves_changed` for `textdb-sqlite` only, so claim 1 could never pass for
    /// this binding no matter how it behaved.
    #[pg_extern(stable)]
    fn leaf_hashes(path: &str) -> TableIterator<'static, (name!(ord, i64), name!(hash, Vec<u8>))> {
        let path = ok(normalize_path(path));
        let path = resolve(&path, false);
        let n = file_by_path(&path);
        let st = SpiStorage::new();
        let root = n.root.unwrap_or_else(|| fail(TextdbError::NotFound(path.clone())));
        let rows: Vec<(i64, Vec<u8>)> = ok(leaves(&st, &root))
            .into_iter()
            .enumerate()
            .map(|(i, l)| (i as i64, l.hash.to_vec()))
            .collect();
        TableIterator::new(rows)
    }

    /// Build the property rows for every document that has front matter and none yet.
    ///
    /// A store whose documents predate the `property` table answers metadata queries with
    /// nothing until this has run — silently, which is worse than slowly, so it is a named
    /// repair rather than something hidden in a read path. Idempotent: it skips documents
    /// that already have rows, so running it twice costs one query.
    /// Fill `heading` and `heading_lc` for section rows written before those columns existed.
    ///
    /// The last component of the stored `heading_path` is exactly what the extractor would
    /// have written, so no document has to be read. Word counts need the bytes and stay
    /// `NULL` until each document is next written. Idempotent, and the number it returns is
    /// how many rows it repaired.
    /// Refresh the planner's statistics for the store's tables.
    ///
    /// A store built in one burst — an import, a sync, a restore — gets analysed once by
    /// autovacuum while it is still nearly empty and then not again, because autovacuum's
    /// threshold is a share of the rows already counted. The stale statistics cost far more
    /// than they look like they should: with them, a heading query over 2,000 notes planned
    /// as a nested loop that rescanned `section_heading` once per document and took 69 ms;
    /// after `ANALYZE` the same query, same parameters, planned as a hash join and took
    /// 2.1 ms. Every query that joins `section`, `property` or `link` to `node` is exposed
    /// the same way.
    ///
    /// Cheap and safe to repeat, so a bulk loader should call it when it finishes rather
    /// than wait for autovacuum to notice.
    #[pg_extern]
    fn analyze_store() {
        // `ANALYZE` cannot run inside the caller's transaction block on some paths, and a
        // failure here is a missed optimisation rather than a lost write, so it is reported
        // and not raised.
        for t in ["kb.node", "kb.section", "kb.property", "kb.link", "kb.chunk", "kb.chunk_ref", "kb.commit"] {
            if let Err(e) = Spi::run(&format!("ANALYZE {t}")) {
                pgrx::warning!("kb.analyze_store: {t}: {e}");
            }
        }
    }

    #[pg_extern]
    fn rebuild_headings() -> i64 {
        ok(crate::sections::backfill())
    }

    #[pg_extern]
    fn rebuild_properties() -> i64 {
        ok(crate::property::backfill());
        Spi::get_one::<i64>("SELECT count(*) FROM kb.property")
            .unwrap_or(Some(0))
            .unwrap_or(0)
    }

    /// The half-open path range that holds everything under `path`, as `[path/, path0)`.
    ///
    /// `/` sorts just below every character a path segment can start with and `0` is its
    /// successor, so the range is exact: nothing sorts between them. The root has no bound,
    /// and takes the range that holds every path. Matches `textdb_sqlite::db::subtree_bounds`
    /// so one prefix means the same thing on either engine, and seeks `node_path` where
    /// `LIKE` would scan it.
    fn subtree_bounds(path: &str) -> (String, String) {
        if path == "/" {
            return ("/".to_string(), "0".to_string());
        }
        (format!("{}/", path), format!("{}0", path))
    }

    /// `[lo, hi)` covering everything that starts with `p`, so a prefix match reads an index
    /// range instead of `LIKE 'p%'`.
    fn prefix_range(p: &str) -> (String, String) {
        let mut hi = p.to_string();
        // The successor of "pro" is "prp", so `[pro, prp)` is exactly the keys beginning "pro".
        while let Some(c) = hi.pop() {
            if let Some(next) = char::from_u32(c as u32 + 1) {
                hi.push(next);
                return (p.to_string(), hi);
            }
        }
        // Empty prefix: the caller's `$1 = ''` branch matches everything and these go unused.
        (String::new(), String::new())
    }

    /// Front-matter property names in use, most-used first; `prefix` narrows them.
    ///
    /// What a prefix means for a query: one document, or everything under a folder.
    ///
    /// Resolved once, before the query, so the predicate is a single indexed comparison
    /// instead of `path = $1 OR (path >= $2 AND path < $3)`. That OR estimated at one row,
    /// which made the planner drive the join from `node` and probe `section` once per
    /// document: 69.8 ms against 2.9 ms for the same answer, 12,078 buffer hits against 236.
    enum Scope {
        File(i64),
        Subtree(String, String),
    }

    impl Scope {
        fn of(path: &str) -> Scope {
            // A file is addressed by id. Anything else — a folder, the root, a path that is
            // not there — takes the range that holds everything below it.
            let id = Spi::get_one_with_args::<i64>(
                "SELECT id FROM kb.node WHERE path = $1 AND deleted_at IS NULL AND kind = 1",
                &[path.into()],
            )
            .ok()
            .flatten();
            match id {
                Some(id) => Scope::File(id),
                None => {
                    let (lo, hi) = subtree_bounds(path);
                    Scope::Subtree(lo, hi)
                }
            }
        }

        /// The predicate, appending its bound values to `args`.
        fn sql(&self, args: &mut Vec<String>) -> String {
            match self {
                // An id comes from the database and is an integer, so it is written in.
                Scope::File(id) => format!("n.id = {}", id),
                Scope::Subtree(lo, hi) => {
                    let sql = format!("n.path >= ${} AND n.path < ${}", args.len() + 1, args.len() + 2);
                    args.push(lo.clone());
                    args.push(hi.clone());
                    sql
                }
            }
        }
    }

    /// Headings under `prefix`, with the file columns a caller would otherwise join for.
    ///
    /// `prefix` is a folder or a single document; `/` is the whole vault. `heading` narrows to
    /// one heading, matched folded so `Next Steps` finds `next steps`, with `mode` choosing
    /// `exact`, `prefix` or `contains` — the first two seek `section_heading`, the third is the
    /// one shape that has to scan. Rows come back in document order within a document and in
    /// path order across them, so an outline pane renders them without sorting.
    #[pg_extern(stable)]
    #[allow(clippy::type_complexity)]
    fn outline(
        prefix: default!(&str, "'/'"),
        heading: default!(Option<&str>, "NULL"),
        mode: default!(&str, "'exact'"),
        max_level: default!(Option<i64>, "NULL"),
        lim: default!(i64, 1000),
    ) -> TableIterator<
        'static,
        (
            name!(path, String),
            name!(heading, String),
            name!(heading_path, String),
            name!(level, i64),
            name!(line_from, i64),
            name!(line_to, i64),
            name!(nwords, Option<i64>),
            name!(nwords_total, Option<i64>),
            name!(nbytes, Option<i64>),
            name!(nlines, Option<i64>),
            name!(file_nwords, Option<i64>),
            name!(version, i64),
            name!(updated_at, pgrx::datum::TimestampWithTimeZone),
            name!(updated_by, Option<String>),
        ),
    > {
        let prefix = ok(normalize_path(prefix));
        let prefix = resolve(&prefix, false);
        let scope = Scope::of(&prefix);
        let folded = heading.map(|h| h.to_lowercase());
        // Every predicate is built only when it applies, and the placeholders are numbered as
        // the parts are appended. Two shapes that looked harmless cost 24x between them:
        // `path = $1 OR (path >= $2 AND path < $3)` estimated at one row, which made the
        // planner drive the join from `node` and probe `section` once per document, and
        // `$4 IS NULL OR level <= $4` is opaque to it for the same reason.
        let mut args: Vec<String> = Vec::new();
        let mut where_sql = scope.sql(&mut args);
        if let Some(h) = folded.as_deref() {
            match mode {
                "prefix" => {
                    let (a, b) = prefix_range(h);
                    where_sql.push_str(&format!(" AND s.heading_lc >= ${} AND s.heading_lc < ${}", args.len() + 1, args.len() + 2));
                    args.push(a);
                    args.push(b);
                }
                "contains" => {
                    where_sql.push_str(&format!(" AND s.heading_lc LIKE ${} ESCAPE '\\'", args.len() + 1));
                    args.push(format!("%{}%", h.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
                }
                _ => {
                    where_sql.push_str(&format!(" AND s.heading_lc = ${}", args.len() + 1));
                    args.push(h.to_string());
                }
            }
        }
        if let Some(l) = max_level {
            where_sql.push_str(&format!(" AND s.level <= ${}::int", args.len() + 1));
            args.push(l.to_string());
        }
        let sql = format!(
            "SELECT n.path, s.heading, s.heading_path, s.level::bigint, s.line_from, s.line_to,
                    s.nwords, s.nwords_total, n.nbytes, n.nlines, n.nwords, n.version,
                    n.updated_at, n.updated_by
               FROM kb.section s JOIN kb.node n ON n.id = s.file_id
              WHERE n.deleted_at IS NULL AND {where_sql}
              ORDER BY n.path, s.line_from LIMIT {lim}",
            lim = lim.max(1)
        );
        let rows = Spi::connect(|client| {
            let args: Vec<pgrx::datum::DatumWithOid> = args.iter().map(|a| a.as_str().into()).collect();
            let r = client.select(&sql, None, &args).unwrap_or_else(|e| spi_err(e));
            let mut out = Vec::new();
            for row in r {
                out.push((
                    row.get::<String>(1).unwrap_or_default().unwrap_or_default(),
                    row.get::<String>(2).unwrap_or_default().unwrap_or_default(),
                    row.get::<String>(3).unwrap_or_default().unwrap_or_default(),
                    row.get::<i64>(4).unwrap_or_default().unwrap_or(0),
                    row.get::<i64>(5).unwrap_or_default().unwrap_or(0),
                    row.get::<i64>(6).unwrap_or_default().unwrap_or(0),
                    row.get::<i64>(7).unwrap_or_default(),
                    row.get::<i64>(8).unwrap_or_default(),
                    row.get::<i64>(9).unwrap_or_default(),
                    row.get::<i64>(10).unwrap_or_default(),
                    row.get::<i64>(11).unwrap_or_default(),
                    row.get::<i64>(12).unwrap_or_default().unwrap_or(0),
                    row.get::<pgrx::datum::TimestampWithTimeZone>(13).unwrap_or_default().expect("updated_at"),
                    row.get::<String>(14).unwrap_or_default(),
                ));
            }
            out
        });
        TableIterator::new(rows)
    }

    /// The canonical link row. `#[pg_extern]` reads a signature literally, so the two
    /// functions spell the tuple out and this alias types what they share underneath.
    type LinkRow = (
        name!(path, String),
        name!(version, i64),
        name!(line, i64),
        name!(kind, String),
        name!(target, String),
        name!(anchor, Option<String>),
        name!(alias, Option<String>),
        name!(status, Option<String>),
        name!(resolved, Option<String>),
        name!(asset, bool),
    );

    /// Both directions in one query: `links` scopes on the file the link is written in,
    /// `backlinks` on the file it resolves to.
    fn link_rows(path: &str, status: &str, lim: i64, incoming: bool) -> Vec<LinkRow> {
        let path = ok(normalize_path(path));
        let path = resolve(&path, false);
        let mut args: Vec<String> = Vec::new();
        let mut where_sql = if incoming {
            // An asset is linked by its own name; the node that exists is the pointer beside it.
            let (lo, hi) = subtree_bounds(&path);
            let sql = format!(
                "l.target_path <> '' AND l.resolved_id IN (SELECT id FROM kb.node WHERE kind = 1 \
                 AND deleted_at IS NULL AND (path = $1 OR path = $1 || '.tdbasset' \
                 OR (path >= $2 AND path < $3)))"
            );
            args.push(path.clone());
            args.push(lo);
            args.push(hi);
            sql
        } else {
            Scope::of(&path).sql(&mut args)
        };
        if !status.is_empty() {
            const KNOWN: [&str; 6] = ["ok", "ambiguous", "anchor-missing", "broken", "not-in-store", "external"];
            if !KNOWN.contains(&status) {
                fail(TextdbError::InvalidEdit(format!("unknown link status: {status}")));
            }
            where_sql.push_str(&format!(" AND l.status = ${}", args.len() + 1));
            args.push(status.to_string());
        }
        let sql = format!(
            "SELECT n.path, n.version, l.line, coalesce(l.kind, ''), l.target_path, l.anchor, l.alias, l.status,
                    CASE WHEN lower(r.path) LIKE '%.tdbasset' THEN left(r.path, -9) ELSE r.path END,
                    coalesce(lower(r.path) LIKE '%.tdbasset', false)
               FROM kb.link l JOIN kb.node n ON n.id = l.file_id
               LEFT JOIN kb.node r ON r.id = l.resolved_id AND r.deleted_at IS NULL
              WHERE n.deleted_at IS NULL AND {where_sql}
              ORDER BY n.path, l.line, l.id LIMIT {lim}",
            lim = lim.max(1)
        );
        Spi::connect(|client| {
            let args: Vec<pgrx::datum::DatumWithOid> = args.iter().map(|a| a.as_str().into()).collect();
            let r = client.select(&sql, None, &args).unwrap_or_else(|e| spi_err(e));
            let mut out = Vec::new();
            for row in r {
                out.push((
                    row.get::<String>(1).unwrap_or_default().unwrap_or_default(),
                    row.get::<i64>(2).unwrap_or_default().unwrap_or(0),
                    row.get::<i64>(3).unwrap_or_default().unwrap_or(0),
                    row.get::<String>(4).unwrap_or_default().unwrap_or_default(),
                    row.get::<String>(5).unwrap_or_default().unwrap_or_default(),
                    row.get::<String>(6).unwrap_or_default(),
                    row.get::<String>(7).unwrap_or_default(),
                    row.get::<String>(8).unwrap_or_default(),
                    row.get::<String>(9).unwrap_or_default(),
                    row.get::<bool>(10).unwrap_or_default().unwrap_or(false),
                ));
            }
            out
        })
    }

    /// Links written under `path` — one document or a whole folder — or, with `backlinks`,
    /// the links that point at it.
    ///
    /// The same canonical row on both engines and in every SDK, `version` included: a line
    /// number belongs to a version, and a caller that reads a link and then edits by line
    /// needs one to pass as `base_version`. A link to an asset is reported as the asset,
    /// not as its `.tdbasset` pointer, which is what the writer wrote.
    #[pg_extern(stable)]
    fn links(
        path: default!(&str, "'/'"),
        status: default!(&str, "''"),
        lim: default!(i64, 10000),
    ) -> TableIterator<'static, (
            name!(path, String),
            name!(version, i64),
            name!(line, i64),
            name!(kind, String),
            name!(target, String),
            name!(anchor, Option<String>),
            name!(alias, Option<String>),
            name!(status, Option<String>),
            name!(resolved, Option<String>),
            name!(asset, bool),
        )> {
        TableIterator::new(link_rows(path, status, lim, false))
    }

    /// The links pointing at `path`, as `links` returns the ones leaving it.
    #[pg_extern(stable)]
    fn backlinks(
        path: default!(&str, "'/'"),
        status: default!(&str, "''"),
        lim: default!(i64, 10000),
    ) -> TableIterator<'static, (
            name!(path, String),
            name!(version, i64),
            name!(line, i64),
            name!(kind, String),
            name!(target, String),
            name!(anchor, Option<String>),
            name!(alias, Option<String>),
            name!(status, Option<String>),
            name!(resolved, Option<String>),
            name!(asset, bool),
        )> {
        TableIterator::new(link_rows(path, status, lim, true))
    }

    /// Distinct headings under `prefix`, most-used first. The autosuggest call for outlines,
    /// so it reads the folded index range rather than grouping raw heading text.
    #[pg_extern(stable)]
    fn headings(
        prefix: default!(&str, "'/'"),
        starts: default!(&str, "''"),
        lim: default!(i64, 100),
    ) -> TableIterator<'static, (name!(heading, String), name!(sections, i64), name!(docs, i64))> {
        let prefix = ok(normalize_path(prefix));
        let prefix = resolve(&prefix, false);
        let lower = starts.to_lowercase();
        // Built the same way `outline` builds its predicate, and for the same reason: an OR
        // over two indexed columns, or a clause guarded by `$n = ''`, is opaque to the
        // planner and costs a join driven from the wrong side.
        let mut args: Vec<String> = Vec::new();
        let mut where_sql = Scope::of(&prefix).sql(&mut args);
        if !lower.is_empty() {
            let (hlo, hhi) = prefix_range(&lower);
            where_sql.push_str(&format!(" AND s.heading_lc >= ${} AND s.heading_lc < ${}", args.len() + 1, args.len() + 2));
            args.push(hlo);
            args.push(hhi);
        }
        let sql = format!(
            "SELECT min(s.heading), count(*), count(DISTINCT s.file_id)
               FROM kb.section s JOIN kb.node n ON n.id = s.file_id
              WHERE n.deleted_at IS NULL AND {where_sql}
              GROUP BY s.heading_lc
              ORDER BY count(*) DESC, s.heading_lc
              LIMIT {lim}",
            lim = lim.max(1)
        );
        let rows = Spi::connect(|client| {
            let args: Vec<pgrx::datum::DatumWithOid> = args.iter().map(|a| a.as_str().into()).collect();
            let r = client.select(&sql, None, &args).unwrap_or_else(|e| spi_err(e));
            let mut out = Vec::new();
            for row in r {
                out.push((
                    row.get::<String>(1).unwrap_or_default().unwrap_or_default(),
                    row.get::<i64>(2).unwrap_or_default().unwrap_or(0),
                    row.get::<i64>(3).unwrap_or_default().unwrap_or(0),
                ));
            }
            out
        });
        TableIterator::new(rows)
    }

    /// This is the autosuggest call — it runs on every keystroke — so it reads an index range
    /// over the folded key rather than enumerating `jsonb` keys, which took 1,007 ms over
    /// 50,000 notes.
    #[pg_extern(stable)]
    fn prop_keys(
        prefix: default!(&str, "''"),
        lim: default!(i64, 200),
    ) -> TableIterator<'static, (name!(key, String), name!(docs, i64), name!(values_n, i64), name!(kind, String))> {
        let lower = prefix.to_lowercase();
        let (lo, hi) = prefix_range(&lower);
        let rows = Spi::connect(|client| {
            let r = client
                .select(
                    "SELECT r.key, count(DISTINCT r.file_id), count(DISTINCT r.val_lc),
                            CASE WHEN count(r.val_num) = 0 THEN 'text'
                                 WHEN count(r.val_num) = count(r.val_txt) THEN 'number'
                                 ELSE 'mixed' END
                       FROM kb.property r
                       JOIN kb.node n ON n.id = r.file_id AND n.deleted_at IS NULL
                      WHERE ($1 = '' OR (r.key_lc >= $2 AND r.key_lc < $3))
                      GROUP BY r.key
                      ORDER BY count(DISTINCT r.file_id) DESC, r.key
                      LIMIT $4",
                    None,
                    &[lower.as_str().into(), lo.as_str().into(), hi.as_str().into(), lim.max(1).into()],
                )
                .unwrap_or_else(|e| spi_err(e));
            let mut out = Vec::new();
            for row in r {
                out.push((
                    row.get::<String>(1).unwrap_or_default().unwrap_or_default(),
                    row.get::<i64>(2).unwrap_or_default().unwrap_or(0),
                    row.get::<i64>(3).unwrap_or_default().unwrap_or(0),
                    row.get::<String>(4).unwrap_or_default().unwrap_or_default(),
                ));
            }
            out
        });
        TableIterator::new(rows)
    }

    /// The values one property takes, most-used first; `prefix` narrows them as above.
    #[pg_extern(stable)]
    fn prop_values(
        key: &str,
        prefix: default!(&str, "''"),
        lim: default!(i64, 200),
    ) -> TableIterator<'static, (name!(value, Option<String>), name!(docs, i64))> {
        let lower = prefix.to_lowercase();
        let (lo, hi) = prefix_range(&lower);
        let k = key.to_lowercase();
        let rows = Spi::connect(|client| {
            let r = client
                .select(
                    "SELECT r.val_txt, count(DISTINCT r.file_id)
                       FROM kb.property r
                       JOIN kb.node n ON n.id = r.file_id AND n.deleted_at IS NULL
                      WHERE r.key_lc = $1
                        AND ($2 = '' OR (r.val_lc >= $3 AND r.val_lc < $4))
                      GROUP BY r.val_txt
                      ORDER BY count(DISTINCT r.file_id) DESC, r.val_txt
                      LIMIT $5",
                    None,
                    &[k.as_str().into(), lower.as_str().into(), lo.as_str().into(), hi.as_str().into(), lim.max(1).into()],
                )
                .unwrap_or_else(|e| spi_err(e));
            let mut out = Vec::new();
            for row in r {
                out.push((row.get::<String>(1).unwrap_or_default(), row.get::<i64>(2).unwrap_or_default().unwrap_or(0)));
            }
            out
        });
        TableIterator::new(rows)
    }

    /// Documents matching a property query, under `folder`.
    ///
    /// The grammar is `textdb_md::query`, the same one the SQLite binding parses, so a query
    /// written against one store means the same thing against the other.
    #[pg_extern(stable)]
    fn prop_find(
        query: default!(&str, "''"),
        folder: default!(&str, "'/'"),
        lim: default!(i64, 500),
    ) -> TableIterator<'static, (name!(path, String), name!(nbytes, i64), name!(updated_at, String), name!(frontmatter, Option<String>))> {
        let expr = match textdb_md::query::parse(query) {
            Ok(e) => e,
            Err(e) => raise("TX004", &e.to_string(), ""),
        };
        let (where_clause, args) = match crate::property::compile(&expr) {
            Ok(v) => v,
            Err(e) => raise("TX004", &e.to_string(), ""),
        };
        let n = args.len();
        let folder = ok(normalize_path(folder));
        let folder = resolve(&folder, false);
        // The same escaping `kb._subtree_like` applies, so a folder named `100%_done` selects
        // itself and not `100XXXdone`.
        let like = format!("{}/%", folder.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let sql = format!(
            // ISO-8601 UTC, not `::text` in the session's time zone: the same field came back
            // `2026-09-16T05:04:00.000Z` on SQLite and `2026-09-16 05:04:00+00` here.
            "SELECT n.path, coalesce(n.nbytes, 0), coalesce(to_char(n.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), ''),
                    (SELECT f.data::text FROM kb.frontmatter f WHERE f.file_id = n.id AND f.version = n.version)
               FROM kb.node n
              WHERE n.deleted_at IS NULL AND n.kind = 1
                AND EXISTS (SELECT 1 FROM kb.property u WHERE u.file_id = n.id)
                AND ({where_clause})
                AND (${folder_i} = '/' OR n.path = ${folder_i} OR n.path LIKE ${like_i} ESCAPE '\\')
              ORDER BY n.path
              LIMIT ${lim_i}",
            folder_i = n + 1,
            like_i = n + 2,
            lim_i = n + 3,
        );
        let rows = Spi::connect(|client| {
            let mut bound: Vec<pgrx::datum::DatumWithOid> = Vec::with_capacity(n + 3);
            for a in &args {
                match a {
                    crate::property::Arg::Text(t) => bound.push(t.as_str().into()),
                    crate::property::Arg::Num(f) => bound.push((*f).into()),
                }
            }
            bound.push(folder.as_str().into());
            bound.push(like.as_str().into());
            bound.push(lim.max(1).into());
            let r = client.select(&sql, None, &bound).unwrap_or_else(|e| spi_err(e));
            let mut out = Vec::new();
            for row in r {
                out.push((
                    row.get::<String>(1).unwrap_or_default().unwrap_or_default(),
                    row.get::<i64>(2).unwrap_or_default().unwrap_or(0),
                    row.get::<String>(3).unwrap_or_default().unwrap_or_default(),
                    row.get::<String>(4).unwrap_or_default(),
                ));
            }
            out
        });
        TableIterator::new(rows)
    }

    /// A file's heading spans, for naming the section a hit line falls in.
    fn section_spans(file_id: i64) -> Vec<(i64, i64, String)> {
        Spi::connect(|client| {
            let rows = client
                .select(
                    "SELECT line_from, line_to, heading_path FROM kb.section WHERE file_id = $1 ORDER BY line_from, level",
                    None,
                    &[file_id.into()],
                )
                .map_err(storage_err)?;
            let mut out = Vec::new();
            for r in rows {
                let a: Option<i64> = r.get(1).map_err(storage_err)?;
                let b: Option<i64> = r.get(2).map_err(storage_err)?;
                let h: Option<String> = r.get(3).map_err(storage_err)?;
                if let (Some(a), Some(b), Some(h)) = (a, b, h) {
                    out.push((a, b, h));
                }
            }
            Ok::<_, TextdbError>(out)
        })
        .unwrap_or_default()
    }

    /// The deepest heading span containing `line`; spans nest, so the last match is the most
    /// specific, which is the one a reader means.
    fn section_at(spans: &[(i64, i64, String)], line: i64) -> Option<String> {
        spans
            .iter()
            .filter(|(from, to, _)| *from <= line && line <= *to)
            .next_back()
            .map(|(_, _, h)| h.clone())
    }

    /// Full-text search: terms ANDed at document level, `"a b"` phrase, `foo*` prefix.
    /// Chunk hits → `chunk_ref` → live files under `prefix` → line via the tree.
    #[pg_extern(stable)]
    fn search(
        query: &str,
        prefix: default!(&str, "'/'"),
        lim: default!(i64, 200),
        per_file: default!(i64, 10),
    ) -> TableIterator<
        'static,
        (
            name!(path, String),
            name!(version, i64),
            name!(line, i64),
            name!(text, String),
            name!(section, Option<String>),
            name!(score, f64),
            name!(more, i64),
        ),
    > {
        let prefix = ok(normalize_path(prefix));
        let prefix = resolve(&prefix, false);
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
        let parsed = textdb_core::terms::parse(terms.clone());
        if parsed.is_empty() {
            return TableIterator::new(Vec::new());
        }
        let per_file = per_file.max(1) as usize;
        // Ranked best first already; the best value scales the rest into (0, 1]. `ts_rank` is
        // positive and bm25 is negative for the same meaning, so both are normalised here
        // rather than left for a caller to discover which engine it is talking to.
        let best = candidates.iter().map(|(_, _, r)| *r as f64).fold(f64::MIN, f64::max);
        let scale = if best > 0.0 { best } else { 1.0 };
        let mut hits = Vec::new();
        for (file_id, _chunk_id, rank) in candidates {
            if hits.len() >= limit {
                break;
            }
            let (path, root, version) = match Spi::connect(|client| {
                let rows = client
                    .select(
                        "SELECT path, root, version FROM kb.node WHERE id = $1 AND deleted_at IS NULL",
                        None,
                        &[file_id.into()],
                    )
                    .map_err(storage_err)?;
                let mut out = None;
                for r in rows {
                    let p: Option<String> = r.get(1).map_err(storage_err)?;
                    let rt: Option<Vec<u8>> = r.get(2).map_err(storage_err)?;
                    let v: Option<i64> = r.get(3).map_err(storage_err)?;
                    if let (Some(p), Some(rt), Some(v)) = (p, rt, v) {
                        out = Some((p, rt, v));
                    }
                }
                Ok::<_, TextdbError>(out)
            }) {
                Ok(Some(x)) => x,
                _ => continue,
            };
            let body = ok(materialize_all(&st, &ok(to_hash(&root))));
            let body = String::from_utf8_lossy(&body);
            let Some(lines) = textdb_core::terms::matching_lines(&parsed, &body) else {
                continue;
            };
            let more = lines.len().saturating_sub(per_file) as i64;
            let spans = section_spans(file_id);
            for (n, line) in lines.into_iter().take(per_file) {
                if hits.len() >= limit {
                    break;
                }
                hits.push((
                    path.clone(),
                    version,
                    n as i64,
                    textdb_core::terms::show(line, &parsed),
                    section_at(&spans, n as i64),
                    ((rank as f64) / scale).clamp(0.0, 1.0),
                    more,
                ));
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

    /// The line of `bytes` that best matches `terms`, and a window of it around the match.
    ///
    /// One implementation, shared with the SQLite binding, so a snippet means the same thing
    /// on either engine. This used to be a second copy of the same logic and drifted: it
    /// compared raw text against a diacritic-folding index, looked for a quoted phrase with
    /// its spaces intact, and showed the head of a long line rather than the match.
    fn locate_terms(bytes: &[u8], terms: &[String]) -> (usize, String) {
        textdb_core::snippet::locate_terms(bytes, terms)
    }


    // ================================================== accounts, tokens and shares (#12)
    //
    // The rules live in `textdb_core::access`, shared with the SQLite binding, so the two engines
    // cannot disagree about what a grant means. What is here is persistence and the SPI calls.

    /// This connection's account id, or `None` for the owner.
    fn account_id_now() -> Option<i64> {
        Spi::get_one::<i64>("SELECT kb.current_account()").ok().flatten()
    }

    /// Only the owner runs the delegation commands.
    fn admin_only(what: &str) {
        if account_id_now().is_some() {
            raise("TX005", &format!("only the owner of the store can {what}"), "");
        }
    }

    fn account_id_of(name: &str) -> i64 {
        let id = Spi::get_one_with_args::<i64>(
            "SELECT id FROM kb.account WHERE name = $1",
            &[name.into()],
        )
        .ok()
        .flatten();
        match id {
            Some(id) => id,
            None => raise("TX003", &format!("there is no account called '{name}'"), ""),
        }
    }

    /// This account's grants as the model's own type: the share roots' *current* paths, so a
    /// folder the owner renamed keeps its alias.
    fn grants_of(account_id: i64) -> textdb_core::access::Grants {
        use textdb_core::access::{Grant, Grants, Rights};
        let rows: Vec<Grant> = Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT g.node_id, g.alias, g.rights, n.path, (n.deleted_at IS NOT NULL), (g.revoked_at IS NOT NULL) \
                     FROM kb.grant g JOIN kb.node n ON n.id = g.node_id \
                     WHERE g.account_id = $1 ORDER BY g.alias",
                    None,
                    &[account_id.into()],
                )
                .map_err(storage_err)?;
            let mut out = Vec::new();
            for r in t {
                out.push(Grant {
                    node_id: r.get::<i64>(1).map_err(storage_err)?.unwrap_or_default(),
                    alias: r.get::<String>(2).map_err(storage_err)?.unwrap_or_default(),
                    rights: Rights::parse(&r.get::<String>(3).map_err(storage_err)?.unwrap_or_default()).unwrap_or(Rights::Ro),
                    store_path: r.get::<String>(4).map_err(storage_err)?.unwrap_or_default(),
                    dormant: r.get::<bool>(5).map_err(storage_err)?.unwrap_or(false),
                    revoked: r.get::<bool>(6).map_err(storage_err)?.unwrap_or(false),
                });
            }
            Ok::<_, TextdbError>(out)
        })
        .unwrap_or_else(|e| fail(e));
        Grants::from_rows(rows)
    }

    /// The node a share binds to. A share is a folder: sharing a file would be a different model.
    fn share_node(path: &str) -> (i64, String) {
        let path = ok(normalize_path(path));
        match NodeRow::by_path(&path, true) {
            Some(n) if n.kind == 0 => (n.id, path),
            Some(_) => fail(TextdbError::InvalidEdit(format!(
                "{path} is a file; a share is a folder and everything below it"
            ))),
            None => fail(TextdbError::NotFound(path)),
        }
    }

    #[pg_extern]
    fn account_create(name: &str, kind: default!(&str, "'agent'"), root: default!(Option<&str>, "NULL")) -> String {
        admin_only("create accounts");
        if name.is_empty() || name.contains('/') || name.trim() != name {
            fail(TextdbError::InvalidEdit(format!("'{name}' cannot be an account name")));
        }
        if !matches!(kind, "agent" | "person") {
            fail(TextdbError::InvalidEdit(format!(
                "account kind is 'agent' or 'person', not '{kind}'"
            )));
        }
        let taken = Spi::get_one_with_args::<i64>("SELECT id FROM kb.account WHERE name = $1", &[name.into()])
            .ok()
            .flatten();
        if taken.is_some() {
            fail(TextdbError::InvalidEdit(format!("there is already an account called '{name}'")));
        }
        let root_node = root.map(share_node);
        let id = Spi::get_one_with_args::<i64>(
            "INSERT INTO kb.account (name, kind, root_node_id) VALUES ($1, $2, $3) RETURNING id",
            &[name.into(), kind.into(), root_node.as_ref().map(|(id, _)| *id).into()],
        )
        .ok()
        .flatten()
        .unwrap_or_default();
        // A single-root account's root is a share held under the empty alias, so everything
        // downstream reads one table rather than two.
        if let Some((node_id, _)) = root_node {
            Spi::run_with_args(
                "INSERT INTO kb.grant (account_id, node_id, alias, rights, granted_by) VALUES ($1, $2, '', 'rw', 'owner')",
                &[id.into(), node_id.into()],
            )
            .unwrap_or_else(|e| spi_err(e));
        }
        name.to_string()
    }

    #[pg_extern(stable)]
    fn account_ls() -> TableIterator<
        'static,
        (
            name!(name, String),
            name!(kind, String),
            name!(root, Option<String>),
            name!(created_at, pgrx::datum::TimestampWithTimeZone),
            name!(disabled, bool),
            name!(shares, i64),
        ),
    > {
        admin_only("list accounts");
        let rows = Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT a.name, a.kind, r.path, a.created_at, (a.disabled_at IS NOT NULL), \
                            (SELECT count(*) FROM kb.grant g WHERE g.account_id = a.id) \
                       FROM kb.account a LEFT JOIN kb.node r ON r.id = a.root_node_id \
                      ORDER BY a.name",
                    None,
                    &[],
                )
                .map_err(storage_err)?;
            let mut out = Vec::new();
            for r in t {
                out.push((
                    r.get::<String>(1).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<String>(2).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<String>(3).map_err(storage_err)?,
                    r.get::<pgrx::datum::TimestampWithTimeZone>(4).map_err(storage_err)?.unwrap(),
                    r.get::<bool>(5).map_err(storage_err)?.unwrap_or(false),
                    r.get::<i64>(6).map_err(storage_err)?.unwrap_or_default(),
                ));
            }
            Ok::<_, TextdbError>(out)
        })
        .unwrap_or_else(|e| fail(e));
        TableIterator::new(rows)
    }

    #[pg_extern]
    fn account_convert(name: &str, alias: default!(Option<&str>, "NULL")) -> String {
        admin_only("convert accounts");
        let id = account_id_of(name);
        let root: Option<i64> =
            Spi::get_one_with_args("SELECT root_node_id FROM kb.account WHERE id = $1", &[id.into()]).ok().flatten();
        if root.is_none() {
            fail(TextdbError::InvalidEdit(format!("'{name}' already holds its shares under aliases")));
        }
        let grants = grants_of(id);
        let g = match grants.iter().next() {
            Some(g) => g.clone(),
            None => fail(TextdbError::InvalidEdit(format!("'{name}' has no share to convert"))),
        };
        let alias = alias
            .map(str::to_string)
            .unwrap_or_else(|| g.store_path.rsplit('/').next().unwrap_or("share").to_string());
        Spi::run_with_args(
            "UPDATE kb.grant SET alias = $2 WHERE account_id = $1",
            &[id.into(), alias.as_str().into()],
        )
        .unwrap_or_else(|e| spi_err(e));
        Spi::run_with_args("UPDATE kb.account SET root_node_id = NULL WHERE id = $1", &[id.into()])
            .unwrap_or_else(|e| spi_err(e));
        alias
    }

    /// Mint a bearer. Returned once; the store keeps only its hash.
    #[pg_extern]
    fn token_create(
        account: &str,
        label: default!(Option<&str>, "NULL"),
        expires_at: default!(Option<pgrx::datum::TimestampWithTimeZone>, "NULL"),
    ) -> TableIterator<'static, (name!(bearer, String), name!(id, i64))> {
        admin_only("create tokens");
        let account_id = account_id_of(account);
        let bearer = new_bearer();
        let id = Spi::get_one_with_args::<i64>(
            "INSERT INTO kb.token (account_id, hash, label, expires_at) \
             VALUES ($1, encode(sha256(convert_to($2, 'UTF8')), 'hex'), $3, $4) RETURNING id",
            &[account_id.into(), bearer.as_str().into(), label.into(), expires_at.into()],
        )
        .ok()
        .flatten()
        .unwrap_or_default();
        TableIterator::once((bearer, id))
    }

    /// 32 bytes of randomness, hex, prefixed so it is recognisable in a log or an environment
    /// variable and greppable by a secret scanner.
    fn new_bearer() -> String {
        let b: [u8; 32] = rand::random();
        format!("tdb_{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
    }

    #[pg_extern(stable)]
    fn token_ls(
        account: default!(Option<&str>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(id, i64),
            name!(account, String),
            name!(label, Option<String>),
            name!(created_at, pgrx::datum::TimestampWithTimeZone),
            name!(expires_at, Option<pgrx::datum::TimestampWithTimeZone>),
            name!(revoked_at, Option<pgrx::datum::TimestampWithTimeZone>),
            name!(last_used_at, Option<pgrx::datum::TimestampWithTimeZone>),
            name!(live, bool),
        ),
    > {
        admin_only("list tokens");
        let rows = Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT t.id, a.name, t.label, t.created_at, t.expires_at, t.revoked_at, t.last_used_at, \
                            (t.revoked_at IS NULL AND (t.expires_at IS NULL OR t.expires_at > now())) \
                       FROM kb.token t JOIN kb.account a ON a.id = t.account_id \
                      WHERE $1::text IS NULL OR a.name = $1 ORDER BY t.id",
                    None,
                    &[account.into()],
                )
                .map_err(storage_err)?;
            let mut out = Vec::new();
            for r in t {
                out.push((
                    r.get::<i64>(1).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<String>(2).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<String>(3).map_err(storage_err)?,
                    r.get::<pgrx::datum::TimestampWithTimeZone>(4).map_err(storage_err)?.unwrap(),
                    r.get::<pgrx::datum::TimestampWithTimeZone>(5).map_err(storage_err)?,
                    r.get::<pgrx::datum::TimestampWithTimeZone>(6).map_err(storage_err)?,
                    r.get::<pgrx::datum::TimestampWithTimeZone>(7).map_err(storage_err)?,
                    r.get::<bool>(8).map_err(storage_err)?.unwrap_or(false),
                ));
            }
            Ok::<_, TextdbError>(out)
        })
        .unwrap_or_else(|e| fail(e));
        TableIterator::new(rows)
    }

    #[pg_extern]
    fn token_revoke(id: i64) -> bool {
        admin_only("revoke tokens");
        let n = Spi::get_one_with_args::<i64>(
            "WITH u AS (UPDATE kb.token SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL RETURNING 1) \
             SELECT count(*) FROM u",
            &[id.into()],
        )
        .ok()
        .flatten()
        .unwrap_or_default();
        if n == 0 {
            fail(TextdbError::NotFound(format!("there is no live token {id}")));
        }
        true
    }

    #[pg_extern]
    fn access_grant(
        account: &str,
        path: &str,
        rights: &str,
        alias: default!(Option<&str>, "NULL"),
    ) -> TableIterator<'static, (name!(alias, String), name!(rights, String), name!(store_path, String), name!(node_id, i64))> {
        use textdb_core::access::{Namespace, Rights};
        admin_only("grant access");
        let rights = match Rights::parse(rights) {
            Some(r) => r,
            None => fail(TextdbError::InvalidEdit(format!("rights are 'ro' or 'rw', not '{rights}'"))),
        };
        let id = account_id_of(account);
        let single_root: Option<i64> =
            Spi::get_one_with_args("SELECT root_node_id FROM kb.account WHERE id = $1", &[id.into()]).ok().flatten();
        let ns = if single_root.is_some() { Namespace::SingleRoot } else { Namespace::Aliased };
        let (node_id, store_path) = share_node(path);
        let mut grants = grants_of(id);
        // Regranting the same node replaces its rights rather than being refused as a duplicate:
        // that is what "raise this share to rw" has to mean.
        let existing = grants.by_node(node_id).cloned();
        let g = match existing {
            Some(mut g) if alias.is_none() || alias == Some(g.alias.as_str()) => {
                g.rights = rights;
                g
            }
            _ => match grants.add(ns, node_id, &store_path, alias, rights) {
                Ok(g) => g,
                Err(e) => fail(TextdbError::InvalidEdit(e.to_string())),
            },
        };
        Spi::run_with_args(
            "INSERT INTO kb.grant (account_id, node_id, alias, rights, granted_by) VALUES ($1, $2, $3, $4, 'owner') \
             ON CONFLICT (account_id, node_id) DO UPDATE SET alias = excluded.alias, rights = excluded.rights, \
             granted_by = excluded.granted_by, granted_at = now(), revoked_at = NULL",
            &[id.into(), g.node_id.into(), g.alias.as_str().into(), g.rights.as_str().into()],
        )
        .unwrap_or_else(|e| spi_err(e));
        TableIterator::once((g.alias.clone(), g.rights.as_str().to_string(), g.store_path.clone(), g.node_id))
    }

    #[pg_extern]
    fn access_rename(account: &str, from: &str, to: &str) -> bool {
        admin_only("rename shares");
        let id = account_id_of(account);
        let mut grants = grants_of(id);
        if let Err(e) = grants.rename(from, to) {
            fail(TextdbError::InvalidEdit(e.to_string()));
        }
        let n = Spi::get_one_with_args::<i64>(
            "WITH u AS (UPDATE kb.grant SET alias = $3 WHERE account_id = $1 AND alias = $2 RETURNING 1) SELECT count(*) FROM u",
            &[id.into(), from.into(), to.into()],
        )
        .ok()
        .flatten()
        .unwrap_or_default();
        if n == 0 {
            fail(TextdbError::NotFound(format!("'{account}' has no share called '{from}'")));
        }
        true
    }

    #[pg_extern]
    fn access_revoke(account: &str, alias: &str) -> bool {
        admin_only("revoke shares");
        let id = account_id_of(account);
        let n = Spi::get_one_with_args::<i64>(
            "WITH d AS (UPDATE kb.grant SET revoked_at = now() WHERE account_id = $1 AND alias = $2 \
                        AND revoked_at IS NULL RETURNING 1) SELECT count(*) FROM d",
            &[id.into(), alias.into()],
        )
        .ok()
        .flatten()
        .unwrap_or_default();
        if n == 0 {
            fail(TextdbError::NotFound(format!("'{account}' has no share called '{alias}'")));
        }
        true
    }

    #[pg_extern(stable)]
    fn access_ls(
        who: default!(Option<&str>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(account, String),
            name!(alias, String),
            name!(rights, String),
            name!(store_path, String),
            name!(node_id, i64),
            name!(dormant, bool),
        ),
    > {
        admin_only("list shares");
        let rows = Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT a.name, g.alias, g.rights, n.path, g.node_id, (n.deleted_at IS NOT NULL OR g.revoked_at IS NOT NULL) \
                       FROM kb.grant g JOIN kb.account a ON a.id = g.account_id JOIN kb.node n ON n.id = g.node_id \
                      ORDER BY a.name, g.alias",
                    None,
                    &[],
                )
                .map_err(storage_err)?;
            let mut out = Vec::new();
            for r in t {
                out.push((
                    r.get::<String>(1).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<String>(2).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<String>(3).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<String>(4).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<i64>(5).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<bool>(6).map_err(storage_err)?.unwrap_or(false),
                ));
            }
            Ok::<_, TextdbError>(out)
        })
        .unwrap_or_else(|e| fail(e));
        // A path answers "who can see this?", an account name "what does this account see?".
        // Which was meant is decided by the shape of the argument, as `ls` does it.
        let rows = rows
            .into_iter()
            .filter(|(account, _, _, store_path, _, _)| match who {
                None => true,
                Some(w) if w.starts_with('/') => {
                    textdb_core::access::contains(store_path, w) || textdb_core::access::contains(w, store_path)
                }
                Some(w) => account == w,
            })
            .collect::<Vec<_>>();
        TableIterator::new(rows)
    }

    /// Who this connection is, and what it can see. One row per share; a single row with a NULL
    /// account for the owner.
    #[pg_extern(stable)]
    fn whoami() -> TableIterator<
        'static,
        (
            name!(account, Option<String>),
            name!(admin, bool),
            name!(kind, String),
            name!(namespace, String),
            name!(alias, Option<String>),
            name!(rights, Option<String>),
            name!(node_id, Option<i64>),
            name!(dormant, Option<bool>),
        ),
    > {
        let id = match account_id_now() {
            None => {
                return TableIterator::once((None, true, "owner".to_string(), "store".to_string(), None, None, None, None));
            }
            Some(id) => id,
        };
        let (name, kind, single_root) = Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT name, kind, (root_node_id IS NOT NULL) FROM kb.account WHERE id = $1",
                    None,
                    &[id.into()],
                )
                .map_err(storage_err)?;
            let r = t.into_iter().next();
            Ok::<_, TextdbError>(match r {
                Some(r) => (
                    r.get::<String>(1).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<String>(2).map_err(storage_err)?.unwrap_or_default(),
                    r.get::<bool>(3).map_err(storage_err)?.unwrap_or(false),
                ),
                None => (String::new(), String::new(), false),
            })
        })
        .unwrap_or_else(|e| fail(e));
        let ns = if single_root { "single-root" } else { "aliased" };
        let grants = grants_of(id);
        // An account is never told where its shares live in the store: the alias exists to hide
        // exactly that, and `whoami` is the one place it would be easy to leak it back.
        let rows: Vec<_> = grants
            .iter()
            .map(|g| {
                (
                    Some(name.clone()),
                    false,
                    kind.clone(),
                    ns.to_string(),
                    Some(g.alias.clone()),
                    Some(g.rights.as_str().to_string()),
                    Some(g.node_id),
                    Some(g.dormant),
                )
            })
            .collect();
        if rows.is_empty() {
            return TableIterator::once((Some(name), false, kind, ns.to_string(), None, None, None, None));
        }
        TableIterator::new(rows)
    }


    /// `mkdir -p`, for a caller's path. `kb._mkdir` is the internal one and takes a store path.
    #[pg_extern]
    fn mkdir(path: &str) -> i64 {
        let path = ok(normalize_path(path));
        let path = resolve(&path, true);
        _mkdir(&path)
    }

    /// Stop an account without forgetting it: its tokens stop working and its grants stay, so
    /// enabling it again is one command rather than re-granting everything it held.
    #[pg_extern]
    fn account_disable(name: &str, disabled: default!(bool, "true")) -> bool {
        admin_only("disable accounts");
        let id = account_id_of(name);
        Spi::run_with_args(
            "UPDATE kb.account SET disabled_at = CASE WHEN $2 THEN now() ELSE NULL END WHERE id = $1",
            &[id.into(), disabled.into()],
        )
        .unwrap_or_else(|e| spi_err(e));
        true
    }

    #[allow(dead_code)]
    fn _unused(_: DatumWithOid) {}
}

/// `cargo pgrx test` runs these in a scratch database with the extension installed.
#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    fn one<T: pgrx::datum::FromDatum + pgrx::datum::IntoDatum>(sql: &str) -> Option<T> {
        Spi::get_one::<T>(sql).expect(sql)
    }

    #[pg_test]
    fn write_read_move_and_the_feed() {
        Spi::run("SELECT kb.write('/notes/a.md', E'# A\\n\\nsee [[B]]\\n')").unwrap();
        assert_eq!(one::<String>("SELECT kb.content('/notes/a.md')").as_deref(), Some("# A\n\nsee [[B]]\n"));
        Spi::run("SELECT kb.move('/notes/a.md', '/archive/a.md', 'tester')").unwrap();
        assert_eq!(one::<String>("SELECT path FROM kb.file WHERE name = 'a.md'").as_deref(), Some("/archive/a.md"));
        assert_eq!(one::<i64>("SELECT count(*) FROM kb.feed(0) WHERE op = 'move' AND author = 'tester'"), Some(1));
    }

    #[pg_test]
    fn links_resolve_stay_current_and_follow_moves() {
        Spi::run("SELECT kb.write('/notes/Plan.md', E'# Plan\\n\\n## Next steps\\n')").unwrap();
        Spi::run(
            "SELECT kb.write('/acc/acme.md', E'[[Plan]] [[Plan#Next steps]] [[plan#Nope]]\\n[x](../notes/Plan.md) [[Missing]] ![[deck.pdf]] [[notes/]] [w](https://x.y)\\n')",
        )
        .unwrap();
        let statuses = "SELECT string_agg(coalesce(status, '?'), ',' ORDER BY id) FROM kb.link";
        assert_eq!(one::<String>(statuses).as_deref(), Some("ok,ok,anchor-missing,ok,broken,not-in-store,folder,external"));

        // A new file resolves links that waited for it; a second of the same name makes them ambiguous.
        Spi::run("SELECT kb.write('/Missing.md', 'here')").unwrap();
        assert_eq!(one::<String>("SELECT status FROM kb.link WHERE target_path = 'Missing'").as_deref(), Some("ok"));
        Spi::run("SELECT kb.write('/other/Plan.md', 'other')").unwrap();
        assert_eq!(one::<String>("SELECT status FROM kb.link WHERE target_path = 'Plan' AND anchor IS NULL").as_deref(), Some("ambiguous"));
        Spi::run("SELECT kb.remove('/other/Plan.md', 'tester', 'not needed')").unwrap();
        assert_eq!(one::<String>("SELECT status FROM kb.link WHERE target_path = 'Plan' AND anchor IS NULL").as_deref(), Some("ok"));
        assert_eq!(one::<String>("SELECT message FROM kb.change WHERE op = 'delete'").as_deref(), Some("not needed"));

        // Reported, not changed; then back, then rewritten in the style each link was written.
        let report = one::<pgrx::JsonB>("SELECT kb.move_links('/notes/Plan.md', '/archive/Plan-v1.md', 'tester', NULL, 'report')").unwrap();
        assert_eq!(report.0["links"].as_array().map(Vec::len), Some(4), "{}", report.0);
        Spi::run("SELECT kb.move_links('/archive/Plan-v1.md', '/notes/Plan.md', NULL, NULL, 'off')").unwrap();
        assert_eq!(one::<String>(statuses).as_deref(), Some("ok,ok,anchor-missing,ok,ok,not-in-store,folder,external"));
        let rewrite = one::<pgrx::JsonB>("SELECT kb.move_links('/notes/Plan.md', '/archive/2026/Plan-v1.md', 'tester', NULL, 'rewrite')").unwrap();
        assert!(rewrite.0["links"].as_array().unwrap().iter().all(|l| !l["version"].is_null()), "{}", rewrite.0);
        assert_eq!(
            one::<String>("SELECT kb.content('/acc/acme.md')").as_deref(),
            Some("[[Plan-v1]] [[Plan-v1#Next steps]] [[Plan-v1#Nope]]\n[x](../archive/2026/Plan-v1.md) [[Missing]] ![[deck.pdf]] [[notes/]] [w](https://x.y)\n")
        );
        assert_eq!(one::<String>("SELECT kb.set_setting('link_updates', 'Rewrite')").as_deref(), Some("rewrite"));
    }

    #[pg_test]
    fn messages_line_ranges_and_replacements() {
        let content = "SELECT kb.content('/p/a.md')";
        Spi::run("SELECT kb.write('/p/a.md', E'Acme one\\nAcme two\\n- item\\n')").unwrap();
        Spi::run("SELECT kb.edit('/p/a.md', 'one', 'uno', 'ana', 'spanish')").unwrap();
        Spi::run("SELECT kb.append('/p/a.md', E'tail\\n', 'ana', 'more')").unwrap();
        Spi::run("SELECT kb.replace_lines('/p/a.md', 3, 3, E'- point\\n', NULL, 'ana', 'point')").unwrap();
        let w = one::<pgrx::JsonB>(r#"SELECT kb.replace_ranges('/p/a.md', '[{"from": 4, "to": 3, "text": "x\n"}, {"from": 1, "to": 1, "text": "ONE\n"}]', 4, 'ana', 'ranges')"#).unwrap();
        assert_eq!(w.0["version"], 5, "{}", w.0);
        assert_eq!(one::<String>(content).as_deref(), Some("ONE\nAcme two\n- point\nx\ntail\n"));
        assert_eq!(
            one::<String>("SELECT string_agg(message, ',' ORDER BY version) FROM kb.commit WHERE file_id = kb._node_id('/p/a.md')").as_deref(),
            Some("spanish,more,point,ranges")
        );
        assert_eq!(one::<String>("SELECT string_agg(message, ',' ORDER BY seq) FROM kb.change WHERE op = 'commit'").as_deref(), Some("spanish,more,point,ranges"));
        let overlap = one::<pgrx::JsonB>(r#"SELECT kb._replace_ranges_j('/p/a.md', '[{"from": 1, "to": 2, "text": ""}, {"from": 2, "to": 2, "text": ""}]', NULL, NULL, NULL)"#).unwrap();
        assert_eq!(overlap.0["code"], "TX004", "{}", overlap.0);

        // Replacements: counts checked, several in one version.
        let wrong = one::<pgrx::JsonB>(r#"SELECT kb._replace_many_j('/p/a.md', '[["Acme", "Globex", 2]]', NULL, NULL)"#).unwrap();
        assert!(wrong.0["message"].as_str().unwrap().contains("expected 2 occurrences of \"Acme\", found 1"), "{}", wrong.0);
        assert_eq!(one::<i64>(r#"SELECT kb.replace_many('/p/a.md', '[["Acme", "Globex", 1], {"old": "- point", "new": "- item"}]', 'ana')"#), Some(6));
        assert_eq!(one::<i64>("SELECT kb.replace('/p/a.md', 'tail', 'end')"), Some(7));
        assert_eq!(one::<String>(content).as_deref(), Some("ONE\nGlobex two\n- item\nx\nend\n"));
        assert_eq!(one::<String>("SELECT message FROM kb.commit WHERE file_id = kb._node_id('/p/a.md') AND version = 7").as_deref(), Some("replace"));

        // The snippet is the line holding the most terms; history has sizes.
        Spi::run("SELECT kb.write('/s.md', E'alpha\\nbeta\\nalpha beta\\n')").unwrap();
        assert_eq!(one::<i64>("SELECT line FROM kb.search('alpha beta')"), Some(3));
        assert_eq!(one::<i64>("SELECT nbytes FROM kb.history('/s.md')"), Some(22));
    }

    #[pg_test]
    fn batches_are_listed_and_reverted() {
        Spi::run("SELECT kb.write('/p/a.md', E'Acme\\n')").unwrap();
        Spi::run("SELECT kb.write('/p/b.md', E'bee\\n')").unwrap();
        Spi::run("SELECT kb.write('/p/old/c.md', E'gone soon\\n')").unwrap();
        assert_eq!(one::<String>("SELECT kb.batch()"), None);
        Spi::run("SELECT set_config('textdb.batch', 'b1', true)").unwrap();
        assert_eq!(one::<String>("SELECT kb.batch()").as_deref(), Some("b1"));
        Spi::run("SELECT kb.replace('/p/a.md', 'Acme', 'Globex')").unwrap();
        Spi::run("SELECT kb.write('/p/new.md', 'new')").unwrap();
        Spi::run("SELECT kb.move('/p/b.md', '/p/b2.md')").unwrap();
        Spi::run("SELECT kb.remove('/p/old')").unwrap();
        Spi::run("SELECT set_config('textdb.batch', '', true)").unwrap();
        assert_eq!(one::<i64>("SELECT count(*) FROM kb.change WHERE batch = 'b1'"), Some(4));
        assert_eq!(one::<i64>("SELECT count(*) FROM kb.commit WHERE batch = 'b1'"), Some(2));
        let changes = one::<pgrx::JsonB>("SELECT kb.batch_changes('b1')").unwrap().0;
        let ops: Vec<&str> = changes.as_array().unwrap().iter().map(|c| c["op"].as_str().unwrap()).collect();
        assert_eq!(ops, ["edit", "create", "move", "delete"], "{changes}");
        assert!(changes[0]["diff"].as_str().unwrap().contains("-Acme\n+Globex\n"), "{changes}");

        let r = one::<pgrx::JsonB>("SELECT kb.revert_batch('b1', 'ana')").unwrap().0;
        assert_eq!(r["removed"], serde_json::json!(["/p/new.md"]), "{r}");
        assert_eq!(r["moved_back"], serde_json::json!([{ "from": "/p/b2.md", "to": "/p/b.md" }]), "{r}");
        assert_eq!(r["recreated"], serde_json::json!(["/p/old/c.md"]), "{r}");
        assert_eq!(r["restored"][0]["path"], "/p/a.md", "{r}");
        assert_eq!(one::<String>("SELECT kb.content('/p/a.md')").as_deref(), Some("Acme\n"));
        assert_eq!(one::<String>("SELECT kb.content('/p/old/c.md')").as_deref(), Some("gone soon\n"));
        assert_eq!(one::<String>("SELECT kb.content('/p/b.md')").as_deref(), Some("bee\n"));
        let again = one::<pgrx::JsonB>("SELECT kb._revert_batch_j('b1', NULL, false)").unwrap().0;
        assert_eq!(again["code"], "TX004", "{again}");
        let unknown = one::<pgrx::JsonB>("SELECT kb._revert_batch_j('nope', NULL, false)").unwrap().0;
        assert_eq!(unknown["code"], "TX003", "{unknown}");
    }
}

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
