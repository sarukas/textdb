import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  subscribe,
  type ChangeEvent,
  type ConnectionState,
  type Info,
  type PurgeStats,
  type Subscription,
  type SyncLinks,
  type Whoami,
} from "./api";
import { AccessPanel } from "./components/AccessPanel";
import { ActivityFeed } from "./components/ActivityFeed";
import { AssetPane } from "./components/AssetPane";
import { AssetsPanel } from "./components/AssetsPanel";
import { DocumentPane, type Mode, type OpenDoc } from "./components/DocumentPane";
import { isPointer, syncLinkFor } from "./assets/model";
import { ExportDialog } from "./components/ExportDialog";
import { SyncDialog } from "./components/SyncDialog";
import { FolderView } from "./components/FolderView";
import { Header } from "./components/Header";
import { ImportDialog } from "./components/ImportDialog";
import {
  BulkDeleteDialog,
  BulkMoveDialog,
  DeleteDialog,
  MoveDialog,
  PurgeDialog,
  ReplaceDialog,
} from "./components/PathDialogs";
import { downloadText } from "./doc/transfer";
import { Sidebar } from "./components/Sidebar";
import { useToast } from "./components/Toasts";
import { TrashDocument } from "./components/TrashDocument";
import { addToFeed, type FeedItem } from "./live/activity";
import { baseName, isWithin, parentOf } from "./live/paths";
import { hashFor, parseHash, type Target } from "./nav/hash";
import { bulkSummary, purgeSummary, type BulkAction, type PathAction, type TrashAction } from "./tree/actions";
import { FeedHub } from "./state/hub";
import { OwnWrites } from "./state/ownWrites";
import { useAuthor } from "./state/useAuthor";

/** A pointer document opens as its asset when the server syncs a folder it is in with a directory. */
function assetLinkOf(path: string, links: SyncLinks | null): SyncLinks["links"][number] | null {
  return isPointer(path) ? syncLinkFor(path, links?.links ?? []) : null;
}

const FEED_CAP = 500;
let openSeq = 0;

