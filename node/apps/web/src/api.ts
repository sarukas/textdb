import type { Hunk } from "./live/hunks";

export type { Hunk };

export interface Info {
  db: string;
  files: number;
  last_seq: number;
}

export type NodeKind = "file" | "folder";

/** A file or folder in a listing. A folder's size, lines, words and versions are totals over every file below it. */
/**
 * A listing row: the canonical record, key for key, from `docs/shapes.md`.
 *
 * Declared here rather than imported because this is a browser bundle and the client package
 * reaches for `node:sqlite`. It drifted from that package's copy — `updated_at` was nullable
 * on this side only — so the shape test in `api.shape.test.ts` now checks the two agree.
 */
export interface Entry {
  path: string;
  name: string;
  kind: NodeKind;
  /** A file's current version; null for a folder. */
  version: number | null;
  /** A file's own size; a folder's total over the live files below it. */
  nbytes: number;
  nlines: number;
  /** ISO-8601 UTC with milliseconds and `Z`. */
  updated_at: string;
  updated_by: string | null;
  id: number;
  /** The parent folder; null for the root. */
  dir: string | null;
  depth: number;
  ext: string | null;
  /** Front matter `title`, else the first level-1 heading. */
  title: string | null;
  nwords: number;
  nsections: number;
  nprops: number;
  nlinks: number;
  nlinks_broken: number;
  versions: number;
  created_at: string;
  /** Folder: live files and folders anywhere below it. */
  files: number | null;
  folders: number | null;
  nauthors: number;
  authors: AuthorCount[];
}

/** The old name for {@link Entry}, kept so call sites read unchanged. */
export type LsEntry = Entry;

export interface AuthorCount {
  author: string | null;
  commits: number;
  first_ts: string;
  last_ts: string;
}

export type SortKey = "name" | "type" | "size" | "lines" | "words" | "versions" | "created" | "updated" | "authors";

export interface ListQuery {
  sort: SortKey;
  order: "asc" | "desc";
  offset: number;
  limit: number;
  recursive?: boolean;
  name?: string;
  author?: string;
  type?: string;
  kind?: NodeKind;
}

export interface ListPage {
  path: string;
  total: number;
  offset: number;
  entries: LsEntry[];
}

/** The last `textdb sync` of a folder with its directory. */
export interface SyncState {
  dir: string;
  seq: number;
  synced_at: string;
  author: string | null;
  git: { commit: string | null; branch: string | null; remote: string | null; clean: boolean } | null;
  /** Files changed, added or deleted in the store since. */
  changed: number;
  conflicts: string[];
}

export interface SyncLink {
  prefix: string;
  dir: string;
  exists: boolean;
  running: boolean;
  last: SyncState | null;
}

export interface SyncLinks {
  available: boolean;
  reason: string | null;
  links: SyncLink[];
}

export interface SyncChanges {
  new: string[];
  changed: string[];
  deleted: string[];
}

export interface SyncNote {
  path: string;
  reason: string;
}

/** What `textdb sync --json` reports, planned (`dry_run`) or done. */
export interface SyncReport {
  prefix: string;
  dir: string;
  dry_run: boolean;
  first_sync: boolean;
  base_commit?: string;
  to_disk: SyncChanges;
  to_textdb: SyncChanges;
  moved: { from: string; to: string }[];
  merged: string[];
  conflicts: string[];
  unresolved: string[];
  kept: SyncNote[];
  unchanged: number;
  skipped: SyncNote[];
  failed: SyncNote[];
  problems: { path: string; kind: string; detail: string; platforms: string[]; blocking: boolean }[];
  stopped: boolean;
  seq: number | null;
  git: {
    commit: string | null;
    branch: string | null;
    remote: string | null;
    clean: boolean;
    authors_from: string | null;
    committed: string | null;
    commit_error: string | null;
  } | null;
  /** What the sync did with assets; absent when the folder has none and no asset store is declared. */
  assets?: SyncAssetsReport;
}

export interface SyncAssetsReport {
  /** The `asset_sync` setting or option this sync followed: off, push, pull or both. */
  mode: string;
  counts: Record<string, number>;
  trashed: string[];
  trash?: string;
  renamed: { from: string; to: string }[];
  conflict_copies: { from: string; to: string }[];
  pushed: string[];
  pulled: string[];
  conflicts: string[];
  failed: string[];
  notes: string[];
}

/** An asset of a synced folder: a binary kept in an asset store, with a `NAME.tdbasset` pointer. */
export interface AssetItem {
  /** The asset's own path, without `.tdbasset`. */
  path: string;
  state: AssetState;
  type: string;
  size?: number;
  store?: string;
  sha256?: string;
  /** The pointer's version in the store. */
  version?: number;
  /** Its file on the server's disk, relative to the folder's directory. */
  file?: string;
  note?: string;
}

