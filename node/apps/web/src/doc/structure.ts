import type { Link, LinkStatus, OutlineEntry } from "../api";

/**
 * What a markdown document is made of, beyond its text: headings and links.
 *
 * Both are rows the store keeps and resolves for every surface (`docs/outlines.md`,
 * `docs/shapes.md`), so nothing here parses markdown. What is left is the shaping the store does
 * not do -- a flat list of headings is a tree, a list of links is one list per document -- and
 * saying what a status means, which every surface otherwise invents its own words for.
 */

export interface OutlineNode {
  entry: OutlineEntry;
  children: OutlineNode[];
}

/**
 * The heading tree of one document, from the rows in document order.
 *
 * Levels are what a writer typed, so they skip (`#` then `###`) and they start anywhere: a node
 * goes under the nearest heading above it that is shallower, and under nothing when there is
 * none. That keeps a document whose first heading is `##` from disappearing, and never loses a
 * row -- every entry is somewhere in the answer, once.
 */
export function outlineTree(entries: readonly OutlineEntry[]): OutlineNode[] {
  const roots: OutlineNode[] = [];
  const stack: OutlineNode[] = [];
  for (const entry of entries) {
    const node: OutlineNode = { entry, children: [] };
    while (stack.length && stack[stack.length - 1]!.entry.level >= entry.level) stack.pop();
    const parent = stack[stack.length - 1];
    if (parent) parent.children.push(node);
    else roots.push(node);
    stack.push(node);
  }
  return roots;
}

/** The rows of `path`, in the order the documents first appear. */
export function byDocument<T extends { path: string }>(rows: readonly T[]): { path: string; rows: T[] }[] {
  const docs = new Map<string, T[]>();
  for (const row of rows) {
    const had = docs.get(row.path);
    if (had) had.push(row);
    else docs.set(row.path, [row]);
  }
  return [...docs].map(([path, own]) => ({ path, rows: own }));
}

export interface StatusLabel {
  label: string;
  /** How it reads at a glance: fine, worth a look, or a link that reaches nothing. */
  tone: "ok" | "action" | "problem";
  hint: string;
}

const STATUSES: Record<LinkStatus, StatusLabel> = {
  ok: { label: "OK", tone: "ok", hint: "It reaches the file it names." },
  external: { label: "External", tone: "ok", hint: "A URL, an email address or a reference: outside the store, and not the store's to check." },
  "not-in-store": {
    label: "Not in the store",
    tone: "action",
    hint: "A PDF, an image or another file a text store does not hold. With a synced folder it may still be an asset, or a file on disk.",
  },
  ambiguous: {
    label: "Ambiguous",
    tone: "action",
    hint: "Several files answer to that name and the nearest one is taken: write more of the path to say which.",
  },
  "anchor-missing": { label: "No such heading", tone: "problem", hint: "The file is there; the heading after `#` is not in it." },
  broken: { label: "Broken", tone: "problem", hint: "Nothing in the store answers to it: a file renamed or deleted, or a name mistyped." },
};

/** What a status means, in words: the same ones on every view that shows a link. */
export function linkStatusLabel(status: LinkStatus | null): StatusLabel {
  if (status === null) return { label: "unresolved", tone: "action", hint: "The store has not resolved this link yet." };
  return STATUSES[status] ?? { label: status, tone: "problem", hint: status };
}

/** The statuses worth someone's attention, in the order a triage view shows them. */
export const NEEDS_ATTENTION: readonly LinkStatus[] = ["broken", "anchor-missing", "ambiguous"];

/** How many links carry each status, counted only where there are some. */
export function statusCounts(links: readonly Link[]): { status: LinkStatus | null; n: number }[] {
  const counts = new Map<LinkStatus | null, number>();
  for (const l of links) counts.set(l.status, (counts.get(l.status) ?? 0) + 1);
  const order = (s: LinkStatus | null) => (s === null ? 99 : ["broken", "anchor-missing", "ambiguous", "not-in-store", "ok", "external"].indexOf(s));
  return [...counts].map(([status, n]) => ({ status, n })).sort((a, b) => order(a.status) - order(b.status));
}

/**
 * The link as it is written in the document: `[[target#anchor|alias]]`, `![[…]]` for an embed,
 * `[alias](target#anchor)` for a markdown one.
 *
 * Reconstructed from the row's parts rather than kept as text, because the row is what every
 * surface has. It is what a reader recognises on the line, which is the point of showing it.
 */
export function linkText(link: Pick<Link, "kind" | "target" | "anchor" | "alias">): string {
  const anchor = link.anchor ? `#${link.anchor}` : "";
  if (link.kind === "md" || link.kind === "image") {
    return `${link.kind === "image" ? "!" : ""}[${link.alias ?? ""}](${link.target}${anchor})`;
  }
  const alias = link.alias ? `|${link.alias}` : "";
  return `${link.kind === "embed" ? "!" : ""}[[${link.target}${anchor}${alias}]]`;
}
