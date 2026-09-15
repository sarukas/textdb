import { describe, expect, test } from "vitest";
import { actionFor, afterChange, assetPath, inFolder, isPointer, pointerPath, previewKind, stateLabel, syncLinkFor } from "./model";

describe("asset pointers", () => {
  test("a pointer names its asset, and only a .tdbasset name is a pointer", () => {
    expect(isPointer("/img/a.png.tdbasset")).toBe(true);
    expect(isPointer("/img/a.png")).toBe(false);
    expect(isPointer(".tdbasset")).toBe(false);
    expect(assetPath("/img/a.png.tdbasset")).toBe("/img/a.png");
    expect(assetPath("/notes/a.md")).toBe("/notes/a.md");
    expect(pointerPath("/img/a.png")).toBe("/img/a.png.tdbasset");
  });

  test("a path belongs to the innermost synced folder it is in", () => {
    const links = [{ prefix: "/notes" }, { prefix: "/notes/work" }, { prefix: "/other" }];
    expect(syncLinkFor("/notes/work/img/a.png", links)?.prefix).toBe("/notes/work");
    expect(syncLinkFor("/notes/a.png", links)?.prefix).toBe("/notes");
    expect(syncLinkFor("/notesx/a.png", links)).toBeNull();
    expect(syncLinkFor("/x.png", [{ prefix: "/" }])?.prefix).toBe("/");
  });

  test("states read as what to do, and previews only what a browser shows safely", () => {
    expect(stateLabel("not-pulled")).toMatchObject({ label: "Not pulled", tone: "action" });
    expect(stateLabel("conflict").tone).toBe("problem");
    expect(stateLabel("something-new")).toMatchObject({ label: "something-new", tone: "problem" });
    expect(actionFor({ state: "outdated" })).toBe("pull");
    expect(actionFor({ state: "modified" })).toBe("push");
    expect(actionFor({ state: "conflict" })).toBeNull();
    expect(previewKind("image/png")).toBe("image");
    expect(previewKind("image/svg+xml")).toBe("download");
    expect(previewKind("application/pdf")).toBe("pdf");
    expect(previewKind("application/vnd.openxmlformats-officedocument.wordprocessingml.document")).toBe("download");
    // Only what the server sends inline: a TIFF or a QuickTime film is a download.
    expect(previewKind("image/tiff")).toBe("download");
    expect(previewKind("video/quicktime")).toBe("download");
  });

  test("an asset follows its pointer when it or a folder above it moves, and knows when it went", () => {
    const at = "/notes/img/a.png.tdbasset";
    expect(afterChange(at, { op: "move", path: "/notes/img/b.png.tdbasset", old_path: at })).toBe("/notes/img/b.png.tdbasset");
    expect(afterChange(at, { op: "move", path: "/archive/pics", old_path: "/notes/img" })).toBe("/archive/pics/a.png.tdbasset");
    expect(afterChange(at, { op: "move", path: "/x", old_path: "/notes/im" })).toBeUndefined();
    expect(afterChange(at, { op: "delete", path: "/notes", old_path: null })).toBeNull();
    expect(afterChange(at, { op: "commit", path: at, old_path: null })).toBeUndefined();
    expect(inFolder("/notes", "/notes/a")).toBe(true);
    expect(inFolder("/notes", "/notesx")).toBe(false);
    expect(inFolder("/", "/anything")).toBe(true);
  });
});
