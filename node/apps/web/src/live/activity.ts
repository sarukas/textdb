import type { ChangeEvent } from "../api";
import { parentOf } from "./paths";

/** One change, shown as it is. */
export interface EventItem {
  kind: "event";
  key: string;
  event: ChangeEvent;
}

/** A run of imported files, and the folders created for them, shown as one row. */
export interface ImportGroup {
  kind: "import";
  key: string;
  author: string | null;
  files: number;
  folders: number;
  /** The deepest folder containing everything in the run. */
  prefix: string;
  /** The run's first file, opened when the row is clicked. */
  first: ChangeEvent;
  firstSeq: number;
  lastSeq: number;
  ts: string;
}

export type FeedItem = EventItem | ImportGroup;

/** Written by an import (the web UI's or the CLI's), which records the message `import`. */
function isImportWrite(e: ChangeEvent): boolean {
  return e.message === "import" && e.node_kind === "file" && (e.op === "create" || e.op === "commit");
}

/** A folder created on the way to a file, which the store records without an author. */
function isImplicitFolder(e: ChangeEvent): boolean {
  return e.op === "mkdir" && e.author === null;
}

export function commonFolder(a: string, b: string): string {
  const x = a.split("/").filter(Boolean);
  const y = b.split("/").filter(Boolean);
  let i = 0;
  while (i < x.length && i < y.length && x[i] === y[i]) i++;
  return "/" + x.slice(0, i).join("/");
}

/**
 * Add `events` (oldest first) to `feed` (newest first) and keep at most `cap` rows.
 *
 * Consecutive import writes by one author, with the folders created for them, collapse into
 * a single row that keeps counting. Otherwise a large import would push every other change
 * out of the feed, and a capped list of its rows could not say how many files it imported.
 */
export function addToFeed(feed: readonly FeedItem[], events: readonly ChangeEvent[], cap: number): FeedItem[] {
  const out = [...feed];
  for (const e of events) {
    const head = out[0];
    if (head?.kind === "import" && (isImplicitFolder(e) || (isImportWrite(e) && e.author === head.author))) {
      const file = isImportWrite(e);
      out[0] = {
        ...head,
        files: head.files + (file ? 1 : 0),
        folders: head.folders + (file ? 0 : 1),
        prefix: commonFolder(head.prefix, file ? parentOf(e.path) : e.path),
        lastSeq: e.seq,
        ts: e.ts,
      };
      continue;
    }
    if (isImportWrite(e)) {
      // The folders an import creates are announced just before its first file.
      let folders = 0;
      let prefix = parentOf(e.path);
      let firstSeq = e.seq;
      for (let top = out[0]; top?.kind === "event" && isImplicitFolder(top.event); top = out[0]) {
        out.shift();
        folders++;
        prefix = commonFolder(prefix, top.event.path);
        firstSeq = top.event.seq;
      }
      out.unshift({ kind: "import", key: `import:${firstSeq}`, author: e.author, files: 1, folders, prefix, first: e, firstSeq, lastSeq: e.seq, ts: e.ts });
      continue;
    }
    out.unshift({ kind: "event", key: `event:${e.seq}`, event: e });
  }
  return out.slice(0, cap);
}
