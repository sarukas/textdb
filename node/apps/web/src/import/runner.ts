import { api, ApiError, type ImportResult } from "../api";
import { destPath, formatBytes, formatDuration } from "./select";
import { errorText, readText, type PickedFile } from "./source";

export interface ImportLimits {
  /** Content per request, and files per request, sent to `POST /api/import`. */
  batchBytes: number;
  batchFiles: number;
  /** Files opened and read at the same time. */
  readers: number;
  /** A file not read by then is skipped (an online-only cloud file may never arrive). */
  readTimeoutMs: number;
  /** Larger files are skipped rather than held in memory. */
  maxFileBytes: number;
}

export const DEFAULT_LIMITS: ImportLimits = {
  batchBytes: 4 << 20,
  batchFiles: 500,
  readers: 8,
  readTimeoutMs: 60_000,
  maxFileBytes: 64 << 20,
};

/** Failures listed in the dialog; all of them are counted. */
const FAILURES_KEPT = 200;

export type ImportState = "running" | "cancelling" | "cancelled" | "done" | "error";

export interface ImportProgress {
  state: ImportState;
  totalFiles: number;
  /** Files settled: sent, refused by the store, or skipped. */
  doneFiles: number;
  sentBytes: number;
  created: number;
  updated: number;
  unchanged: number;
  failed: number;
  skipped: number;
  failures: { path: string; reason: string }[];
  startedAt: number;
  finishedAt: number | null;
  error: string | null;
}

interface RunOptions {
  files: readonly PickedFile[];
  prefix: string;
  author: string;
  onProgress: (progress: ImportProgress) => void;
  /** Checked before each file is read: a request that has started always completes. */
  shouldStop: () => boolean;
  limits?: Partial<ImportLimits>;
  send?: (body: { author?: string; files: { path: string; content: string }[] }) => Promise<ImportResult>;
}

type Read = { text: string; size: number } | { skip: string };

async function readFile(file: PickedFile, limits: ImportLimits): Promise<Read> {
  const work = (async (): Promise<Read> => {
    const f = await file.open();
    if (f.size > limits.maxFileBytes) return { skip: `larger than ${formatBytes(limits.maxFileBytes)}` };
    const read = await readText(f);
    return "skip" in read ? read : { text: read.text, size: f.size };
  })().catch((e: unknown): Read => ({ skip: `could not be read: ${errorText(e)}` }));
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<Read>((resolve) => {
    timer = setTimeout(
      () => {
        const ms = limits.readTimeoutMs;
        resolve({ skip: `not read within ${ms < 1000 ? `${ms} ms` : formatDuration(ms / 1000)}` });
      },
      limits.readTimeoutMs,
    );
  });
  try {
    return await Promise.race([work, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

interface Batch {
  payload: { path: string; content: string }[];
  bytes: number;
  files: number;
  skipped: number;
  notes: { path: string; reason: string }[];
}

const emptyBatch = (): Batch => ({ payload: [], bytes: 0, files: 0, skipped: 0, notes: [] });

/**
 * Import `files` under `prefix`. A few readers open files as they go and fill a batch; a full
 * batch is sent while reading carries on into the next one, and readers wait for the store when
 * it falls behind. Progress is reported after every request.
 */
export async function runImport(options: RunOptions): Promise<ImportProgress> {
  const { files, prefix, author, onProgress, shouldStop } = options;
  const limits = { ...DEFAULT_LIMITS, ...options.limits };
  const send = options.send ?? api.importBatch;

  let progress: ImportProgress = {
    state: "running",
    totalFiles: files.length,
    doneFiles: 0,
    sentBytes: 0,
    created: 0,
    updated: 0,
    unchanged: 0,
    failed: 0,
    skipped: 0,
    failures: [],
    startedAt: Date.now(),
    finishedAt: null,
    error: null,
  };
  let stopped = false;
  const emit = (next: Partial<ImportProgress>) => {
    progress = { ...progress, ...next };
    if (progress.state === "running" && (stopped || shouldStop())) progress = { ...progress, state: "cancelling" };
    onProgress(progress);
  };
  emit({});

  let open = emptyBatch();
  let sending: Promise<void> = Promise.resolve();
  let next = 0;

  const post = async (batch: Batch) => {
    if (progress.state === "error" || batch.files === 0) return;
    let result: ImportResult = { created: 0, updated: 0, unchanged: 0, failed: 0, failures: [] };
    if (batch.payload.length > 0) {
      try {
        result = await send({ author, files: batch.payload });
      } catch (e) {
        stopped = true;
        const error = e instanceof ApiError ? `${e.code}: ${e.message}` : errorText(e);
        emit({ state: "error", error, finishedAt: Date.now() });
        return;
      }
    }
    const failures = [...progress.failures];
    const keep = (path: string, reason: string) => {
      if (failures.length < FAILURES_KEPT) failures.push({ path, reason });
    };
    for (const n of batch.notes) keep(n.path, n.reason);
    for (const f of result.failures) keep(f.path, `${f.code}: ${f.message}`);
    emit({
      doneFiles: progress.doneFiles + batch.files,
      sentBytes: progress.sentBytes + batch.bytes,
      created: progress.created + result.created,
      updated: progress.updated + result.updated,
      unchanged: progress.unchanged + result.unchanged,
      failed: progress.failed + result.failed,
      skipped: progress.skipped + batch.skipped,
      failures,
    });
  };

  /** Hand the open batch to the sender (requests go one at a time, in order). */
  const flush = () => {
    const batch = open;
    open = emptyBatch();
    sending = sending.then(() => post(batch));
    return sending;
  };

  const reader = async () => {
    for (;;) {
      if (stopped || shouldStop()) {
        stopped = true;
        return;
      }
      const i = next++;
      if (i >= files.length) return;
      const file = files[i]!;
      const path = destPath(prefix, file.rel);
      const read = await readFile(file, limits);
      if (stopped) return;
      let full: Promise<void> | null = null;
      if ("skip" in read) {
        open.skipped++;
        open.notes.push({ path, reason: `skipped: ${read.skip}` });
      } else {
        // A file that would overflow the batch starts the next one; a large file travels alone.
        if (open.payload.length > 0 && open.bytes + read.size > limits.batchBytes) full = flush();
        open.payload.push({ path, content: read.text });
        open.bytes += read.size;
      }
      open.files++;
      if (open.files >= limits.batchFiles || open.bytes >= limits.batchBytes) full = flush();
      if (full) await full;
    }
  };

  await Promise.all(Array.from({ length: Math.max(1, limits.readers) }, reader));
  // A stop leaves the files read since the last request unsent.
  await (stopped ? sending : flush());

  if (progress.state !== "error") emit({ state: stopped ? "cancelled" : "done", finishedAt: Date.now() });
  return progress;
}
