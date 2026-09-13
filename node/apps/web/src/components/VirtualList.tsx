import { useEffect, useState, type CSSProperties, type HTMLAttributes, type ReactNode, type RefObject } from "react";

interface Props extends Omit<HTMLAttributes<HTMLDivElement>, "children"> {
  count: number;
  rowHeight: number;
  outerRef: RefObject<HTMLDivElement>;
  renderRow: (index: number, style: CSSProperties) => ReactNode;
  overscan?: number;
}

/** Fixed-row-height virtual list: only the rows in view (plus a margin) are in the DOM. */
export function VirtualList({ count, rowHeight, outerRef, renderRow, overscan = 10, onScroll, ...rest }: Props) {
  const [scrollTop, setScrollTop] = useState(0);
  const [height, setHeight] = useState(600);

  useEffect(() => {
    const el = outerRef.current;
    if (!el) return;
    setHeight(el.clientHeight);
    const ro = new ResizeObserver(() => setHeight(el.clientHeight));
    ro.observe(el);
    return () => ro.disconnect();
  }, [outerRef]);

  const start = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan);
  const end = Math.min(count, Math.ceil((scrollTop + height) / rowHeight) + overscan);
  const rows: ReactNode[] = [];
  for (let i = start; i < end; i++) {
    rows.push(renderRow(i, { position: "absolute", top: i * rowHeight, height: rowHeight, left: 0, right: 0 }));
  }
  return (
    <div
      {...rest}
      ref={outerRef}
      onScroll={(e) => {
        setScrollTop(e.currentTarget.scrollTop);
        onScroll?.(e);
      }}
    >
      <div style={{ height: count * rowHeight, position: "relative" }}>{rows}</div>
    </div>
  );
}

/** Scroll the list just enough to show row `index`. */
export function ensureRowVisible(el: HTMLElement | null, index: number, rowHeight: number): void {
  if (!el) return;
  const top = index * rowHeight;
  if (top < el.scrollTop) el.scrollTop = top;
  else if (top + rowHeight > el.scrollTop + el.clientHeight) el.scrollTop = top + rowHeight - el.clientHeight;
}
