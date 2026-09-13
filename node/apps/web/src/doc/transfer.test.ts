import { describe, expect, it } from "vitest";
import { lineChange } from "./transfer";

describe("lineChange", () => {
  it("counts lines added and removed", () => {
    expect(lineChange("a\nb\nc\n", "a\nB\nc\nd\n")).toEqual({ added: 2, removed: 1 });
    expect(lineChange("same\n", "same\n")).toEqual({ added: 0, removed: 0 });
    expect(lineChange("", "one\ntwo\n")).toEqual({ added: 2, removed: 0 });
  });
});
