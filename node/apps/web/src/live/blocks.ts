import type MarkdownIt from "markdown-it";
import type { LineSpan } from "./changeset";

/** A top-level markdown block: source lines `[start, end)` (0-based) and its HTML. */
export interface Block {
  key: string;
  start: number;
  end: number;
  html: string;
}

function fnv1a(s: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return (h >>> 0).toString(36);
}

type Token = ReturnType<MarkdownIt["parse"]>[number];

/**
 * Split a markdown source into top-level blocks using `token.map`. Keys derive from the
 * rendered HTML (plus an occurrence counter), so an unchanged block keeps its key — and, in
 * React, its DOM — wherever it moves.
 */
export function renderBlocks(md: MarkdownIt, src: string): Block[] {
  const env: Record<string, unknown> = {};
  const blocks: Block[] = [];
  const seen = new Map<string, number>();
  const push = (start: number, end: number, html: string) => {
    const hash = fnv1a(html) + html.length.toString(36);
    const n = seen.get(hash) ?? 0;
    seen.set(hash, n + 1);
    blocks.push({ key: `${hash}.${n}`, start, end, html });
  };

  // YAML front matter is not markdown: show it as one literal block so it keeps its lines.
  let offset = 0;
  const fm = FRONT_MATTER.exec(src);
  if (fm) {
    const text = fm[0];
    offset = (text.match(/\n/g) ?? []).length + (text.endsWith("\n") ? 0 : 1);
    push(0, offset, `<pre class="front-matter"><code>${md.utils.escapeHtml(text.replace(/\n$/, ""))}</code></pre>\n`);
    src = src.slice(text.length);
  }

  const tokens: Token[] = md.parse(src, env);
  let i = 0;
  while (i < tokens.length) {
    const t = tokens[i]!;
    if (t.level !== 0 || t.nesting === -1 || !t.map) {
      i++;
      continue;
    }
    let j = i;
    if (t.nesting === 1) {
      j = i + 1;
      while (j < tokens.length && !(tokens[j]!.level === 0 && tokens[j]!.nesting === -1)) j++;
    }
    const html = md.renderer.render(tokens.slice(i, j + 1), md.options, env);
    push(t.map[0] + offset, t.map[1] + offset, html);
    i = j + 1;
  }
  return blocks;
}

/** A leading `---` … `---` block (Jekyll/Hugo style front matter). */
const FRONT_MATTER = /^---[ \t]*\r?\n[\s\S]*?\r?\n---[ \t]*(?:\r?\n|$)/;

/**
 * Indices of the blocks that touch the changed lines. Spans are 1-based and half-open; a
 * deletion (`from === to`) touches the block holding that line and a block ending right there.
 */
export function blocksTouching(blocks: readonly Block[], spans: readonly LineSpan[]): number[] {
  const out: number[] = [];
  blocks.forEach((b, idx) => {
    const hit = spans.some((s) => {
      const from = s.from - 1;
      const to = s.to - 1;
      if (from === to) return (b.start <= from && from < b.end) || b.end === from;
      return b.start < to && from < b.end;
    });
    if (hit) out.push(idx);
  });
  return out;
}

/** Index of the block holding 1-based `line`, or the nearest block before it (or 0). */
export function blockAtLine(blocks: readonly Block[], line: number): number {
  const l = line - 1;
  let lo = 0;
  let hi = blocks.length - 1;
  let best = 0;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (blocks[mid]!.start <= l) {
      best = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return best;
}
