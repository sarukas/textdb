import type { Text } from "@codemirror/state";
import { api, ApiError, type ChangeEvent, type Chunk, type ConflictInfo } from "../api";
import type { ChunkMark } from "../editor/chunks";
import { offsetOfLine } from "../live/changeset";
import { authorHue } from "../live/color";
import { diffLines } from "../live/diff";
import { HunkMismatchError, type Hunk } from "../live/hunks";
import { isWithin } from "../live/paths";
import { DocTracker, type RemoteApplied } from "../live/tracker";
import { Emitter, type FeedHub } from "../state/hub";
import type { OwnWrites } from "../state/ownWrites";
import type { ToastFn } from "../state/toast";

export interface FlashInfo {
  id: number;
  author: string;
  version: number;
  kind: string;
  hue: number;
  label: string;
}

export interface RemoteUpdate {
  applied: RemoteApplied;
  /** `null` for changes that should not be flashed (the echo of an own save). */
  flash: FlashInfo | null;
}

export interface ConflictState extends ConflictInfo {
  /** The full text that was sent. */
  mine: string;
}

export interface DocNotice {
  kind: "deleted" | "moved" | "overlap";
  text: string;
}

function describeLines(spans: ReadonlyArray<{ from: number; to: number }>): string {
  const parts = spans.slice(0, 3).map((s) => (s.to - s.from <= 1 ? `${s.from}` : `${s.from}–${s.to - 1}`));
  const single = spans.length === 1 && spans[0]!.to - spans[0]!.from <= 1;
  return `${single ? "line" : "lines"} ${parts.join(", ")}${spans.length > 3 ? ", …" : ""}`;
}

export interface DocState {
  path: string;
  status: "loading" | "ready" | "error";
  error: string | null;
  /** The version the base text corresponds to. */
  version: number;
  headVersion: number;
  updatedBy: string | null;
  updatedAt: string | null;
  dirty: boolean;
  saving: boolean;
  conflict: ConflictState | null;
  notice: DocNotice | null;
  chunks: Chunk[] | null;
  chunkVersion: number;
  freshChunks: ReadonlySet<string>;
  /** Bumps whenever a feed event touches this document. */
  feedRev: number;
}

export interface ControllerDeps {
  hub: FeedHub;
  own: OwnWrites;
  getAuthor: () => string;
  toast: ToastFn;
  onPathChange: (path: string) => void;
}

let flashSeq = 0;
const EMPTY = new Set<string>();

/**
 * One open document: loads it, applies remote commits from the feed (in order, one at a
 * time), saves, and resolves conflicts. Views subscribe to its state and to `remote` / `reset`.
 */
export class DocController {
  tracker: DocTracker | null = null;
  state: DocState;
  /** A remote commit (or a merged save) changed the current document. */
  readonly remote = new Emitter<RemoteUpdate>();
  /** The document was replaced wholesale (reload). */
  readonly reset = new Emitter<Text>();

  private readonly listeners = new Set<() => void>();
  private queue: Promise<void> = Promise.resolve();
  private readonly offs: Array<() => void> = [];
  private disposed = false;
  private chunksWanted = false;
  private chunkTimer: ReturnType<typeof setTimeout> | null = null;
  private freshTimer: ReturnType<typeof setTimeout> | null = null;
  private highlightNextChunks = false;
  /** Authors of versions seen on the feed, to attribute what a merged save folded in. */
  private readonly authors = new Map<number, string>();

  constructor(
    path: string,
    private readonly deps: ControllerDeps,
  ) {
    this.state = {
      path,
      status: "loading",
      error: null,
      version: 0,
      headVersion: 0,
      updatedBy: null,
      updatedAt: null,
      dirty: false,
      saving: false,
      conflict: null,
      notice: null,
      chunks: null,
      chunkVersion: -1,
      freshChunks: EMPTY,
      feedRev: 0,
    };
    this.offs.push(deps.hub.events.on((e) => this.onEvent(e)));
    this.offs.push(deps.hub.reconnected.on(() => this.enqueue(() => this.resyncToHead())));
    void this.enqueue(() => this.load());
  }

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  getState = (): DocState => this.state;