/** Show `target` in the address bar: a history entry for a navigation, in place otherwise. */
function writeHash(target: Target, push: boolean): void {
  const hash = hashFor(target);
  if (location.hash === hash) return;
  if (push) history.pushState(null, "", hash);
  else history.replaceState(null, "", hash);
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
  const [initial] = useState(() => parseHash(location.hash));
  const [author, setAuthor] = useAuthor();
  const [info, setInfo] = useState<Info | null>(null);
  /** Who this session is: the owner, or the account whose token it presented. */
  const [who, setWho] = useState<Whoami | null>(null);
  /** The access panel, and the folder it was opened from when it was opened from one. */
  const [access, setAccess] = useState<{ folder: string | null } | null>(null);
  /** The assets panel, and the asset it was opened on when it was opened from one. */
  const [assetsAt, setAssetsAt] = useState<{ path: string | null } | null>(null);
  const [connection, setConnection] = useState<ConnectionState>("connecting");
  const [lastSeq, setLastSeq] = useState(0);
  const [feed, setFeed] = useState<FeedItem[]>([]);
  const [importing, setImporting] = useState(false);
  /** The folder being exported. */
  const [exporting, setExporting] = useState<string | null>(null);
  const [pathAction, setPathAction] = useState<PathAction | null>(null);
  const [trashAction, setTrashAction] = useState<TrashAction | null>(null);
  const [trashDoc, setTrashDoc] = useState<number | null>(initial?.type === "trash" ? initial.id : null);
  // The folder in the central listing, when neither a document nor a trashed file is open.
  const [folder, setFolder] = useState<string | null>(initial?.type === "folder" ? initial.path : null);
  const [bulkAction, setBulkAction] = useState<BulkAction | null>(null);
  const toast = useToast();
  const [replacing, setReplacing] = useState<{ path: string; file: File } | null>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const replaceTarget = useRef<string | null>(null);
  const [syncLinks, setSyncLinks] = useState<SyncLinks | null>(null);
  /** The folder being synced. */
  const [syncing, setSyncing] = useState<string | null>(null);
  const loadSyncLinks = useCallback(() => {
    api.syncLinks().then(setSyncLinks, () => setSyncLinks(null));
  }, []);
  useEffect(loadSyncLinks, [loadSyncLinks]);
  // Commits move the "changed here since" counts of synced folders.
  const hasSyncLinks = (syncLinks?.links.length ?? 0) > 0;
  useEffect(() => {
    if (!hasSyncLinks) return;
    const timer = setTimeout(loadSyncLinks, 1500);
    return () => clearTimeout(timer);
  }, [lastSeq, hasSyncLinks, loadSyncLinks]);
  const syncLink = syncing === null ? undefined : syncLinks?.links.find((l) => l.prefix === syncing);

  // Download and replace need no dialog of their own before the browser's: the file picker has
  // to open within the click that asked for it.
  const onPathAction = useCallback(
    (action: PathAction) => {
      if (action.op === "download") {
        api.file(action.path, action.version).then(
          (f) => downloadText(baseName(f.path), f.content),
          (e: unknown) => toast(`Could not download ${action.path}: ${e instanceof Error ? e.message : String(e)}`, "error"),
        );
      } else if (action.op === "export") {
        setExporting(action.path);
      } else if (action.op === "replace") {
        replaceTarget.current = action.path;
        fileInput.current?.click();
      } else {
        setPathAction(action);
      }
    },
    [toast],
  );
  const [open, setOpen] = useState<OpenDoc | null>(() =>
    initial?.type === "file" ? { id: ++openSeq, path: initial.path, line: initial.line, nonce: 1 } : null,
  );
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
      setFeed((prev) => addToFeed(prev, batch, FEED_CAP));
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
        api.whoami().then(
          (w) => !stopped && setWho(w),
          () => {},
        );
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

  // Opening something is a navigation (Back returns); `push = false` shows it without a new
  // history entry, for the hash changes that already are one.
  const openFile = useCallback((path: string, line?: number, push = true) => {
    setTrashDoc(null);
    setFolder(null);
    setOpen((prev) =>
      prev && prev.path === path
        ? { ...prev, line: line ?? null, nonce: prev.nonce + 1 }
        : { id: ++openSeq, path, line: line ?? null, nonce: 1 },
    );
    if (line !== undefined) setMode((m) => (m === "history" ? "preview" : m));
    writeHash({ type: "file", path, line: line ?? null }, push);
  }, []);

  const openFolder = useCallback((path: string, push = true) => {
    setTrashDoc(null);
    setOpen(null);
    setFolder(path);
    writeHash({ type: "folder", path }, push);
  }, []);

  const openTrash = useCallback((id: number, push = true) => {
    setOpen(null);
    setFolder(null);
    setTrashDoc(id);
    writeHash({ type: "trash", id }, push);
  }, []);

  const closeTrash = useCallback(() => openFolder("/", false), [openFolder]);

  // Back, Forward and links typed into the address bar change the hash without a reload.
  useEffect(() => {
    const onHash = () => {
      const target: Target = parseHash(location.hash) ?? { type: "folder", path: "/" };
      if (target.type === "trash") openTrash(target.id, false);
      else if (target.type === "folder") openFolder(target.path, false);
      else openFile(target.path, target.line ?? undefined, false);
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, [openFile, openFolder, openTrash]);

  // A purged trash file that is open closes here; one purged with its folder notices by itself.
  const onPurged = (action: TrashAction, stats: PurgeStats) => {
    setTrashAction(null);
    if (action.op === "empty" || action.entry.id === trashDoc) closeTrash();
    toast(purgeSummary(stats), "ok");
  };

  const onPathChange = useCallback((path: string) => {
    setOpen((prev) => (prev ? { ...prev, path } : prev));
    writeHash({ type: "file", path, line: null }, false);
  }, []);

  // A moved document follows its file through the change feed (see DocController); a deleted
  // document or folder listing gives way to the folder that held it.
  const onDeleted = (path: string) => {
    setPathAction(null);
    const shown = open?.path ?? folder;
    if (shown && (shown === path || isWithin(path, shown))) openFolder(parentOf(path), false);
  };

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
      <Header
        onAccess={() => setAccess({ folder: folder })}
        onAssets={() => setAssetsAt({ path: null })}
        who={who}
        onWho={(w) => {
          setWho(w);
          // Every path in the app is this session's own, so nothing that was fetched as somebody
          // else can stay on screen.
          location.reload();
        }}
        info={info}
        connection={connection}
        lastSeq={lastSeq}
        author={author}
        onAuthor={setAuthor}
        onImport={() => setImporting(true)}
      />
      {access && <AccessPanel folder={access.folder} onClose={() => setAccess(null)} />}
      {assetsAt && (
        <AssetsPanel
          links={syncLinks}
          path={assetsAt.path}
          author={author}
          onClose={() => {
            setAssetsAt(null);
            loadSyncLinks();
          }}
        />
      )}
      {importing && <ImportDialog author={author} onClose={() => setImporting(false)} onOpen={openFile} />}
      {exporting !== null && <ExportDialog key={exporting} path={exporting} onClose={() => setExporting(null)} />}
      {syncLink && (
        <SyncDialog
          key={syncLink.prefix}
          link={syncLink}
          author={author}
          onOpenFile={openFile}
          onClose={() => {
            setSyncing(null);
            loadSyncLinks();
          }}
        />
      )}
      {pathAction?.op === "move" && (
        <MoveDialog
          key={pathAction.path}
          target={pathAction}
          author={author}
          onClose={() => setPathAction(null)}
          onMoved={() => setPathAction(null)}
        />
      )}
      <input
        ref={fileInput}
        type="file"
        hidden
        onChange={(e) => {
          const file = e.currentTarget.files?.[0];
          const path = replaceTarget.current;
          e.currentTarget.value = "";
          if (file && path) setReplacing({ path, file });
        }}
      />
      {replacing && (
        <ReplaceDialog
          key={`${replacing.path}:${replacing.file.name}`}
          path={replacing.path}
          file={replacing.file}
          author={author}
          onClose={() => setReplacing(null)}
          onReplaced={(path, result) => {
            setReplacing(null);
            toast(
              result.kind === "noop"
                ? `${baseName(path)} is unchanged`
                : `Replaced ${baseName(path)}: v${result.version}${result.kind === "direct" ? "" : ` · ${result.kind}`}`,
              "ok",
            );
          }}
        />
      )}
      {trashAction && (
        <PurgeDialog
          key={trashAction.op === "empty" ? "empty" : trashAction.entry.id}
          action={trashAction}
          author={author}
          onClose={() => setTrashAction(null)}
          onDone={(stats) => onPurged(trashAction, stats)}
        />
      )}
      {pathAction?.op === "delete" && (
        <DeleteDialog
          key={pathAction.path}
          target={pathAction}
          author={author}
          openPath={open?.path ?? null}
          onClose={() => setPathAction(null)}
          onDeleted={onDeleted}
        />
      )}
      {bulkAction && (
        <>
          {bulkAction.op === "move" ? (
            <BulkMoveDialog
              action={bulkAction}
              author={author}
              openPath={open?.path ?? null}
              onClose={() => setBulkAction(null)}
              onDone={(result) => {
                setBulkAction(null);
                toast(bulkSummary(result), "ok");
              }}
            />
          ) : (
            <BulkDeleteDialog
              action={bulkAction}
              author={author}
              openPath={open?.path ?? null}
              onClose={() => setBulkAction(null)}
              onDone={(result) => {
                setBulkAction(null);
                toast(bulkSummary(result), "ok");
              }}
            />
          )}
        </>
      )}
      <main className="workspace">
        <aside className="sidebar" aria-label="Files and search">
          <Sidebar
            hub={hub}
            openPath={open?.path ?? null}
            openFolder={open || trashDoc !== null ? null : (folder ?? "/")}
            openTrashId={trashDoc}
            onOpen={openFile}
            onOpenFolder={openFolder}
            onOpenTrash={openTrash}
            onAction={onPathAction}
            onTrashAction={setTrashAction}
          />
        </aside>
        <section className="center" aria-label={trashDoc !== null ? "Trashed file" : open ? "Document" : "Folder"}>
          {trashDoc !== null ? (
            <TrashDocument
              key={trashDoc}
              id={trashDoc}
              hub={hub}
              onPurge={(entry) => setTrashAction({ op: "purge", entry })}
              onClose={closeTrash}
            />
          ) : open && assetLinkOf(open.path, syncLinks) ? (
            <AssetPane
              key={open.id}
              pointer={open.path}
              link={assetLinkOf(open.path, syncLinks)!}
              hub={hub}
              author={author}
              onAction={onPathAction}
              onOpenFolder={openFolder}
              onPathChange={onPathChange}
              onWhere={(asset) => setAssetsAt({ path: asset })}
            />
          ) : open ? (
            <DocumentPane
              key={open.id}
              open={open}
              mode={mode}
              onMode={setMode}
              hub={hub}
              own={own}
              author={author}
              onPathChange={onPathChange}
              onAction={onPathAction}
              onOpenFolder={openFolder}
            />
          ) : (
            <FolderView
              key={folder ?? "/"}
              path={folder ?? "/"}
              hub={hub}
              onOpenFolder={openFolder}
              onOpenFile={openFile}
              onAction={onPathAction}
              onBulk={setBulkAction}
              syncLink={syncLinks?.links.find((l) => l.prefix === (folder ?? "/")) ?? null}
              assetLink={syncLinkFor(folder ?? "/", syncLinks?.links ?? [])}
              onSync={setSyncing}
            />
          )}
        </section>
        <aside className="activity-wrap" aria-label="Activity feed">
          <ActivityFeed items={feed} author={author} own={own} open={feedOpen} onToggle={toggleFeed} onOpen={openFile} />
        </aside>
      </main>
    </div>
  );
}
