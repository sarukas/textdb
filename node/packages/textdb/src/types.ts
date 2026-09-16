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
export interface Entry {
  id: number;
  name: string;
  path: string;
  kind: NodeKind;
  nbytes: number | null;
  nlines: number | null;
  /** Words, as `wc -w` counts them. */
  nwords: number | null;
  /** A file's version; a folder's total of versions below it. */
  versions: number;
  /** A file's last commit or move; a folder's latest change, to it or anywhere below. */
  updated_at: string;
  updated_by: string | null;
  created_at: string;
  /** Folder: live files anywhere below it; null for a file. */
  files: number | null;
  /** Folder: live folders anywhere below it; null for a file. */
  folders: number | null;
  /** File: distinct commit authors; null for a folder. */
  nauthors: number | null;
  /** File: who committed to it, most commits first; empty for a folder. */
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
  nbytes: number;
  /** Null for commits recorded by a build that predates commit kinds. */
  kind: CommitKind | null;
  base_version: number | null;
}

export interface Hunk {
  old_from: number;
  old_count: number;
  new_from: number;
  new_count: number;
  old_text: string;
  new_text: string;
}

export interface SearchHit {
  path: string;
  line: number;
  snippet: string;
  rank: number;
}

/** A front-matter property name in use across the store. */
export interface PropertyKey {
  key: string;
  /** Documents carrying it — a note with three tags counts once. */
  docs: number;
  /** Distinct values it takes. */
  valuesN: number;
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
  updatedAt: string;
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
  updatedAt: string;
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