export type AssetState =
  | "ok"
  | "new"
  | "modified"
  | "outdated"
  | "conflict"
  | "not-pulled"
  | "conflict-copy"
  | "orphan"
  | "invalid-pointer"
  | "invalid-path";

export interface AssetStatus {
  prefix: string;
  dir: string;
  assets: AssetItem[];
  counts: Record<string, number>;
}

export interface AssetPushReport {
  dry_run: boolean;
  pushed: { path: string; file?: string; state: string; size: number; store: string; version?: number }[];
  bytes: number;
  conflicts: string[];
  failed: string[];
}

export interface AssetPullReport {
  dry_run: boolean;
  pulled: { path: string; state: string; size: number; store: string }[];
  bytes: number;
  kept: string[];
  failed: string[];
}

/** A file an export writes, relative to the exported folder. */
export interface ExportFileInfo {
  rel: string;
  nbytes: number;
  updated_at: string;
}

export interface BulkResult {
  op: "move" | "delete";
  to: string | null;
  done: string[];
  skipped: string[];
}

export interface FileDoc {
  path: string;
  version: number;
  head_version: number;
  content: string;
  nbytes: number;
  nlines: number;
  updated_at: string | null;
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

export type CommitKind = "direct" | "rebased" | "merged";

export interface HistoryEntry {
  version: number;
  author: string | null;
  ts: string;
  message: string | null;
  nbytes: number;
  kind: CommitKind | null;
  base_version: number | null;
}

/** One matching line: the canonical hit row from `docs/shapes.md`. */
export interface SearchHit {
  path: string;
  /** The version the line number belongs to. */
  version: number;
  line: number;
  /** The matching line, windowed around the match when it is long. */
  text: string;
  /** The heading path the line sits under. */
  section: string | null;
  /** Relevance, higher is better, scaled to (0, 1]. */
  score: number | null;
  /** Matching lines in this file held back by `per_file`. */
  more: number;
}

export interface WriteResult {
  version: number;
  kind: CommitKind | "noop";
}

export interface ImportResult {
  created: number;
  updated: number;
  unchanged: number;
  failed: number;
  failures: { path: string; code: string; message: string }[];
}

export interface ConflictInfo {
  path: string;
  region_line_from: number;
  region_line_to: number;
  base: string;
  theirs: string;
  ours: string;
  current_version: number;
}

export type ChangeOp = "create" | "commit" | "mkdir" | "move" | "delete" | "purge";

export interface ChangeEvent {
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
  hunks?: Hunk[];
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly conflict?: ConflictInfo,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

function qs(params: Record<string, string | number | undefined | null>): string {
  const u = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== null) u.set(k, String(v));
  return u.toString();
}

/** Where the server sends an asset's file from a synced folder's directory: to show, or to download. */
export function assetFileUrl(prefix: string, path: string, download = false): string {
  return `/api/assets/file?${qs({ prefix, path, download: download ? 1 : undefined })}`;
}

async function request<T>(method: string, url: string, body?: unknown, signal?: AbortSignal): Promise<T> {
  const init: RequestInit = { method, headers: { accept: "application/json" } };
  if (signal) init.signal = signal;
  if (body !== undefined) {
    init.headers = { ...init.headers, "content-type": "application/json" };
    init.body = JSON.stringify(body);
  }
  const res = await fetch(url, init);
  const text = await res.text();
  let json: unknown = undefined;
  try {
    json = text ? JSON.parse(text) : undefined;
  } catch {
    // Not JSON (e.g. a proxy error page): reported below.
  }
  if (!res.ok) {
    const e = (json ?? {}) as { code?: string; message?: string; conflict?: ConflictInfo };
    throw new ApiError(res.status, e.code ?? `HTTP${res.status}`, e.message ?? (text.slice(0, 200) || res.statusText), e.conflict);
  }
  return json as T;
}

/** A front-matter property name in use across the store. */
export interface PropertyKey {
  key: string;
  /** Documents carrying it — a note with three tags counts once. */
  docs: number;
  /** Distinct values it takes. */
  valuesN: number;
  /** Whether `>` and `<` mean anything on this property. */
  kind: "number" | "text" | "mixed";
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
  /** The whole front matter, so the results table can show any column without refetching. */
  frontmatter: Record<string, unknown> | null;
}

export const api = {
  info: () => request<Info>("GET", "/api/info"),
  ls: (path: string, signal?: AbortSignal) => request<LsEntry[]>("GET", `/api/ls?${qs({ path })}`, undefined, signal),
  list: (path: string, q: ListQuery, signal?: AbortSignal) =>
    request<ListPage>(
      "GET",
      `/api/list?${qs({ path, ...q, recursive: q.recursive ? 1 : undefined })}`,
      undefined,
      signal,
    ),
  entry: (path: string, signal?: AbortSignal) => request<LsEntry>("GET", `/api/entry?${qs({ path })}`, undefined, signal),
  bulk: (body: { op: "move" | "delete"; paths: string[]; to?: string; author?: string }) =>
    request<BulkResult>("POST", "/api/bulk", body),
  exportFiles: (path: string, signal?: AbortSignal) =>
    request<{ path: string; files: ExportFileInfo[] }>("GET", `/api/export/files?${qs({ path })}`, undefined, signal),
  exportHashes: (paths: string[], signal?: AbortSignal) =>
    request<{ hashes: { path: string; sha256: string }[] }>("POST", "/api/export/hashes", { paths }, signal),
  /** A file's stored bytes, unchanged. */
  exportBytes: async (path: string, signal?: AbortSignal): Promise<Uint8Array> => {
    const res = await fetch(`/api/export/file?${qs({ path })}`, signal ? { signal } : {});
    if (!res.ok) {
      const text = await res.text();
      let e: { code?: string; message?: string } = {};
      try {
        e = JSON.parse(text) as typeof e;
      } catch {
        // not JSON
      }
      throw new ApiError(res.status, e.code ?? `HTTP${res.status}`, e.message ?? (text.slice(0, 200) || res.statusText));
    }
    return new Uint8Array(await res.arrayBuffer());
  },
  exportZipUrl: (path: string) => `/api/export/zip?${qs({ path })}`,
  syncLinks: () => request<SyncLinks>("GET", "/api/sync/links"),
  sync: (body: { prefix: string; dry_run?: boolean; commit?: boolean; base?: string | undefined; author?: string }) =>
    request<SyncReport>("POST", "/api/sync", body),
  syncConflict: (prefix: string, rel: string) =>
    request<{ rel: string; text: string }>("GET", `/api/sync/conflict?${qs({ prefix, rel })}`),
  syncResolve: (body: { prefix: string; rel: string; keep: "textdb" | "disk"; author?: string }) =>
    request<SyncReport>("POST", "/api/sync/resolve", body),
  assets: (prefix: string, path?: string, signal?: AbortSignal) =>
    request<AssetStatus>("GET", `/api/assets?${qs({ prefix, path })}`, undefined, signal),
  pullAssets: (body: { prefix: string; paths?: string[]; author?: string }) =>
    request<AssetPullReport>("POST", "/api/assets/pull", body),
  pushAssets: (body: { prefix: string; paths?: string[]; message?: string; author?: string }) =>
    request<AssetPushReport>("POST", "/api/assets/push", body),
  file: (path: string, version?: number) => request<FileDoc>("GET", `/api/file?${qs({ path, version })}`),
  chunks: (path: string, version?: number) => request<Chunk[]>("GET", `/api/chunks?${qs({ path, version })}`),
  history: (path: string) => request<HistoryEntry[]>("GET", `/api/history?${qs({ path })}`),
  hunks: (path: string, from: number, to: number) => request<Hunk[]>("GET", `/api/hunks?${qs({ path, from, to })}`),
  diff: (path: string, from: number, to: number) => request<{ diff: string }>("GET", `/api/diff?${qs({ path, from, to })}`),
  search: (q: string, opts: { prefix?: string; limit?: number; signal?: AbortSignal } = {}) =>
    request<SearchHit[]>("GET", `/api/search?${qs({ q, prefix: opts.prefix, limit: opts.limit })}`, undefined, opts.signal),
  // Front-matter discovery. `metaKeys` and `metaValues` back the autosuggest and are called
  // on every keystroke, so both take an abort signal — a stale suggestion list arriving after
  // a newer one would make the dropdown flicker between answers.
  metaKeys: (opts: { prefix?: string; limit?: number; signal?: AbortSignal } = {}) =>
    request<PropertyKey[]>("GET", `/api/meta/keys?${qs({ prefix: opts.prefix, limit: opts.limit })}`, undefined, opts.signal),
  metaValues: (key: string, opts: { prefix?: string; limit?: number; signal?: AbortSignal } = {}) =>
    request<PropertyValue[]>(
      "GET",
      `/api/meta/values?${qs({ key, prefix: opts.prefix, limit: opts.limit })}`,
      undefined,
      opts.signal,
    ),
  metaFind: (q: string, opts: { folder?: string; limit?: number; signal?: AbortSignal } = {}) =>
    request<PropertyHit[]>("GET", `/api/meta/find?${qs({ q, folder: opts.folder, limit: opts.limit })}`, undefined, opts.signal),
  write: (body: { path: string; content: string; base_version?: number; author?: string; message?: string }) =>
    request<WriteResult>("PUT", "/api/file", body),
  importBatch: (body: { author?: string; files: { path: string; content: string }[] }) =>
    request<ImportResult>("POST", "/api/import", body),
  stat: (path: string) => request<Stat>("GET", `/api/stat?path=${encodeURIComponent(path)}`),
  move: (from: string, to: string, author?: string) =>
    request<{ from: string; to: string }>("POST", "/api/move", { from, to, author }),
  remove: (path: string, author?: string) => request<{ path: string }>("POST", "/api/delete", { path, author }),
  pathHistory: (target: { path: string } | { id: number }) =>
    request<PathEvent[]>("GET", `/api/path-history?${"path" in target ? qs({ path: target.path }) : qs({ id: target.id })}`),
  trash: (parent?: number) => request<TrashEntry[]>("GET", `/api/trash?${qs({ parent })}`),
  trashFile: (id: number, version?: number) => request<TrashFile>("GET", `/api/trash/file?${qs({ id, version })}`),
  trashHistory: (id: number) => request<HistoryEntry[]>("GET", `/api/trash/history?${qs({ id })}`),
  purge: (id: number, author?: string) => request<PurgeStats>("POST", "/api/trash/purge", { id, author }),
  emptyTrash: (author?: string) => request<PurgeStats>("POST", "/api/trash/empty", { author }),
};

/** A rename, move or delete as it touched one file or folder. */
export interface PathEvent {
  id: number;
  ts: string;
  op: "rename" | "move" | "delete";
  old_path: string;
  /** Where it went; null for a delete. */
  new_path: string | null;
  /** The folder the operation named, when this file or folder went along with it. */
  via: string | null;
  /** A file's version when it happened. */
  version: number | null;
  author: string | null;
}

/** Something a delete left behind, readable until it is purged. */
export interface TrashEntry {
  id: number;
  name: string;
  kind: "file" | "folder";
  /** Where it was when it was deleted. */
  path: string;
  version: number;
  /** A file's size; for a folder, the total of the files deleted with it. */
  nbytes: number;
  nlines: number | null;
  /** 1 for a file; for a folder, the files deleted with it. */
  files: number;
  updated_at: string;
  updated_by: string | null;
  deleted_at: string;
  deleted_by: string | null;
}

export interface TrashFile {
  entry: TrashEntry;
  version: number;
  content: string;
}

export interface PurgeStats {
  items: number;
  files: number;
  folders: number;
  versions: number;
  chunks: number;
  tree_nodes: number;
  bytes: number;
}

export interface Stat {
  path: string;
  kind: "file" | "folder";
  /** Files in the subtree (1 for a file). */
  files: number;
  /** Folders below a folder, not counting itself. */
  folders: number;
  nbytes: number;
}

export type ConnectionState = "connecting" | "live" | "offline";

export interface Subscription {
  close(): void;
}

/**
 * Subscribe to the change feed. The browser's EventSource reconnects on its own with
 * `Last-Event-ID`; when it gives up (the proxy answered with an error instead of a stream) a
 * new one is opened from the last seen `seq`. Events are delivered once, in `seq` order.
 */
export function subscribe(
  since: number,
  handlers: {
    onEvent: (e: ChangeEvent) => void;
    onState: (s: ConnectionState) => void;
    /** Called when the stream is back after an interruption, with the last seq seen. */
    onReconnect?: (lastSeq: number) => void;
  },
): Subscription {
  let lastSeq = since;
  let es: EventSource | null = null;
  let closed = false;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let backoff = 500;
  let interrupted = false;

  const open = () => {
    if (closed) return;
    handlers.onState("connecting");
    es = new EventSource(`/api/events?${qs({ since: lastSeq })}`);
    es.addEventListener("open", () => {
      backoff = 500;
      handlers.onState("live");
      if (interrupted) {
        interrupted = false;
        handlers.onReconnect?.(lastSeq);
      }
    });
    es.addEventListener("change", (msg) => {
      let e: ChangeEvent;
      try {
        e = JSON.parse((msg as MessageEvent<string>).data) as ChangeEvent;
      } catch {
        return;
      }
      if (typeof e.seq !== "number" || e.seq <= lastSeq) return;
      lastSeq = e.seq;
      handlers.onEvent(e);
    });
    es.addEventListener("error", () => {
      if (closed || !es) return;
      interrupted = true;
      if (es.readyState === EventSource.CLOSED) {
        handlers.onState("offline");
        es = null;
        retry = setTimeout(open, backoff);
        backoff = Math.min(backoff * 2, 10_000);
      } else {
        handlers.onState("connecting");
      }
    });
  };
  open();
  return {
    close() {
      closed = true;
      if (retry) clearTimeout(retry);
      es?.close();
    },
  };
}
