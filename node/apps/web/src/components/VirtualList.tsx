import { useEffect, useState, type CSSProperties, type HTMLAttributes, type ReactNode, type RefObject } from "react";

interface Props extends Omit<HTMLAttributes<HTMLDivElement>, "children"> {
  count: number;
  rowHeight: number;
  outerRef: RefObject<HTMLDivElement>;
  renderRow: (index: number, style: CSSProperties) => ReactNode;
  overscan?: number;
  /** Rendered above the rows inside the scroll area, such as a sticky header `headerHeight` px tall. */
  header?: ReactNode;
  headerHeight?: number;
  /** Style of the content box; a `minWidth` makes the list scroll sideways. */
  innerStyle?: CSSProperties;
  /** Told the rows `[start, end)` in the DOM after each render, for loading data on demand. */
  onRange?: (start: number, end: number) => void;
}

/** Fixed-row-height virtual list: only the rows in view (plus a margin) are in the DOM. */
export function VirtualList({
  count,
  rowHeight,
  outerRef,
  renderRow,
  overscan = 10,
  header,
  headerHeight = 0,
  innerStyle,
  onRange,
  onScroll,
  ...rest
}: Props) {
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

  const top = scrollTop - headerHeight;
  const start = Math.max(0, Math.floor(top / rowHeight) - overscan);
  const end = Math.min(count, Math.ceil((top + height) / rowHeight) + overscan);
  const rows: ReactNode[] = [];
  for (let i = start; i < end; i++) {
    rows.push(renderRow(i, { position: "absolute", top: i * rowHeight, height: rowHeight, left: 0, right: 0 }));
  }
  // Scrolling re-renders only this list, so the range is reported from here, not by the parent.
  useEffect(() => {
    onRange?.(start, end);
  }, [start, end, onRange]);
  const body = <div style={{ height: count * rowHeight, position: "relative" }}>{rows}</div>;
  return (
    <div
      {...rest}
      ref={outerRef}
      onScroll={(e) => {
        setScrollTop(e.currentTarget.scrollTop);
        onScroll?.(e);
      }}
    >
      {header || innerStyle ? (
        <div style={innerStyle}>
          {header}
          {body}
        </div>
      ) : (
        body
      )}
    </div>
  );
}

/** Scroll the list just enough to show row `index`, below a sticky header of `headerHeight` px. */
export function ensureRowVisible(el: HTMLElement | null, index: number, rowHeight: number, headerHeight = 0): void {
  if (!el) return;
  const top = index * rowHeight;
  if (top < el.scrollTop) el.scrollTop = top;
  else if (top + rowHeight + headerHeight > el.scrollTop + el.clientHeight) {
    el.scrollTop = top + rowHeight + headerHeight - el.clientHeight;
  }
}
