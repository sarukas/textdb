/** Taking a document out of the store as a file, and putting a file in as a new version. */
import { diffLines } from "../live/diff";

/** Hand `text` to the browser as a download named `name`. */
export function downloadText(name: string, text: string): void {
  const url = URL.createObjectURL(new Blob([text], { type: "text/plain;charset=utf-8" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  document.body.append(a);
  a.click();
  a.remove();
  // Revoking at once can cancel the download in some browsers.
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

export interface LineChange {
  added: number;
  removed: number;
}

/** Lines added and removed going from `a` to `b`. */
export function lineChange(a: string, b: string): LineChange {
  return diffLines(a, b).reduce(
    (n, h) => ({ added: n.added + h.new_count, removed: n.removed + h.old_count }),
    { added: 0, removed: 0 },
  );
}
