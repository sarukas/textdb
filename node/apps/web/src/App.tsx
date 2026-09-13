import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, subscribe, type ChangeEvent, type ConnectionState, type Info, type Subscription } from "./api";
import { ActivityFeed } from "./components/ActivityFeed";
import { DocumentPane, type Mode, type OpenDoc } from "./components/DocumentPane";
import { Header } from "./components/Header";
import { Sidebar } from "./components/Sidebar";
import { FeedHub } from "./state/hub";
import { OwnWrites } from "./state/ownWrites";
import { useAuthor } from "./state/useAuthor";

const FEED_CAP = 500;
let openSeq = 0;

function fromHash(): OpenDoc | null {
  const raw = decodeURIComponent(location.hash.slice(1));
  const m = /^(\/.*?)(?::(\d+))?$/.exec(raw);
  if (!m?.[1]) return null;
  return { id: ++openSeq, path: m[1], line: m[2] ? Number(m[2]) : null, nonce: 1 };
}

function readFeedOpen(): boolean {
  try {
    return localStorage.getItem("textdb.feedOpen") !== "0";
  } catch {
    return true;
  }
}

export function App() {
  const hub = useMemo(() => new FeedHub(), []);
  const own = useMemo(() => new OwnWrites(), []);
  const [author, setAuthor] = useAuthor();
  const [info, setInfo] = useState<Info | null>(null);
  const [connection, setConnection] = useState<ConnectionState>("connecting");
  const [lastSeq, setLastSeq] = useState(0);
  const [events, setEvents] = useState<ChangeEvent[]>([]);
  const [open, setOpen] = useState<OpenDoc | null>(fromHash);
  const [mode, setMode] = useState<Mode>("preview");
  const [feedOpen, setFeedOpen] = useState(readFeedOpen);

  // ---- feed connection -------------------------------------------------------------------

  const pending = useRef<ChangeEvent[]>([]);
  const flushTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const infoTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    let sub: Subscription | null = null;
    let stopped = false;
    let retry: ReturnType<typeof setTimeout> | null = null;

    const flush = () => {
      flushTimer.current = null;
      const batch = pending.current;
      pending.current = [];
      if (!batch.length) return;
      setLastSeq((s) => Math.max(s, batch[batch.length - 1]!.seq));
      setEvents((prev) => [...batch.reverse(), ...prev].slice(0, FEED_CAP));
    };
    const refreshInfo = () => {
      if (infoTimer.current) clearTimeout(infoTimer.current);
      infoTimer.current = setTimeout(() => {
        api.info().then((i) => !stopped && setInfo(i), () => {});
      }, 1000);
    };

    const start = async () => {
      try {
        const i = await api.info();
        if (stopped) return;
        setInfo(i);
        setLastSeq((s) => Math.max(s, i.last_seq));
        sub = subscribe(i.last_seq, {
          onEvent: (e) => {
            hub.events.emit(e);
            pending.current.push(e);
            if (!flushTimer.current) flushTimer.current = setTimeout(flush, 50);
            if (e.node_kind === "file" && (e.op === "create" || e.op === "delete")) refreshInfo();
          },
          onState: setConnection,
          onReconnect: (seq) => {
            hub.reconnected.emit(seq);
            refreshInfo();
          },
        });
      } catch {
        if (stopped) return;
        setConnection("offline");
        retry = setTimeout(() => void start(), 3000);
      }
    };
    void start();
    return () => {
      stopped = true;
      if (retry) clearTimeout(retry);
      if (flushTimer.current) clearTimeout(flushTimer.current);
      if (infoTimer.current) clearTimeout(infoTimer.current);
      sub?.close();
    };
  }, [hub]);

  // ---- navigation ----------------------------------------------------------------------------

  const openFile = useCallback((path: string, line?: number) => {
    setOpen((prev) =>
      prev && prev.path === path
        ? { ...prev, line: line ?? null, nonce: prev.nonce + 1 }
        : { id: ++openSeq, path, line: line ?? null, nonce: 1 },
    );
    if (line !== undefined) setMode((m) => (m === "history" ? "preview" : m));
    history.replaceState(null, "", `#${encodeURI(path)}${line ? `:${line}` : ""}`);
  }, []);

  // Follow deep links typed into the address bar (a hash change does not reload the page).
  useEffect(() => {
    const onHash = () => {
      const target = fromHash();
      if (target) openFile(target.path, target.line ?? undefined);
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, [openFile]);

  const onPathChange = useCallback((path: string) => {
    setOpen((prev) => (prev ? { ...prev, path } : prev));
    history.replaceState(null, "", `#${encodeURI(path)}`);
  }, []);

  const toggleFeed = () =>
    setFeedOpen((v) => {
      try {
        localStorage.setItem("textdb.feedOpen", v ? "0" : "1");
      } catch {
        // ignore
      }
      return !v;
    });

  return (
    <div className={`app${feedOpen ? "" : " feed-collapsed"}`}>
      <Header info={info} connection={connection} lastSeq={lastSeq} author={author} onAuthor={setAuthor} />
      <main className="workspace">
        <aside className="sidebar" aria-label="Files and search">
          <Sidebar hub={hub} openPath={open?.path ?? null} onOpen={openFile} />
        </aside>
        <section className="center" aria-label="Document">
          {open ? (
            <DocumentPane
              key={open.id}
              open={open}
              mode={mode}
              onMode={setMode}
              hub={hub}
              own={own}
              author={author}
              onPathChange={onPathChange}
            />
          ) : (
            <div className="empty welcome">
              <h2>Open a document</h2>
              <p>
                Pick a file from the tree or search the corpus (<kbd>/</kbd>). Changes made by agents from the command
                line appear here as they land.
              </p>
            </div>
          )}
        </section>
        <aside className="activity-wrap" aria-label="Activity feed">
          <ActivityFeed events={events} author={author} own={own} open={feedOpen} onToggle={toggleFeed} onOpen={openFile} />
        </aside>
      </main>
    </div>
  );
}
