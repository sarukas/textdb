/** How a diff is laid out: two columns side by side (the default), or one column. */
export type DiffLayout = "unified" | "split";

const KEY = "textdb.diffLayout";

export function parseDiffLayout(raw: string | null): DiffLayout {
  return raw === "unified" ? "unified" : "split";
}

/** The viewer's last choice, side by side when there is none. */
export function readDiffLayout(): DiffLayout {
  try {
    return parseDiffLayout(localStorage.getItem(KEY));
  } catch {
    return "split";
  }
}

export function saveDiffLayout(layout: DiffLayout): void {
  try {
    localStorage.setItem(KEY, layout);
  } catch {
    // Not persisted; the choice still applies to this page.
  }
}
