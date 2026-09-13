import { describe, expect, it } from "vitest";
import { applyHunks, HunkMismatchError, mapLine, type Hunk } from "./hunks";
import { diffLines } from "./diff";
import { countLines, lineStarts, splitLines } from "./lines";

function rng(seed: number) {
  let s = seed >>> 0;
  return () => {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    return s / 2 ** 32;
  };
}

function mutate(text: string, rand: () => number): string {
  const lines = splitLines(text);
  const edits = 1 + Math.floor(rand() * 5);
  for (let e = 0; e < edits; e++) {
    const at = Math.floor(rand() * (lines.length + 1));
    const op = rand();
    if (op < 0.33 && lines.length > 0) lines.splice(at, 1 + Math.floor(rand() * 3));
    else if (op < 0.66) lines.splice(at, 0, `inserted ${Math.floor(rand() * 1000)}\n`);
    else if (lines.length > 0) lines[Math.min(at, lines.length - 1)] = `changed ${Math.floor(rand() * 1000)}\n`;
  }
  if (rand() < 0.2 && lines.length > 0) {
    const last = lines.length - 1;
    lines[last] = lines[last]!.replace(/\n$/, "");
  }
  return lines.join("");
}

describe("lines", () => {
  it("follows the store's line model", () => {
    expect(splitLines("a\nb\n")).toEqual(["a\n", "b\n"]);
    expect(splitLines("a\nb")).toEqual(["a\n", "b"]);
    expect(splitLines("")).toEqual([]);
    expect(lineStarts("a\nbb\n")).toEqual([0, 2, 5]);
    expect(countLines("a\nb")).toBe(2);
    expect(countLines("\n")).toBe(1);
    expect(countLines("")).toBe(0);
  });
});

describe("applyHunks", () => {
  // The hunks the store returns in crates/textdb-sqlite/tests/live.rs for the same edit.
  it("applies a commit's hunks to the old text and yields the new version exactly", () => {
    const body = Array.from({ length: 400 }, (_, i) => `line ${i} of a document long enough to span many chunks\n`).join("");
    const edited =
      body
        .replace("line 10 of", "LINE TEN of")
        .replace("line 300 of a document long enough to span many chunks\n", "") + "appended\n";
    const hunks: Hunk[] = [
      {
        old_from: 11,
        old_count: 1,
        new_from: 11,
        new_count: 1,
        old_text: "line 10 of a document long enough to span many chunks\n",
        new_text: "LINE TEN of a document long enough to span many chunks\n",
      },
      {
        old_from: 301,
        old_count: 1,
        new_from: 300,
        new_count: 0,
        old_text: "line 300 of a document long enough to span many chunks\n",
        new_text: "",
      },
      { old_from: 401, old_count: 0, new_from: 400, new_count: 1, old_text: "", new_text: "appended\n" },
    ];
    expect(applyHunks(body, hunks)).toBe(edited);
    // Order of the hunk list does not matter.
    expect(applyHunks(body, [...hunks].reverse())).toBe(edited);
  });

  it("treats version 0 as the empty document", () => {
    const text = "# Title\n\nbody\n";
    expect(applyHunks("", [{ old_from: 1, old_count: 0, new_from: 1, new_count: 3, old_text: "", new_text: text }])).toBe(text);
  });

  it("handles a last line without a newline", () => {
    const hunks: Hunk[] = [{ old_from: 2, old_count: 1, new_from: 2, new_count: 2, old_text: "b", new_text: "b\nc\n" }];
    expect(applyHunks("a\nb", hunks)).toBe("a\nb\nc\n");
  });

  it("rejects hunks that do not match the text", () => {
    const bad: Hunk[] = [{ old_from: 1, old_count: 1, new_from: 1, new_count: 1, old_text: "x\n", new_text: "y\n" }];
    expect(() => applyHunks("a\n", bad)).toThrow(HunkMismatchError);
    const outside: Hunk[] = [{ old_from: 5, old_count: 1, new_from: 5, new_count: 0, old_text: "a\n", new_text: "" }];
    expect(() => applyHunks("a\n", outside)).toThrow(HunkMismatchError);
  });

  it("round-trips random edits through diffLines", () => {
    const rand = rng(42);
    let text = Array.from({ length: 60 }, (_, i) => `row ${i}\n`).join("");
    for (let round = 0; round < 300; round++) {
      const next = mutate(text, rand);
      const hunks = diffLines(text, next);
      expect(applyHunks(text, hunks)).toBe(next);
      for (const h of hunks) {
        expect(countLines(h.old_text)).toBe(h.old_count);
        expect(countLines(h.new_text)).toBe(h.new_count);
      }
      text = next;
    }
  });

  it("maps lines through hunks", () => {
    const hunks: Hunk[] = [
      { old_from: 3, old_count: 1, new_from: 3, new_count: 3, old_text: "c\n", new_text: "c\nd\ne\n" },
      { old_from: 6, old_count: 2, new_from: 8, new_count: 0, old_text: "f\ng\n", new_text: "" },
    ];
    expect(mapLine(1, hunks)).toBe(1);
    expect(mapLine(3, hunks)).toBe(3);
    expect(mapLine(4, hunks)).toBe(6);
    expect(mapLine(7, hunks)).toBe(8);
    expect(mapLine(8, hunks)).toBe(8);
  });
});

describe("diffLines", () => {
  it("returns no hunks for equal texts and tight hunks otherwise", () => {
    expect(diffLines("a\nb\n", "a\nb\n")).toEqual([]);
    expect(diffLines("a\nb\nc\n", "a\nB\nc\n")).toEqual([
      { old_from: 2, old_count: 1, new_from: 2, new_count: 1, old_text: "b\n", new_text: "B\n" },
    ]);
    expect(diffLines("a\nc\n", "a\nb\nc\n")).toEqual([
      { old_from: 2, old_count: 0, new_from: 2, new_count: 1, old_text: "", new_text: "b\n" },
    ]);
  });
});
