import { ChangeSet } from "@codemirror/state";
import { describe, expect, it } from "vitest";
import { changedLineSpans, hunksToChangeSet, rebase } from "./changeset";
import { diffLines } from "./diff";
import { applyHunks, HunkMismatchError, type Hunk } from "./hunks";
import { DocTracker, textOf } from "./tracker";

const base = "# Title\n\nalpha\nbeta\ngamma\n\n## Notes\n\ndelta\n";

describe("hunksToChangeSet", () => {
  it("produces the same text as applying the hunks to the string", () => {
    const next = "# Title\n\nalpha\nBETA\nBETA 2\ngamma\n\n## Notes\n\ndelta\nepsilon\n";
    const hunks = diffLines(base, next);
    const cs = hunksToChangeSet(textOf(base), hunks);
    expect(cs.apply(textOf(base)).toString()).toBe(next);
    expect(applyHunks(base, hunks)).toBe(next);
  });

  it("works on an unterminated last line and the empty document", () => {
    const cs = hunksToChangeSet(textOf("a\nb"), [
      { old_from: 2, old_count: 1, new_from: 2, new_count: 2, old_text: "b", new_text: "b\nc\n" },
    ]);
    expect(cs.apply(textOf("a\nb")).toString()).toBe("a\nb\nc\n");
    const first = hunksToChangeSet(textOf(""), [
      { old_from: 1, old_count: 0, new_from: 1, new_count: 1, old_text: "", new_text: "hello\n" },
    ]);
    expect(first.apply(textOf("")).toString()).toBe("hello\n");
  });

  it("rejects stale hunks", () => {
    const stale: Hunk[] = [{ old_from: 3, old_count: 1, new_from: 3, new_count: 1, old_text: "ALPHA\n", new_text: "x\n" }];
    expect(() => hunksToChangeSet(textOf(base), stale)).toThrow(HunkMismatchError);
  });
});

