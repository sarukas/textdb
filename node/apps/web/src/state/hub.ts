import type { ChangeEvent } from "../api";

type Listener<T> = (value: T) => void;

/** A tiny synchronous event emitter. */
export class Emitter<T> {
  private listeners = new Set<Listener<T>>();

  on(fn: Listener<T>): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  emit(value: T): void {
    for (const fn of [...this.listeners]) fn(value);
  }
}

/** Fan-out point for the change feed: every panel subscribes here instead of to the SSE stream. */
export class FeedHub {
  readonly events = new Emitter<ChangeEvent>();
  /** The stream came back after an interruption. */
  readonly reconnected = new Emitter<number>();
}
