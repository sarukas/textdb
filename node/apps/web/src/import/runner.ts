import { api, ApiError, type ImportResult } from "../api";
import { destPath, planBatches } from "./select";
import { readText, type PickedFile } from "./source";

/** Content per request, and files per request, sent to `POST /api/import`. */
const BATCH_BYTES = 4 << 20;
const BATCH_FILES = 500;
/** Failures listed in the dialog; all of them are counted. */
const FAILURES_KEPT = 200;

export type ImportState = "running" | "cancelling" | "cancelled" | "done" | "error";

export interface ImportProgress {
  state: ImportState;
  totalFiles: number;
  totalBytes: number;
  doneFiles: number;
  doneBytes: number;
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
  /** Checked between requests: a request that has started always completes. */
  shouldStop: () => boolean;
}

const sizeOf = (files: readonly PickedFile[]) => files.reduce((n, f) => n + f.size, 0);

/** Import `files` under `prefix` a few megabytes at a time, reporting after every request. */
export async function runImport({ files, prefix, author, onProgress, shouldStop }: RunOptions): Promise<ImportProgress> {
  let progress: ImportProgress = {
    state: "running",
    totalFiles: files.length,
    totalBytes: sizeOf(files),
    doneFiles: 0,
    doneBytes: 0,
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
  const emit = (next: Partial<ImportProgress>) => {
    progress = { ...progress, ...next };
    onProgress(progress);
  };
  emit({});

  for (const batch of planBatches(files, BATCH_BYTES, BATCH_FILES)) {
    if (shouldStop()) {
      emit({ state: "cancelled", finishedAt: Date.now() });
      return progress;
    }
    const failures = [...progress.failures];
    const note = (path: string, reason: string) => {
      if (failures.length < FAILURES_KEPT) failures.push({ path, reason });
    };

    let skipped = 0;
    const payload: { path: string; content: string }[] = [];
    const contents = await Promise.all(
      batch.map((f) => readText(f.file).catch((e: unknown) => ({ skip: e instanceof Error ? e.message : String(e) }))),
    );
    batch.forEach((f, i) => {
      const path = destPath(prefix, f.rel);
      const content = contents[i]!;
      if ("skip" in content) {
        skipped++;
        note(path, `skipped: ${content.skip}`);
      } else {
        payload.push({ path, content: content.text });
      }
    });

    let result: ImportResult = { created: 0, updated: 0, unchanged: 0, failed: 0, failures: [] };
    if (payload.length > 0) {
      try {
        result = await api.importBatch({ author, files: payload });
      } catch (e) {
        const error = e instanceof ApiError ? `${e.code}: ${e.message}` : e instanceof Error ? e.message : String(e);
        emit({ state: "error", error, failures, skipped: progress.skipped + skipped, finishedAt: Date.now() });
        return progress;
      }
    }
    for (const f of result.failures) note(f.path, `${f.code}: ${f.message}`);

    emit({
      doneFiles: progress.doneFiles + batch.length,
      doneBytes: progress.doneBytes + sizeOf(batch),
      created: progress.created + result.created,
      updated: progress.updated + result.updated,
      unchanged: progress.unchanged + result.unchanged,
      failed: progress.failed + result.failed,
      skipped: progress.skipped + skipped,
      failures,
    });
  }
  emit({ state: "done", finishedAt: Date.now() });
  return progress;
}
