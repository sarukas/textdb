/** "just now", "5s", "3m", "2h", "4d", or a date for anything older than a month. */
export function relativeTime(ts: string | number | null | undefined, now: number = Date.now()): string {
  if (ts === null || ts === undefined || ts === "") return "";
  const t = typeof ts === "number" ? (ts < 1e12 ? ts * 1000 : ts) : Date.parse(ts);
  if (Number.isNaN(t)) return String(ts);
  const s = Math.round((now - t) / 1000);
  if (s < 5) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.round(h / 24);
  if (d < 30) return `${d}d ago`;
  return new Date(t).toISOString().slice(0, 10);
}