  dispose(): void {
    this.disposed = true;
    this.offs.forEach((off) => off());
    if (this.chunkTimer) clearTimeout(this.chunkTimer);
    if (this.freshTimer) clearTimeout(this.freshTimer);
    this.listeners.clear();
  }

  private set(patch: Partial<DocState>): void {
    if (this.disposed) return;
    this.state = { ...this.state, ...patch };
    for (const fn of [...this.listeners]) fn();
  }

  /** Run tasks strictly one after another: remote commits, saves and reloads never interleave. */
  private enqueue(task: () => Promise<void>): Promise<void> {
    const run = this.queue.then(async () => {
      if (this.disposed) return;
      try {
        await task();
      } catch (err) {
        this.fail(err);
      }
    });
    this.queue = run;
    return run;
  }

  private fail(err: unknown): void {
    const message = err instanceof Error ? err.message : String(err);
    if (this.state.status === "loading") this.set({ status: "error", error: message });
    else this.deps.toast(message, "error");
  }

  private async load(): Promise<void> {
    const f = await api.file(this.state.path);
    this.tracker = new DocTracker(f.content, f.version);
    this.set({
      status: "ready",
      error: null,
      version: f.version,
      headVersion: f.head_version,
      updatedBy: f.updated_by,
      updatedAt: f.updated_at,
      dirty: false,
    });
    if (this.chunksWanted) this.scheduleChunks(false, 0);
  }

  // ---- local edits ------------------------------------------------------------------------

  /** Called by the editor for every local (non-remote) transaction that changed the document. */
  userEdit(changes: import("@codemirror/state").ChangeSet): void {
    const t = this.tracker;
    if (!t) return;
    t.recordLocal(changes);
    const dirty = t.dirty;
    if (dirty !== this.state.dirty) this.set({ dirty });
  }

  // ---- feed ---------------------------------------------------------------------------------

  private onEvent(e: ChangeEvent): void {
    const path = this.state.path;
    const touches =
      e.path === path ||
      e.old_path === path ||
      (e.node_kind === "folder" && (isWithin(e.path, path) || (e.old_path !== null && isWithin(e.old_path, path))));
    if (!touches) return;
    if (e.path === path && e.version !== null && e.author) this.authors.set(e.version, e.author);
    this.set({ feedRev: this.state.feedRev + 1 });
    void this.enqueue(() => this.applyEvent(e));
  }

  private async applyEvent(e: ChangeEvent): Promise<void> {
    const t = this.tracker;
    const path = this.state.path;
    const who = e.author ?? "someone";
    if (e.op === "move" && e.old_path !== null) {
      if (e.old_path === path || isWithin(e.old_path, path)) {
        const next = e.path + path.slice(e.old_path.length);
        this.set({ path: next, notice: { kind: "moved", text: `Moved from ${path} by ${who}` } });
        this.deps.onPathChange(next);
      }
      return;
    }
    if (e.op === "delete") {
      if (e.path === path || isWithin(e.path, path)) {
        this.set({ notice: { kind: "deleted", text: `Deleted by ${who}. Saving will re-create it.` } });
      }
      return;
    }
    if ((e.op !== "commit" && e.op !== "create") || e.path !== path || e.version === null || !t) return;
    if (e.version > this.state.headVersion) {
      this.set({ headVersion: e.version, updatedBy: e.author, updatedAt: e.ts });
    }
    if (e.op === "create" && this.state.notice?.kind === "deleted") {
      // Re-created after a delete: its versions start over, so reload rather than patch.
      await this.reloadHead(false);
      return;
    }
    if (e.version <= t.version) return; // our own save, or already applied
    const hunks: Hunk[] =
      e.version === t.version + 1 && e.hunks ? e.hunks : await api.hunks(path, t.version, e.version);
    const kind = e.commit_kind ?? (e.op === "create" ? "created" : "direct");
    await this.applyRemoteHunks(hunks, e.version, who, kind);
  }

