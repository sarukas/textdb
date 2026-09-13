import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

export function tempDir(): { dir: string; remove(): void } {
  const dir = mkdtempSync(path.join(tmpdir(), 'textdb-server-'));
  // Windows keeps the WAL files locked for a moment after close.
  return { dir, remove: () => rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 50 }) };
}

export async function waitFor<T>(probe: () => T | undefined, timeoutMs = 5000): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = probe();
    if (value !== undefined) return value;
    if (Date.now() > deadline) throw new Error(`timed out after ${timeoutMs} ms`);
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

export interface SseEvent {
  id: string | undefined;
  event: string | undefined;
  data: string;
}

/** A minimal event-stream reader over fetch, collecting events and comment lines. */
export class SseClient {
  readonly events: SseEvent[] = [];
  readonly comments: string[] = [];
  private readonly abort: AbortController;
  private readonly done: Promise<void>;

  private constructor(body: ReadableStream<Uint8Array>, abort: AbortController) {
    this.abort = abort;
    this.done = this.read(body);
  }

  static async connect(url: string, headers: Record<string, string> = {}): Promise<SseClient> {
    const abort = new AbortController();
    const res = await fetch(url, { headers, signal: abort.signal });
    if (!res.ok || !res.body) throw new Error(`event stream failed: ${res.status}`);
    return new SseClient(res.body, abort);
  }

  changes<T = Record<string, unknown>>(): T[] {
    return this.events.filter((e) => e.event === 'change').map((e) => JSON.parse(e.data) as T);
  }

  async close(): Promise<void> {
    this.abort.abort();
    await this.done;
  }

  private async read(body: ReadableStream<Uint8Array>): Promise<void> {
    const decoder = new TextDecoder();
    let buffer = '';
    try {
      for await (const chunk of body) {
        buffer += decoder.decode(chunk, { stream: true });
        for (let end = buffer.indexOf('\n\n'); end >= 0; end = buffer.indexOf('\n\n')) {
          this.parse(buffer.slice(0, end));
          buffer = buffer.slice(end + 2);
        }
      }
    } catch (error) {
      if (!this.abort.signal.aborted) throw error;
    }
  }

  private parse(block: string): void {
    const event: SseEvent = { id: undefined, event: undefined, data: '' };
    const data: string[] = [];
    for (const line of block.split('\n')) {
      if (line.startsWith(':')) this.comments.push(line.slice(1).trim());
      else if (line.startsWith('id:')) event.id = line.slice(3).trim();
      else if (line.startsWith('event:')) event.event = line.slice(6).trim();
      else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
    }
    if (data.length === 0) return;
    event.data = data.join('\n');
    this.events.push(event);
  }
}
