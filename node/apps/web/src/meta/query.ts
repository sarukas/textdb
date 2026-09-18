/**
 * Reading a half-typed property query well enough to suggest the next thing.
 *
 * The server owns the grammar; this is only about the caret. To offer the right list the UI
 * has to know which part of which term the caret sits in — a property name, a value, or the
 * gap between terms where a connective belongs — and which key a value belongs to, since
 * `status:dr` should suggest the values of `status` and nothing else.
 */

export type SlotKind = "key" | "value" | "connective";

export interface Slot {
  kind: SlotKind;
  /** The text being completed, which is what a suggestion replaces. */
  token: string;
  /** Caret-relative span of `token` in the query. */
  from: number;
  to: number;
  /** For a value slot, the property whose values to offer. */
  key?: string;
  /** For a value slot, an operator already typed (`>`, `>=`, `~`), which a suggestion keeps. */
  operator?: string;
  /** True when the token is inside quotes, so a suggestion must not re-quote it. */
  quoted?: boolean;
}

const CONNECTIVES = ["AND", "OR", "NOT"];

/** Is `c` a character that ends a bare word? */
function breaks(c: string): boolean {
  return c === " " || c === "\t" || c === "(" || c === ")";
}

/**
 * The words of a query, as `[from, to)` spans, with quoted runs kept whole.
 *
 * The same scan the server's lexer does: a quote opens a span that spaces cannot break, so
 * `title:"quarterly review"` is one token rather than two.
 */
function tokens(text: string): { from: number; to: number }[] {
  const out: { from: number; to: number }[] = [];
  let i = 0;
  while (i < text.length) {
    if (breaks(text[i]!)) {
      i++;
      continue;
    }
    const from = i;
    let quoted = false;
    while (i < text.length) {
      const c = text[i]!;
      if (c === '"') {
        quoted = !quoted;
        i++;
        continue;
      }
      if (!quoted && breaks(c)) break;
      i++;
    }
    out.push({ from, to: i });
  }
  return out;
}

/**
 * The word the caret sits in, as `[from, to)`.
 *
 * Walking backwards from the caret cannot work: inside `title:"quarterly rev|"` the space
 * before `rev` looks like a word break, because nothing seen so far says a quote is open.
 */
function wordAt(text: string, caret: number): { from: number; to: number } {
  for (const t of tokens(text)) {
    if (caret >= t.from && caret <= t.to) return t;
  }
  return { from: caret, to: caret };
}

/** Strip a leading `-`, which is negation rather than part of the name. */
function withoutNegation(token: string, from: number): { token: string; from: number } {
  return token.startsWith("-") ? { token: token.slice(1), from: from + 1 } : { token, from };
}

/** Split `key:>=value` into its parts, keeping any operator so a suggestion preserves it. */
function splitTerm(token: string): { key: string; operator: string; value: string } | null {
  const colon = token.indexOf(":");
  if (colon < 0) return null;
  const key = token.slice(0, colon);
  const rest = token.slice(colon + 1);
  const m = /^(>=|<=|!=|[><~])?(.*)$/s.exec(rest);
  return { key, operator: m?.[1] ?? "", value: m?.[2] ?? "" };
}

/**
 * What the caret is completing.
 *
 * `has:budget` is treated as a key slot whose token is the property, because that is what the
 * user is choosing there — the suggestions should be property names, not values.
 */
export function slotAt(text: string, caret: number): Slot {
  const at = Math.max(0, Math.min(caret, text.length));
  const span = wordAt(text, at);
  const raw = text.slice(span.from, span.to);
  const { token, from } = withoutNegation(raw, span.from);

  // Between terms with nothing typed: a connective, or the start of a new term.
  if (!token.trim()) return { kind: "connective", token: "", from: at, to: at };

  if (/^has:/i.test(token)) {
    const value = token.slice(4);
    return { kind: "key", token: value, from: from + 4, to: span.to };
  }
  const parts = splitTerm(token);
  if (!parts) {
    // A bare word: either a property name being typed, or a connective.
    const upper = token.toUpperCase();
    if (CONNECTIVES.some((c) => c.startsWith(upper) && upper.length > 0 && !token.includes(":"))) {
      // Ambiguous — `o` could be `OR` or a property. The caller offers both; the kind says
      // which list leads, and a property name is the commoner intent mid-query.
      return { kind: "key", token, from, to: span.to };
    }
    return { kind: "key", token, from, to: span.to };
  }
  const quoted = parts.value.startsWith('"');
  return {
    kind: "value",
    token: quoted ? parts.value.replace(/^"|"$/g, "") : parts.value,
    from: from + parts.key.length + 1 + parts.operator.length,
    to: span.to,
    key: parts.key,
    operator: parts.operator,
    quoted,
  };
}

/** A value needs quoting when it holds a space or a character the grammar would read. */
export function quoteIfNeeded(value: string): string {
  return /[\s()":]/.test(value) ? `"${value.replace(/"/g, '')}"` : value;
}

/**
 * Put `replacement` into `slot`, returning the new query and where the caret should land.
 *
 * Completing a property name adds the colon, because nobody wants to type it, and leaves the
 * caret after it so the value suggestions open immediately.
 */
export function applySuggestion(
  text: string,
  slot: Slot,
  replacement: string,
  options: { addColon?: boolean } = {},
): { text: string; caret: number } {
  const insert = slot.kind === "value" ? quoteIfNeeded(replacement) : replacement + (options.addColon ? ":" : "");
  const next = text.slice(0, slot.from) + insert + text.slice(slot.to);
  return { text: next, caret: slot.from + insert.length };
}

/** The connectives worth offering, filtered by what has been typed. */
export function connectiveSuggestions(token: string): string[] {
  const upper = token.toUpperCase();
  return CONNECTIVES.filter((c) => c.startsWith(upper));
}

/**
 * Is the query complete enough to run?
 *
 * The UI runs as you type, and a query ending mid-term (`status:`) or with a dangling
 * connective is an error the server would reject — showing that error on every keystroke
 * would be noise, so those states simply do not run.
 */
export function isRunnable(text: string): boolean {
  const t = text.trim();
  if (!t) return true; // empty means everything, which is a real answer
  if (/(^|\s)(AND|OR|NOT)$/i.test(t)) return false;
  if (/:$/.test(t)) return false;
  if (/(^|\s)-$/.test(t)) return false;
  // An odd number of quotes is a string still being typed.
  if ((t.match(/"/g)?.length ?? 0) % 2 === 1) return false;
  const open = (t.match(/\(/g) ?? []).length;
  const close = (t.match(/\)/g) ?? []).length;
  if (open !== close) return false;
  // Every word has to be a whole term. Typing a property name gets as far as `stat`, which
  // the grammar rejects — running it would mean a failed request and a red message on every
  // keystroke, so a half-typed term simply waits for its colon.
  for (const span of tokens(t)) {
    const bare = t.slice(span.from, span.to).replace(/^-/, "");
    if (!bare) return false;
    if (CONNECTIVES.includes(bare.toUpperCase())) continue;
    if (!bare.includes(":")) return false;
    // `has:` and `key:` with nothing after are also mid-term.
    if (/:$/.test(bare)) return false;
  }
  return true;
}
