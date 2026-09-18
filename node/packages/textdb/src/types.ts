export type NodeKind = 'file' | 'folder';
export type CommitKind = 'direct' | 'rebased' | 'merged';
export type WriteKind = CommitKind | 'noop';
export type ChangeOp = 'create' | 'commit' | 'mkdir' | 'move' | 'delete' | 'purge';

export interface Info {
  db: string;
  files: number;
  last_seq: number;
}

/** A file or folder in a listing. A folder's size, lines, words and versions are totals over every file below it. */
/**
 * One listing row: the same twenty-four keys as every other surface, in this order.
 *
 * Every key is always present; one that does not apply is `null`.
 */
export interface Entry {
  // The minimal tier: what every surface carries.
  path: string;
  name: string;
  kind: NodeKind;
  /** A file's current version — what `cat -n` shows and `base_version` takes; null for a folder. */
  version: number | null;
  /** A file's own size; a folder's total over the live files below it. */
  nbytes: number;
  nlines: number;
  /** ISO-8601 UTC with milliseconds and `Z`, on both backends. */
  updated_at: string;
  updated_by: string | null;

  // The rest of the full tier.
  id: number;
  /** The parent folder; null for the root. */
  dir: string | null;
  depth: number;
  /** Lower case, no dot; null for a folder or a name without one. */
  ext: string | null;
  /** Front matter `title`, else the first level-1 heading, else null. */
  title: string | null;
  /** Words, as `wc -w` counts them. */
  nwords: number;
  /** Headings, top-level front-matter keys, links, and links that reach nothing. */
  nsections: number;
  nprops: number;
  nlinks: number;
  nlinks_broken: number;
  /** A file's version count; a folder's sum of the versions below it. */
  versions: number;
  created_at: string;
  /** Folder: live files anywhere below it; null for a file. */
  files: number | null;
  /** Folder: live folders anywhere below it; null for a file. */
  folders: number | null;
  nauthors: number;
  /** Who committed to it, most commits first; empty for a folder. */
  authors: AuthorCount[];
}

export interface AuthorCount {
  /** Null for commits made without an author. */
  author: string | null;
  commits: number;
  first_ts: string;
  last_ts: string;
}

export type SortKey = 'name' | 'type' | 'size' | 'lines' | 'words' | 'versions' | 'created' | 'updated' | 'authors';

export const SORT_KEYS: readonly SortKey[] = ['name', 'type', 'size', 'lines', 'words', 'versions', 'created', 'updated', 'authors'];

export interface ListOptions {
  /** Default `name`. Folders come before files, except in a recursive listing. */
  sort?: SortKey;
  order?: 'asc' | 'desc';
  offset?: number;
  /** Default 200, at most 1000. */
  limit?: number;
  /** Everything below the folder instead of its own entries. */
  recursive?: boolean;
  /** Keep names containing this, or matching it as a glob when it has `*` or `?`; ASCII case-insensitive. */
  name?: string;
  /** Keep files this author committed to; `''` for commits without an author. */
  author?: string;
  /** Keep files with this extension (`md` or `.md`). */
  type?: string;
  kind?: NodeKind;
}

/** One page of a sorted, filtered listing. */
export interface ListPage {
  path: string;
  /** Entries matching the filters, across all pages. */
  total: number;
  offset: number;
  /** The page size actually applied, which the caller's request may have been clamped to. */
  limit: number;
  entries: Entry[];
}

export interface FileView {
  path: string;
  version: number;
  head_version: number;
  content: string;
  nbytes: number;
  nlines: number;
  updated_at: string;
  updated_by: string | null;
}

export interface Chunk {
  ord: number;
  hash: string;
  byte_from: number;
  nbytes: number;
  line_from: number;
  nlines: number;
}

export interface HistoryEntry {
  version: number;
  author: string | null;
  ts: string;
  message: string | null;
  /** Null for commits recorded by a build that predates commit kinds. */
  kind: CommitKind | null;
  base_version: number | null;
  nbytes: number;
  /** The file's size in lines and words as of this version. */
  nlines: number | null;
  nwords: number | null;
}

