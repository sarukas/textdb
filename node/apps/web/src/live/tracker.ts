import { ChangeSet, Text } from "@codemirror/state";
import { changedLineSpans, hunksToChangeSet, rebase, type LineSpan } from "./changeset";
import { diffLines } from "./diff";
import type { Hunk } from "./hunks";

export function textOf(s: string): Text {
  return Text.of(s.split("\n"));
}

export interface RemoteApplied {
  /** Change to dispatch on the current document. */
  changes: ChangeSet;
  /** The current document afterwards. */
  doc: Text;
  /** Lines of `doc` the remote change touched. */
  spans: LineSpan[];
  /**
   * Lines of `doc` where the remote change landed on text the user had edited locally. Both
   * texts are kept (character-level rebase), so the result may read oddly and needs a look.
   */
  overlaps: LineSpan[];
}

interface PendingSave {
  snapshot: Text;
  /** User changes made after the snapshot was taken: snapshot → current. */
  later: ChangeSet;
}

/**
 * Where `remote` (over the same base as `local`) touches text `local` changed, as line spans
 * of `doc`, the document after both. `localAfter` is `local` rebased over `remote`.
 */
function overlapSpans(local: ChangeSet, remote: ChangeSet, localAfter: ChangeSet, doc: Text): LineSpan[] {
  if (local.empty) return [];
  const spans: LineSpan[] = [];
  remote.iterChangedRanges((fromA, toA, fromB, toB) => {
    if (!local.touchesRange(fromA, toA)) return;
    const from = Math.min(localAfter.mapPos(fromB, -1), doc.length);
    const to = Math.min(Math.max(localAfter.mapPos(toB, 1), from), doc.length);
    const first = doc.lineAt(from).number;
    const end = doc.lineAt(to);
    const last = to > from && end.from === to ? end.number - 1 : end.number;
    const span = { from: first, to: Math.max(last, first) + 1 };
    const prev = spans[spans.length - 1];
    if (prev && span.from <= prev.to) prev.to = Math.max(prev.to, span.to);
    else spans.push(span);
  });
  return spans;
}

/**
 * The state of an open document: the base text (what version `version` holds), the local
 * unsaved changes over it, and the document the user sees. Pure; the editor feeds it every
 * local transaction and dispatches whatever it returns for remote ones.
 */
export class DocTracker {
  base: Text;
  local: ChangeSet;
  current: Text;
  version: number;
  private pending: PendingSave | null = null;

  constructor(text: string, version: number) {
    this.base = textOf(text);
    this.current = this.base;
    this.local = ChangeSet.empty(this.base.length);
    this.version = version;
  }

  reset(text: string, version: number): void {
    this.base = textOf(text);
    this.current = this.base;
    this.local = ChangeSet.empty(this.base.length);
    this.version = version;
    this.pending = null;
  }

  get dirty(): boolean {
    if (this.local.empty) return false;
    return !(this.current.length === this.base.length && this.current.eq(this.base));
  }

  get saving(): boolean {
    return this.pending !== null;
  }

  /** A local edit: `changes` apply to the current document. */
  recordLocal(changes: ChangeSet): void {
    this.local = this.local.compose(changes);
    this.current = changes.apply(this.current);
    if (this.pending) this.pending.later = this.pending.later.compose(changes);
  }

  /** A remote commit, as hunks over the base text, producing version `version`. */
  applyRemote(hunks: readonly Hunk[], version: number): RemoteApplied {
    const remote = hunksToChangeSet(this.base, hunks);
    const localBefore = this.local;
    const r = rebase(localBefore, remote);
    this.base = remote.apply(this.base);
    this.local = r.local;
    this.current = r.remote.apply(this.current);
    this.version = version;
    return {
      changes: r.remote,
      doc: this.current,
      spans: changedLineSpans(r.remote, this.current),
      overlaps: overlapSpans(localBefore, remote, r.local, this.current),
    };
  }

  /** Take the text to send in a save. Local edits may continue while the request runs. */
  beginSave(): string {
    this.pending = { snapshot: this.current, later: ChangeSet.empty(this.current.length) };
    return this.current.toString();
  }

  abortSave(): void {
    this.pending = null;
  }

  /** The store kept the snapshot as-is as `version` (kind `direct` or `noop`). */
  finishSaveExact(version: number): void {
    const p = this.requirePending();
    this.base = p.snapshot;
    this.local = p.later;
    this.version = version;
    this.pending = null;
  }

  /**
   * The store merged the snapshot with commits that landed meanwhile; `serverText` is what
   * `version` holds. The difference is applied like a remote commit.
   */
  finishSaveMerged(version: number, serverText: string): RemoteApplied {
    const p = this.requirePending();
    const hunks = diffLines(p.snapshot.toString(), serverText);
    const remote = hunksToChangeSet(p.snapshot, hunks);
    const r = rebase(p.later, remote);
    this.base = remote.apply(p.snapshot);
    this.local = r.local;
    this.current = r.remote.apply(this.current);
    this.version = version;
    this.pending = null;
    return {
      changes: r.remote,
      doc: this.current,
      spans: changedLineSpans(r.remote, this.current),
      overlaps: overlapSpans(p.later, remote, r.local, this.current),
    };
  }

  /** Map a position in the base text to the current document. */
  mapBasePos(pos: number, assoc: -1 | 1 = -1): number {
    return this.local.mapPos(pos, assoc);
  }

  private requirePending(): PendingSave {
    if (!this.pending) throw new Error("no save in progress");
    return this.pending;
  }
}
