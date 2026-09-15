import { describe, expect, it } from "vitest";
import { applySuggestion, isRunnable, quoteIfNeeded, slotAt } from "./query";

/** `|` marks the caret, which keeps these cases readable. */
function at(withCaret: string) {
  const caret = withCaret.indexOf("|");
  return slotAt(withCaret.replace("|", ""), caret);
}

describe("slotAt", () => {
  it("offers property names while one is being typed", () => {
    const s = at("sta|");
    expect(s.kind).toBe("key");
    expect(s.token).toBe("sta");
  });

  it("switches to values once the colon is there, and says which key", () => {
    const s = at("status:|");
    expect(s.kind).toBe("value");
    expect(s.key).toBe("status");
    expect(s.token).toBe("");
  });

  it("narrows values by what has been typed", () => {
    const s = at("status:dr|");
    expect(s).toMatchObject({ kind: "value", key: "status", token: "dr" });
  });

  it("keeps an operator so completing the value does not lose it", () => {
    const s = at("priority:>=|");
    expect(s).toMatchObject({ kind: "value", key: "priority", operator: ">=", token: "" });
  });

  it("reads the key of the term the caret is in, not the first in the query", () => {
    const s = at("status:draft tags:tel|");
    expect(s).toMatchObject({ kind: "value", key: "tags", token: "tel" });
  });

  it("treats a leading minus as negation rather than part of the name", () => {
    const s = at("-stat|");
    expect(s.kind).toBe("key");
    expect(s.token).toBe("stat");
  });

  it("completes the property after has:", () => {
    const s = at("has:bud|");
    expect(s.kind).toBe("key");
    expect(s.token).toBe("bud");
  });

  it("stays inside one token across a quoted span with spaces", () => {
    const s = at('title:"quarterly rev|"');
    expect(s).toMatchObject({ kind: "value", key: "title", token: "quarterly rev", quoted: true });
  });

  it("offers a connective in the gap after a finished term", () => {
    const s = at("status:draft |");
    expect(s.kind).toBe("connective");
    expect(s.token).toBe("");
  });

  it("does not run past a bracket when finding the word", () => {
    const s = at("(status:dr|)");
    expect(s).toMatchObject({ kind: "value", key: "status", token: "dr" });
  });
});

describe("applySuggestion", () => {
  it("adds the colon when completing a property, leaving the caret ready for a value", () => {
    const text = "sta";
    const got = applySuggestion(text, slotAt(text, 3), "status", { addColon: true });
    expect(got.text).toBe("status:");
    expect(got.caret).toBe(7);
  });

  it("replaces only the term the caret is in", () => {
    const text = "status:draft tags:tel";
    const got = applySuggestion(text, slotAt(text, text.length), "telco");
    expect(got.text).toBe("status:draft tags:telco");
  });

  it("keeps the operator when completing a compared value", () => {
    const text = "priority:>=";
    const got = applySuggestion(text, slotAt(text, text.length), "3");
    expect(got.text).toBe("priority:>=3");
  });

  it("quotes a value that would otherwise parse as two terms", () => {
    const text = "title:";
    const got = applySuggestion(text, slotAt(text, text.length), "quarterly review");
    expect(got.text).toBe('title:"quarterly review"');
  });
});

describe("quoteIfNeeded", () => {
  it("leaves a plain value alone and quotes one with a space or a colon", () => {
    expect(quoteIfNeeded("draft")).toBe("draft");
    expect(quoteIfNeeded("in review")).toBe('"in review"');
    expect(quoteIfNeeded("a:b")).toBe('"a:b"');
  });
});

describe("isRunnable", () => {
  it("treats an empty query as runnable, because everything is a real answer", () => {
    expect(isRunnable("")).toBe(true);
    expect(isRunnable("   ")).toBe(true);
  });

  it("holds off on the half-typed states rather than showing an error per keystroke", () => {
    expect(isRunnable("status:")).toBe(false);
    expect(isRunnable("status:draft AND")).toBe(false);
    expect(isRunnable("status:draft -")).toBe(false);
    expect(isRunnable('title:"half')).toBe(false);
    expect(isRunnable("(status:draft")).toBe(false);
  });

  it("waits for a half-typed property name rather than sending a query the grammar rejects", () => {
    // This is what typing `status` one letter at a time looks like; each of these would have
    // been a 400 and a red message under the box.
    expect(isRunnable("stat")).toBe(false);
    expect(isRunnable("status:draft ta")).toBe(false);
    expect(isRunnable("has:")).toBe(false);
    expect(isRunnable("-stat")).toBe(false);
  });

  it("runs a complete query", () => {
    expect(isRunnable("status:draft")).toBe(true);
    expect(isRunnable("(status:draft OR status:review) has:budget")).toBe(true);
    expect(isRunnable('title:"quarterly review"')).toBe(true);
    expect(isRunnable("status:draft AND tags:telco")).toBe(true);
    expect(isRunnable("has:budget")).toBe(true);
  });
});