  private async applyRemoteHunks(hunks: Hunk[], version: number, author: string, kind: string): Promise<void> {
    const t = this.tracker!;
    if (version <= t.version) return;
    let applied: RemoteApplied;
    try {
      applied = t.applyRemote(hunks, version);
    } catch (err) {
      if (!(err instanceof HunkMismatchError)) throw err;
      // The base drifted from what the hunks expect: rebuild the patch from full text.
      const f = await api.file(this.state.path, version);
      if (version <= t.version) return;
      applied = t.applyRemote(diffLines(t.base.toString(), f.content), version);
    }
    const own = this.deps.own.has(this.state.path, version);
    const flash = own ? null : makeFlash(author, version, kind);
    const patch: Partial<DocState> = { version: t.version, dirty: t.dirty, headVersion: Math.max(this.state.headVersion, version) };
    if (applied.overlaps.length && !own) {
      patch.notice = {
        kind: "overlap",
        text: `${author} changed ${describeLines(applied.overlaps)} where you had unsaved edits. Both texts were kept — review before saving.`,
      };
    }
    this.set(patch);
    this.remote.emit({ applied, flash });
    this.scheduleChunks(!own);
  }

  /** After a dropped connection: catch up with HEAD if commits were missed. */
  private async resyncToHead(): Promise<void> {
    const t = this.tracker;
    if (!t) return;
    const f = await api.file(this.state.path);
    this.set({ headVersion: f.head_version });
    if (f.version <= t.version) return;
    await this.applyRemoteHunks(diffLines(t.base.toString(), f.content), f.version, f.updated_by ?? "someone", "resync");
  }

  // ---- save & conflicts --------------------------------------------------------------------

  save(opts: { overwrite?: boolean } = {}): Promise<void> {
    return this.enqueue(async () => {
      const t = this.tracker;
      if (!t) return;
      if (!t.dirty && !opts.overwrite && this.state.notice?.kind !== "deleted") {
        this.deps.toast("Nothing to save");
        return;
      }
      const path = this.state.path;
      const conflict = this.state.conflict;
      const baseVersion = opts.overwrite && conflict ? Math.max(conflict.current_version, t.version) : t.version;
      const content = t.beginSave();
      this.set({ saving: true });
      try {
        const res = await api.write({ path, content, base_version: baseVersion, author: this.deps.getAuthor() });
        this.deps.own.add(path, res.version);
        if (res.kind === "direct" || res.kind === "noop") {
          t.finishSaveExact(res.version);
        } else {
          const f = await api.file(path, res.version);
          const applied = t.finishSaveMerged(res.version, f.content);
          const others = new Set<string>();
          for (let v = baseVersion + 1; v < res.version; v++) {
            const a = this.authors.get(v);
            if (a) others.add(a);
          }
          const who = [...others].join(", ") || "store";
          const flash = applied.spans.length ? makeFlash(who, res.version, res.kind) : null;
          if (applied.overlaps.length) {
            this.set({
              notice: {
                kind: "overlap",
                text: `${who} changed ${describeLines(applied.overlaps)} where you had unsaved edits. Both texts were kept — review before saving.`,
              },
            });
          }
          this.remote.emit({ applied, flash });
        }
        this.set({
          saving: false,
          dirty: t.dirty,
          version: t.version,
          headVersion: Math.max(this.state.headVersion, t.version),
          updatedBy: this.deps.getAuthor(),
          conflict: null,
          notice: this.state.notice?.kind === "deleted" ? null : this.state.notice,
        });
        this.deps.toast(`saved v${res.version} · ${res.kind}`, "ok");
        this.scheduleChunks(false);
      } catch (err) {
        t.abortSave();
        this.set({ saving: false });
        if (err instanceof ApiError && err.status === 409) {
          const info: ConflictInfo = err.conflict ?? {
            path,
            region_line_from: 0,
            region_line_to: 0,
            base: "",
            theirs: "",
            ours: "",
            current_version: (await api.file(path).catch(() => null))?.head_version ?? t.version,
          };
          this.set({ conflict: { ...info, mine: content } });
          this.deps.toast(`conflict with v${info.current_version}`, "error");
        } else {
          throw err;
        }
      }
    });
  }

