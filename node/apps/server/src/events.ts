import type { Context } from 'hono';
import { streamSSE } from 'hono/streaming';
import type { ChangeHub } from './hub.ts';
import { parseInteger, queryInt } from './params.ts';

/** `Last-Event-ID` wins over `?since`: a reconnecting EventSource repeats the original URL. */
function startSeq(c: Context): number | undefined {
  const lastEventId = c.req.header('Last-Event-ID');
  if (lastEventId) return parseInteger(lastEventId, 'Last-Event-ID');
  return queryInt(c, 'since');
}

export function eventStream(c: Context, hub: ChangeHub, pingMs: number): Response {
  const since = startSeq(c);
  return streamSSE(c, async (stream) => {
    const abort = new AbortController();
    stream.onAbort(() => abort.abort());
    const ping = setInterval(() => void stream.write(': ping\n\n'), pingMs);
    try {
      // Flushes the headers, so the client sees the stream open before the first change.
      await stream.write(': connected\n\n');
      for await (const event of hub.changes(since, abort.signal)) {
        await stream.writeSSE({ id: String(event.seq), event: 'change', data: JSON.stringify(event) });
      }
    } finally {
      clearInterval(ping);
    }
  });
}
