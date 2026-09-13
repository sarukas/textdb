import { describe, expect, it } from "vitest";
import type { ImportResult } from "../api";
import { runImport, type ImportLimits, type ImportProgress } from "./runner";
import type { PickedFile } from "./source";

const text = (rel: string, content = `# ${rel}\n`): PickedFile => ({
  rel,
  size: null,
  open: async () => new File([content], rel),
});

type Body = { author?: string; files: { path: string; content: string }[] };

function recorder(fail?: (call: number) => Error | null) {
  const calls: Body[] = [];
  const send = async (body: Body): Promise<ImportResult> => {
    calls.push(body);
    const error = fail?.(calls.length);
    if (error) throw error;
    return { created: body.files.length, updated: 0, unchanged: 0, failed: 0, failures: [] };
  };
  return { calls, send };
}

function run(files: PickedFile[], send: (b: Body) => Promise<ImportResult>, limits: Partial<ImportLimits> = {}, stop = () => false) {
  const seen: ImportProgress[] = [];
  return runImport({
    files,
    prefix: "/kb",
    author: "tester",
    onProgress: (p) => seen.push(p),
    shouldStop: stop,
    limits,
    send,
  }).then((final) => ({ final, seen }));
}

describe("runImport", () => {
  it("sends every file once, in batches of at most batchFiles", async () => {
    const files = Array.from({ length: 1234 }, (_, i) => text(`d/${i}.md`));
    const { calls, send } = recorder();
    const { final } = await run(files, send, { batchFiles: 100 });
    expect(final.state).toBe("done");
    expect(final.doneFiles).toBe(1234);
    expect(final.created).toBe(1234);
    expect(calls.every((c) => c.files.length <= 100 && c.author === "tester")).toBe(true);
    const paths = calls.flatMap((c) => c.files.map((f) => f.path)).sort();
    expect(paths).toEqual(files.map((f) => `/kb/${f.rel}`).sort());
  });

  it("starts a new batch before it would exceed batchBytes", async () => {
    const files = Array.from({ length: 10 }, (_, i) => text(`${i}.md`, "x".repeat(40)));
    const { calls, send } = recorder();
    await run(files, send, { batchBytes: 100, readers: 1 });
    expect(calls.map((c) => c.files.length)).toEqual([2, 2, 2, 2, 2]);
  });

  it("skips files that cannot be opened, time out, are binary or too large", async () => {
    const files: PickedFile[] = [
      text("ok.md"),
      { rel: "offline.md", size: null, open: () => Promise.reject(new DOMException("could not read", "NotReadableError")) },
      { rel: "hangs.md", size: null, open: () => new Promise<File>(() => {}) },
      { rel: "bin.md", size: null, open: async () => new File([new Uint8Array([35, 0, 1])], "bin.md") },
      text("big.md", "y".repeat(500)),
    ];
    const { calls, send } = recorder();
    const { final } = await run(files, send, { readTimeoutMs: 30, maxFileBytes: 100 });
    expect(final.state).toBe("done");
    expect(final.doneFiles).toBe(5);
    expect(final.created).toBe(1);
    expect(final.skipped).toBe(4);
    expect(calls.flatMap((c) => c.files.map((f) => f.path))).toEqual(["/kb/ok.md"]);
    const reasons = Object.fromEntries(final.failures.map((f) => [f.path, f.reason]));
    expect(reasons["/kb/offline.md"]).toBe("skipped: could not be read: NotReadableError: could not read");
    expect(reasons["/kb/hangs.md"]).toBe("skipped: not read within 30 ms");
    expect(reasons["/kb/bin.md"]).toBe("skipped: binary file");
    expect(reasons["/kb/big.md"]).toBe("skipped: larger than 100 B");
  });

  it("stops at the first failed request", async () => {
    const files = Array.from({ length: 50 }, (_, i) => text(`${i}.md`));
    const { calls, send } = recorder((n) => (n === 2 ? new Error("server went away") : null));
    const { final } = await run(files, send, { batchFiles: 10 });
    expect(final.state).toBe("error");
    expect(final.error).toBe("server went away");
    expect(final.doneFiles).toBe(10);
    expect(calls).toHaveLength(2);
  });

  it("cancels between files and reports what was sent", async () => {
    const files = Array.from({ length: 100 }, (_, i) => text(`${i}.md`));
    const { send } = recorder();
    let reads = 0;
    const counted = files.map((f) => ({ ...f, open: () => (reads++, f.open()) }));
    const { final } = await run(counted, send, { batchFiles: 10, readers: 1 }, () => reads >= 25);
    expect(final.state).toBe("cancelled");
    // The five files read after the second request are dropped, not sent.
    expect(final.doneFiles).toBe(20);
    expect(reads).toBe(25);
  });
});
