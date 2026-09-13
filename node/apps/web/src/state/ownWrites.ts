import { Emitter } from "./hub";

/** Versions this browser tab committed, so their echo on the feed is not treated as remote. */
export class OwnWrites {
  private readonly seen = new Map<string, Set<number>>();
  readonly changed = new Emitter<void>();

  add(path: string, version: number): void {
    let s = this.seen.get(path);
    if (!s) this.seen.set(path, (s = new Set()));
    s.add(version);
    this.changed.emit();
  }

  has(path: string, version: number | null | undefined): boolean {
    return version !== null && version !== undefined && (this.seen.get(path)?.has(version) ?? false);
  }
}
