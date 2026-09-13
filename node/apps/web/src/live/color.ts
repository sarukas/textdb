/** A stable hue (0–359) for an author name. */
export function authorHue(name: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < name.length; i++) {
    h ^= name.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  // Scramble the low bits so similar names ("agent-1", "agent-2") land far apart.
  h ^= h >>> 15;
  h = Math.imul(h, 0x2c1b3c6d);
  h ^= h >>> 12;
  return (h >>> 0) % 360;
}

/** Inline style setting the `--h` custom property the stylesheet turns into author colours. */
export function authorStyle(name: string | null | undefined): Record<string, string> {
  return { "--h": String(authorHue(name ?? "")) };
}
