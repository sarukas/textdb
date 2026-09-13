import type { Hunk } from "./live/hunks";

export type { Hunk };

export interface Info {
  db: string;
  files: number;
  last_seq: number;
}

export type NodeKind = "file" | "folder";

export interface LsEntry {
  name: string;
  path: string;
  kind: NodeKind;
  nbytes: number | null;
  nlines: number | null;
  updated_at: string | null;
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

export interface SearchHit {
  path: string;
  line: number;
  snippet: string;
  rank: number;
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

export type ChangeOp = "create" | "commit" | "mkdir" | "move" | "delete";

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

export const api = {
  info: () => request<Info>("GET", "/api/info"),
  ls: (path: string, signal?: AbortSignal) => request<LsEntry[]>("GET", `/api/ls?${qs({ path })}`, undefined, signal),
  file: (path: string, version?: number) => request<FileDoc>("GET", `/api/file?${qs({ path, version })}`),
  chunks: (path: string, version?: number) => request<Chunk[]>("GET", `/api/chunks?${qs({ path, version })}`),
  history: (path: string) => request<HistoryEntry[]>("GET", `/api/history?${qs({ path })}`),
  hunks: (path: string, from: number, to: number) => request<Hunk[]>("GET", `/api/hunks?${qs({ path, from, to })}`),
  diff: (path: string, from: number, to: number) => request<{ diff: string }>("GET", `/api/diff?${qs({ path, from, to })}`),
  search: (q: string, opts: { prefix?: string; limit?: number; signal?: AbortSignal } = {}) =>
    request<SearchHit[]>("GET", `/api/search?${qs({ q, prefix: opts.prefix, limit: opts.limit })}`, undefined, opts.signal),
  write: (body: { path: string; content: string; base_version?: number; author?: string; message?: string }) =>
    request<WriteResult>("PUT", "/api/file", body),
  importBatch: (body: { author?: string; files: { path: string; content: string }[] }) =>
    request<ImportResult>("POST", "/api/import", body),
};

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
