import { Fragment, useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";

export interface MenuItem {
  label: string;
  hint?: string;
  danger?: boolean;
  /** Draw a separator above this item. */
  separated?: boolean;
  disabled?: boolean;
  run: () => void;
}

export interface MenuState {
  x: number;
  y: number;
  /** The row the menu belongs to. */
  key: string;
  label: string;
  items: MenuItem[];
}

interface Props extends MenuState {
  onClose: (refocus: boolean) => void;
}

/** A row's context menu: kept inside the window, closed by Escape, a click elsewhere or a choice. */
export function ContextMenu({ x, y, label, items, onClose }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ left: x, top: y });

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    setPos({
      left: Math.max(4, Math.min(x, window.innerWidth - r.width - 4)),
      top: Math.max(4, Math.min(y, window.innerHeight - r.height - 4)),
    });
    el.querySelector<HTMLElement>("[role=menuitem]:not(:disabled)")?.focus();
  }, [x, y]);

  useEffect(() => {
    const away = (e: Event) => {
      if (!ref.current?.contains(e.target as Node)) onClose(false);
    };
    const leave = () => onClose(false);
    window.addEventListener("mousedown", away, true);
    window.addEventListener("resize", leave);
    window.addEventListener("blur", leave);
    return () => {
      window.removeEventListener("mousedown", away, true);
      window.removeEventListener("resize", leave);
      window.removeEventListener("blur", leave);
    };
  }, [onClose]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const enabled = Array.from(ref.current?.querySelectorAll<HTMLElement>("[role=menuitem]:not(:disabled)") ?? []);
    const at = enabled.indexOf(document.activeElement as HTMLElement);
    if (e.key === "Escape" || e.key === "Tab") onClose(true);
    else if (e.key === "ArrowDown") enabled[(at + 1) % enabled.length]?.focus();
    else if (e.key === "ArrowUp") enabled[(at - 1 + enabled.length) % enabled.length]?.focus();
    else return;
    e.preventDefault();
    e.stopPropagation();
  };

  return (
    <div
      ref={ref}
      className="menu"
      role="menu"
      aria-label={`Actions for ${label}`}
      style={pos}
      onKeyDown={onKeyDown}
      onContextMenu={(e) => e.preventDefault()}
    >
      {items.map((item) => (
        <Fragment key={item.label}>
          {item.separated && <div role="separator" />}
          <button
            type="button"
            role="menuitem"
            className={item.danger ? "danger" : undefined}
            disabled={item.disabled}
            onClick={() => {
              onClose(false);
              item.run();
            }}
          >
            {item.label}
            {item.hint && <kbd>{item.hint}</kbd>}
          </button>
        </Fragment>
      ))}
    </div>
  );
}