/** One link: the canonical row of `docs/shapes.md`, the same ten keys on every surface. */
export interface Link {
  /** The file the link is written in. */
  path: string;
  /** The version the line number belongs to; pass it as `baseVersion` when editing by line. */
  version: number;
  line: number;
  kind: 'wiki' | 'embed' | 'md' | 'image';
  target: string;
  anchor: string | null;
  alias: string | null;
  status: 'ok' | 'ambiguous' | 'anchor-missing' | 'broken' | 'not-in-store' | 'external' | null;
  /** The file it points to; for an asset, the asset rather than its `.tdbasset` pointer. */
  resolved: string | null;
  asset: boolean;
}

/** Which links a `links` call returns: those a status names, or all of them. */
export type LinkStatus = NonNullable<Link['status']>;

export const LINK_STATUSES: readonly LinkStatus[] = [
  'ok',
  'ambiguous',
  'anchor-missing',
  'broken',
  'not-in-store',
  'external',
];

export interface LinkOptions {
  /** Keep only links with this status; `broken` is the one worth asking for. */
  status?: LinkStatus;
  /** Default 10000. */
  limit?: number;
}

export interface Hunk {
  old_from: number;
  old_count: number;
  new_from: number;
  new_count: number;
  old_text: string;
  new_text: string;
}

/**
 * One matching line — the same seven keys as the CLI and the SQL functions.
 *
 * One row per matching *line*, not per document with a guessed line.
 */
export interface SearchHit {
  path: string;
  /** The version the line number belongs to; pass it as `base_version` when editing. */
  version: number;
  line: number;
  /** The matching line, windowed around the match when longer than the cut. */
  text: string;
  /** The heading path the line sits under; null outside any heading. */
  section: string | null;
  /** Relevance, higher is better, scaled to (0, 1]; null when nothing ranked. */
  score: number | null;
  /** Matching lines in this file not returned because of `perFile`. */
  more: number;
}

/** A front-matter property name in use across the store. */
export interface PropertyKey {
  key: string;
  /** Documents carrying it — a note with three tags counts once. */
  docs: number;
  /**
   * Distinct values it takes.
   *
   * `values_n` and not `values` on every surface: `values` is reserved in SQL, so a column
   * named that would need quoting in every query that touched it.
   */
  values_n: number;
  /** `number`, `text` or `mixed`; a UI offers `>` and `<` only where they mean something. */
  kind: 'number' | 'text' | 'mixed';
}

/** One value a property takes, and how many documents use it. */
export interface PropertyValue {
  value: string | null;
  docs: number;
}

/** A document matched by a property query. */
export interface PropertyHit {
  path: string;
  nbytes: number;
  updated_at: string;
  /** The whole front matter, so a result table can show any column without a query per row. */
  frontmatter: Record<string, unknown> | null;
}

/** One markdown heading, with its document's own figures alongside. */
export interface OutlineEntry {
  path: string;
  /** The last component of the heading path, as written. */
  heading: string;
  /** The breadcrumb, `Parent / Child`. */
  headingPath: string;
  /** 1 for `#`, 2 for `##`, and so on. */
  level: number;
  lineFrom: number;
  lineTo: number;
  /** Words in the section's own lines. */
  nwords: number | null;
  /** Words in the section and everything nested under it. */
  nwordsTotal: number | null;
  /** The document's own figures, repeated on each of its rows. */
  nbytes: number | null;
  nlines: number | null;
  fileNwords: number | null;
  version: number;
  updated_at: string;
  updatedBy: string | null;
}

/** A distinct heading in use across the scope asked about. */
export interface HeadingName {
  heading: string;
  sections: number;
  docs: number;
}

/** How `outline` matches the heading it is given. */
export type HeadingMatch = 'exact' | 'prefix' | 'contains';

export interface WriteResult {
  version: number;
  kind: WriteKind;
}

export interface Change {
  seq: number;
  ts: string;
  op: ChangeOp;
  path: string;
  old_path: string | null;
  node_kind: NodeKind;
  version: number | null;
  base_version: number | null;
  commit_kind: CommitKind | null;
  author: string | null;
  message: string | null;
}