describe("rebase over unsaved local edits", () => {
  it("preserves both the remote commit and the local edits", () => {
    const baseDoc = textOf(base);
    // Local: the user typed at the end of "alpha" and added a line after "delta".
    const alphaEnd = base.indexOf("alpha") + "alpha".length;
    const local = ChangeSet.of(
      [
        { from: alphaEnd, insert: " (mine)" },
        { from: base.length, insert: "my line\n" },
      ],
      baseDoc.length,
    );
    const current = local.apply(baseDoc);
    // Remote: the agent rewrote "gamma" and inserted a heading line at the top.
    const remoteText = "<!-- agent -->\n# Title\n\nalpha\nbeta\nGAMMA\n\n## Notes\n\ndelta\n";
    const remote = hunksToChangeSet(baseDoc, diffLines(base, remoteText));

    const r = rebase(local, remote);
    const viaCurrent = r.remote.apply(current).toString();
    const viaNewBase = r.local.apply(remote.apply(baseDoc)).toString();
    expect(viaCurrent).toBe(viaNewBase);
    expect(viaCurrent).toBe("<!-- agent -->\n# Title\n\nalpha (mine)\nbeta\nGAMMA\n\n## Notes\n\ndelta\nmy line\n");
  });

  it("keeps a local insertion in front of a remote one at the same spot", () => {
    const doc = textOf("a\nb\n");
    const local = ChangeSet.of([{ from: 2, insert: "L\n" }], doc.length);
    const remote = hunksToChangeSet(doc, [{ old_from: 2, old_count: 0, new_from: 2, new_count: 1, old_text: "", new_text: "R\n" }]);
    const r = rebase(local, remote);
    expect(r.remote.apply(local.apply(doc)).toString()).toBe("a\nL\nR\nb\n");
    expect(r.local.apply(remote.apply(doc)).toString()).toBe("a\nL\nR\nb\n");
  });

  it("DocTracker applies consecutive remote commits over continuing local typing", () => {
    const t = new DocTracker(base, 3);
    const type = (at: number, text: string) => t.recordLocal(ChangeSet.of([{ from: at, insert: text }], t.current.length));
    type(t.current.toString().indexOf("beta") + 4, "!");
    let serverText = base;
    const commit = (next: string, version: number) => {
      const hunks = diffLines(serverText, next);
      serverText = next;
      return t.applyRemote(hunks, version);
    };
    const applied = commit(base.replace("delta", "delta (agent)"), 4);
    expect(t.version).toBe(4);
    // "delta" is line 9 of the document.
    expect(applied.spans).toEqual([{ from: 9, to: 10 }]);
    expect(t.current.toString()).toBe(serverText.replace("beta", "beta!"));
    type(0, "> ");
    commit(serverText.replace("## Notes", "## Notes (v5)"), 5);
    expect(t.base.toString()).toBe(serverText);
    expect(t.local.apply(t.base).toString()).toBe(t.current.toString());
    expect(t.current.toString()).toBe("> " + serverText.replace("beta", "beta!"));
    expect(t.dirty).toBe(true);
  });

  it("DocTracker reports a remote change that lands on lines the user is editing", () => {
    const t = new DocTracker("a\nb\nc\n", 1);
    // Local, unsaved: rewrite "b".
    t.recordLocal(ChangeSet.of([{ from: 2, to: 3, insert: "B-local" }], t.current.length));
    // Remote: a different rewrite of the same line.
    const clash = t.applyRemote(
      [{ old_from: 2, old_count: 1, new_from: 2, new_count: 1, old_text: "b\n", new_text: "b-remote\n" }],
      2,
    );
    expect(clash.overlaps).toHaveLength(1);
    expect(clash.overlaps[0]!.from).toBe(2);
    // Both texts are kept, so nothing is lost; the overlap tells the UI to ask the user to look.
    expect(t.current.toString()).toContain("B-local");
    expect(t.current.toString()).toContain("b-remote");

    // A remote change elsewhere is not an overlap.
    const apart = t.applyRemote(
      [{ old_from: 3, old_count: 1, new_from: 3, new_count: 1, old_text: "c\n", new_text: "C\n" }],
      3,
    );
    expect(apart.overlaps).toEqual([]);
  });

  it("DocTracker folds a merged save back in, keeping typing done during the request", () => {
    const t = new DocTracker(base, 1);
    t.recordLocal(ChangeSet.of([{ from: 0, insert: "mine\n" }], t.current.length));
    const sent = t.beginSave();
    expect(sent).toBe("mine\n" + base);
    // The user keeps typing while the request is in flight.
    t.recordLocal(ChangeSet.of([{ from: t.current.length, insert: "typing\n" }], t.current.length));
    // Meanwhile the agent had changed "gamma"; the store merged both into version 3.
    const server = "mine\n" + base.replace("gamma", "GAMMA");
    const applied = t.finishSaveMerged(3, server);
    expect(t.version).toBe(3);
    expect(t.base.toString()).toBe(server);
    expect(t.current.toString()).toBe(server + "typing\n");
    expect(applied.spans).toEqual([{ from: 6, to: 7 }]);
    expect(t.dirty).toBe(true);
  });

  it("DocTracker makes the snapshot the base after a direct save", () => {
    const t = new DocTracker("x\n", 1);
    t.recordLocal(ChangeSet.of([{ from: 2, insert: "y\n" }], 2));
    t.beginSave();
    t.finishSaveExact(2);
    expect(t.dirty).toBe(false);
    expect(t.base.toString()).toBe("x\ny\n");
  });
});

describe("changedLineSpans", () => {
  it("reports inserted, modified and deleted lines of the new document", () => {
    const doc = textOf("a\nb\nc\nd\n");
    const cs = ChangeSet.of(
      [
        { from: 0, insert: "new\n" }, // whole-line insert before line 1
        { from: 3, to: 3, insert: "B" }, // inside line "b"
        { from: 4, to: 6 }, // delete line "c"
      ],
      doc.length,
    );
    const after = cs.apply(doc);
    expect(after.toString()).toBe("new\na\nbB\nd\n");
    expect(changedLineSpans(cs, after)).toEqual([
      { from: 1, to: 2 },
      { from: 3, to: 4 },
      { from: 4, to: 4 },
    ]);
  });
});