  /** Discard local changes and load HEAD. */
  reloadTheirs(): Promise<void> {
    return this.enqueue(() => this.reloadHead(true));
  }

  private async reloadHead(announce: boolean): Promise<void> {
    const t = this.tracker;
    if (!t) return;
    const f = await api.file(this.state.path);
    t.reset(f.content, f.version);
    this.set({
      version: f.version,
      headVersion: f.head_version,
      updatedBy: f.updated_by,
      updatedAt: f.updated_at,
      dirty: false,
      conflict: null,
      notice: null,
    });
    this.reset.emit(t.current);
    if (announce) this.deps.toast(`reloaded v${f.version}`);
    this.scheduleChunks(false);
  }

  dismissConflict(): void {
    this.set({ conflict: null });
  }

  dismissNotice(): void {
    this.set({ notice: null });
  }

  // ---- chunks -------------------------------------------------------------------------------

  setChunksEnabled(on: boolean): void {
    this.chunksWanted = on;
    const t = this.tracker;
    if (on && t && this.state.chunkVersion !== t.version) this.scheduleChunks(false, 0);
  }

  private scheduleChunks(highlight: boolean, delay = 250): void {
    this.highlightNextChunks ||= highlight;
    if (!this.chunksWanted) return;
    if (this.chunkTimer) clearTimeout(this.chunkTimer);
    this.chunkTimer = setTimeout(() => void this.fetchChunks(), delay);
  }

  private async fetchChunks(): Promise<void> {
    const t = this.tracker;
    if (!t || this.disposed) return;
    const version = t.version;
    const path = this.state.path;
    let list: Chunk[];
    try {
      list = await api.chunks(path, version);
    } catch (err) {
      this.deps.toast(`chunks: ${err instanceof Error ? err.message : String(err)}`, "error");
      return;
    }
    if (t.version !== version || this.state.path !== path) return; // a newer fetch is scheduled
    const highlight = this.highlightNextChunks;
    this.highlightNextChunks = false;
    const prev = new Set((this.state.chunks ?? []).map((c) => c.hash));
    const fresh = highlight && this.state.chunks ? new Set(list.filter((c) => !prev.has(c.hash)).map((c) => c.hash)) : EMPTY;
    this.set({ chunks: list, chunkVersion: version, freshChunks: fresh });
    if (this.freshTimer) clearTimeout(this.freshTimer);
    if (fresh.size) this.freshTimer = setTimeout(() => this.set({ freshChunks: EMPTY }), 6000);
  }

  /** Chunk boundaries placed in the current document, or `null` when the listing is stale. */
  chunkMarks(): ChunkMark[] | null {
    const t = this.tracker;
    const s = this.state;
    if (!t || !s.chunks || s.chunkVersion !== t.version) return null;
    return s.chunks.map((c) => ({
      ord: c.ord,
      hash: c.hash,
      fresh: s.freshChunks.has(c.hash),
      pos: t.mapBasePos(offsetOfLine(t.base, Math.max(1, c.line_from)), 1),
    }));
  }
}

function makeFlash(author: string, version: number, kind: string): FlashInfo {
  return {
    id: ++flashSeq,
    author,
    version,
    kind,
    hue: authorHue(author),
    label: `${author} · v${version} · ${kind}`,
  };
}
