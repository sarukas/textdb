/** A file's history as one list: its versions and the renames, moves and deletes between them. */
import type { PathEvent } from "../api";

export type TimelineItem<V> = { kind: "version"; entry: V } | { kind: "path"; event: PathEvent };

/**
 * Versions and path events in time order, oldest first. At the same instant a version comes
 * before a path event: a commit and a rename made together were made in that order.
 */
export function timeline<V extends { ts: string }>(versions: readonly V[], events: readonly PathEvent[]): TimelineItem<V>[] {
  const items: TimelineItem<V>[] = [
    ...versions.map((entry) => ({ kind: "version" as const, entry })),
    ...events.map((event) => ({ kind: "path" as const, event })),
  ];
  const ts = (i: TimelineItem<V>) => (i.kind === "version" ? i.entry.ts : i.event.ts);
  // Array.prototype.sort is stable, so ties keep versions ahead of events.
  return items.sort((a, b) => (ts(a) < ts(b) ? -1 : ts(a) > ts(b) ? 1 : 0));
}

/** "Renamed from /a/old.md", "Moved with folder /a, from /a/x.md", "Deleted". */
export function describePathEvent(e: PathEvent): string {
  if (e.op === "delete") return e.via ? `Deleted with folder ${e.via}` : "Deleted";
  const verb = e.op === "rename" ? "Renamed" : "Moved";
  return e.via ? `${verb} with folder ${e.via}, from ${e.old_path}` : `${verb} from ${e.old_path}`;
}
