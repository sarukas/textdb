import { useCallback, useEffect, useRef, useState } from "react";
import {
  ApiError,
  api,
  type AssetItem,
  type AssetStatus,
  type AssetStoreRow,
  type AssetVerification,
  type SyncLinks,
} from "../api";
import { stateLabel, syncLinkFor } from "../assets/model";
import { formatBytes } from "../import/select";
import { effectiveAuthor } from "../state/useAuthor";

interface Props {
  links: SyncLinks | null;
  onClose: () => void;
  /** Who a pull or a push writes pointers as. */
  author: string;
  /** Open on this asset's whereabouts, when this was opened from one. */
  path?: string | null;
}

/** What a pull, a push or a relocate did, including the ones it would not do. */
function done(did: string, nothing: string, n: number, bytes: number | undefined, problems: string[]): string {
  const said = n === 0 ? nothing : `${did} ${n} ${n === 1 ? "asset" : "assets"}${bytes ? ` (${formatBytes(bytes)})` : ""}`;
  return problems.length ? `${said}; ${problems.join("; ")}` : said;
}

/** A size, or an em dash when the pointer does not carry one. */
const size = (n: number | undefined) => (n === undefined ? "—" : formatBytes(n));

/**
 * The assets of a synced folder, and the stores their bytes are actually in.
 *
 * Two things are worth seeing and could not be seen anywhere before: **every** asset of a vault at
 * once, and, for one of them, the whole chain from its path here to the bytes on a drive. An asset
 * is a pointer document and a file somewhere else, and almost every confusing thing about one comes
 * from not knowing which end is being talked about.
 *
 * All of this is the owner's: it works on directories of the server's own machine, and the server
 * refuses a token session outright rather than half-answering.
 */
export function AssetsPanel({ links, onClose, path, author }: Props) {
  const usable = (links?.links ?? []).filter((l) => l.last);
  // The folder an asset is in is the innermost one that holds it, which is what `syncLinkFor`
  // answers: `/notes` and `/notesx` are different folders, and nesting means the longest wins.
  const [prefix, setPrefix] = useState("");
  const wanted = (path && syncLinkFor(path, usable)?.prefix) || usable[0]?.prefix || "";
  // The links arrive after the first render when the page has just loaded, so the folder is picked
  // when they do rather than once, which left the panel saying there were none. The folders are
  // compared as their names, since the array itself is new on every render.
  const available = JSON.stringify(usable.map((l) => l.prefix));
  useEffect(() => {
    const folders = JSON.parse(available) as string[];
    setPrefix((had) => (had && folders.includes(had) ? had : wanted));
  }, [wanted, available]);
  const [status, setStatus] = useState<AssetStatus | null>(null);
  const [stores, setStores] = useState<AssetStoreRow[] | null>(null);
  const [verification, setVerification] = useState<AssetVerification | null>(null);
  const [where, setWhere] = useState<string | null>(path ?? null);
  const [problem, setProblem] = useState<string | null>(null);
  /** What the last pull, push or relocate did, in its own words. */
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const who = effectiveAuthor(author);
  const dialogRef = useRef<HTMLDialogElement | null>(null);
  // A verify hashes every asset here and in its store and can run for a while; closing the panel
  // gives up on it rather than leaving it to answer into a component that has gone.
  const running = useRef<AbortController | null>(null);
  useEffect(() => () => running.current?.abort(), []);

  useEffect(() => {
    dialogRef.current?.showModal();
  }, []);

  /** Runs one call, shows whatever it refuses with, and says whether it got through. */
  const guard = useCallback(async (run: () => Promise<void>): Promise<boolean> => {
    setBusy(true);
    setProblem(null);
    try {
      await run();
      return true;
    } catch (error) {
      setProblem(error instanceof ApiError ? `${error.code}: ${error.message}` : String(error));
      return false;
    } finally {
      setBusy(false);
    }
  }, []);

  const reload = useCallback(
    () =>
      guard(async () => {
        setStores(await api.assetStores());
        if (prefix) setStatus(await api.assets(prefix));
      }),
    [guard, prefix],
  );

  useEffect(() => {
    void reload();
  }, [reload]);

  // What was said about the folder that was on screen is not said about the next one.
  useEffect(() => {
    setVerification(null);
    setNote(null);
    setWhere((at) => (at && prefix && (at === prefix || at.startsWith(`${prefix}/`)) ? at : null));
  }, [prefix]);

  const selected = status?.assets.find((a) => a.path === where) ?? null;
  const storeOf = (name: string | undefined) => stores?.find((s) => s.name === name) ?? null;

  return (
    <dialog
      ref={dialogRef}
      className="dialog assets-dialog"
      aria-labelledby="assets-title"
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
    >
      <div className="dialog-head">
        <h2 id="assets-title">Assets</h2>
        {/* Closing is never refused: a verify hashes every asset here and in its store, and a
            panel that holds someone until that finishes is a worse answer than a closed one. */}
        <button type="button" className="btn btn-ghost btn-small" onClick={onClose} aria-label="Close">
          ✕
        </button>
      </div>

      {problem && <p className="access-problem">{problem}</p>}

      <section className="access-section">
        <h3 className="assets-head">Stores</h3>
        <p className="muted assets-note">
          Declared once, for everyone, and <strong>bound per machine</strong>: the same shared drive is mounted differently by
          each person. A binding here is this server's own, written to its config file.
        </p>
        <table className="access-table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Driver</th>
              <th>Root (everyone's)</th>
              <th>This machine</th>
              <th>Reachable</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {(stores ?? []).map((s) => (
              <tr key={s.name}>
                <td>
                  <code>{s.name}</code>
                </td>
                <td>{s.driver}</td>
                <td>
                  <code>{s.root}</code>
                </td>
                <td>
                  {s.bound_to ? (
                    <span title={s.bound_by ?? undefined}>
                      <code>{s.bound_to}</code>
                    </span>
                  ) : (
                    <span className="muted">the root</span>
                  )}
                </td>
                <td>
                  {s.reachable ? (
                    <span className="asset-ok">yes</span>
                  ) : (
                    <span className="asset-problem" title={s.problem ?? undefined}>
                      no
                    </span>
                  )}
                </td>
                <td className="access-row-actions">
                  <button
                    type="button"
                    className="btn btn-small"
                    disabled={busy}
                    onClick={() => {
                      const to = prompt(`Where does this machine reach ${s.name}? (empty clears the binding)`, s.bound_to ?? "");
                      if (to !== null) void guard(async () => setStores((await api.bindAssetStore(s.name, to)).stores));
                    }}
                  >
                    Bind…
                  </button>
                  <button
                    type="button"
                    className="btn btn-small btn-danger"
                    disabled={busy}
                    title="Refused while any pointer names it: the bytes would have to move with it"
                    onClick={() => void guard(async () => setStores((await api.removeAssetStore(s.name)).stores))}
                  >
                    Remove
                  </button>
                </td>
              </tr>
            ))}
            {stores?.length === 0 && (
              <tr>
                <td colSpan={6} className="muted">
                  No asset stores. Declare one and a push has somewhere to put the bytes.
                </td>
              </tr>
            )}
          </tbody>
        </table>
        <NewStore busy={busy} onAdd={(body) => guard(async () => setStores((await api.putAssetStore(body)).stores))} />
      </section>

      <section className="access-section">
        <h3 className="assets-head">
          Assets
          {usable.length > 1 && (
            <select value={prefix} onChange={(e) => setPrefix(e.target.value)} aria-label="Folder">
              {usable.map((l) => (
                <option key={l.prefix} value={l.prefix}>
                  {l.prefix}
                </option>
              ))}
            </select>
          )}
        </h3>
        {!prefix && <p className="muted">No synced folder on this server yet. Sync one, and its assets appear here.</p>}
        {status && (
          <>
            <div className="assets-counts">
              {Object.entries(status.counts).map(([state, n]) => (
                <span key={state} className={`asset-count asset-${stateLabel(state).tone}`}>
                  <strong>{n}</strong> {stateLabel(state).label.toLowerCase()}
                </span>
              ))}
              <span className="muted assets-dir" title={status.dir}>
                {status.dir}
              </span>
            </div>
            <div className="assets-actions">
              <button
                type="button"
                className="btn btn-small"
                disabled={busy}
                onClick={() => void guard(async () => {
                  const r = await api.pullAssets({ prefix, paths: [], author: who });
                  setNote(done("pulled", "nothing to pull", r.pulled.length, r.bytes, [...r.kept, ...r.failed]));
                  await reload();
                })}
              >
                Pull all
              </button>
              <button
                type="button"
                className="btn btn-small"
                disabled={busy}
                onClick={() => void guard(async () => {
                  const r = await api.pushAssets({ prefix, paths: [], author: who });
                  setNote(done("pushed", "nothing to push", r.pushed.length, r.bytes, [...r.conflicts, ...r.failed]));
                  await reload();
                })}
              >
                Push all
              </button>
              <button
                type="button"
                className="btn btn-small"
                disabled={busy}
                title="Move the files of assets that moved here while their file stayed where it was"
                onClick={() => void guard(async () => {
                  const r = (await api.relocateAssets({ prefix, paths: [], author: who })) as {
                    moved?: unknown[];
                    failed?: unknown[];
                  };
                  setNote(done("moved the files of", "nothing to move in the store", r.moved?.length ?? 0, undefined, (r.failed ?? []).map(String)));
                  await reload();
                })}
              >
                Relocate
              </button>
              <button
                type="button"
                className="btn btn-small"
                disabled={busy}
                title="Hash every asset here and in its store, and list the store's files no pointer names"
                onClick={() =>
                  void guard(async () => {
                    running.current?.abort();
                    const ctl = new AbortController();
                    running.current = ctl;
                    setVerification(await api.verifyAssets(prefix, undefined, ctl.signal));
                  })
                }
              >
                Verify
              </button>
            </div>
            {note && <p className="muted assets-note">{note}</p>}
            <table className="access-table">
              <thead>
                <tr>
                  <th>Asset</th>
                  <th>State</th>
                  <th>Size</th>
                  <th>Type</th>
                  <th>Store</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {status.assets.map((a) => (
                  <tr key={a.path} className={where === a.path ? "assets-selected" : undefined}>
                    <td>
                      <code>{a.path}</code>
                    </td>
                    <td>
                      <span className={`asset-badge asset-${stateLabel(a.state).tone}`} title={a.note ?? stateLabel(a.state).hint}>
                        {stateLabel(a.state).label}
                      </span>
                    </td>
                    <td className="mono">{size(a.size)}</td>
                    <td className="muted">{a.type}</td>
                    <td>{a.store ? <code>{a.store}</code> : <span className="muted">—</span>}</td>
                    <td className="access-row-actions">
                      <button type="button" className="btn btn-small" onClick={() => setWhere(where === a.path ? null : a.path)}>
                        Where?
                      </button>
                    </td>
                  </tr>
                ))}
                {status.assets.length === 0 && (
                  <tr>
                    <td colSpan={6} className="muted">
                      No assets in this folder.
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </>
        )}
        {selected && <Whereabouts item={selected} store={storeOf(selected.store)} dir={status?.dir ?? ""} />}
        {verification && (
          <div className="assets-verify">
            <h4>
              Verified {verification.assets.length} assets · {verification.problems} problems
            </h4>
            <ul>
              {verification.assets
                .filter((a) => a.here !== "ok" || a.asset_store !== "ok")
                .map((a) => (
                  <li key={a.path}>
                    <code>{a.path}</code> — here: {a.here}, asset store: {a.asset_store}
                  </li>
                ))}
              {verification.unnamed.map((u, i) => (
                <li key={`unnamed-${i}`} className="muted">
                  {u.unchecked ? (
                    <>the asset store {u.store} could not be listed: {u.unchecked}</>
                  ) : (
                    <>
                      <code>{u.at}</code> in {u.store}: no pointer names it
                    </>
                  )}
                </li>
              ))}
            </ul>
          </div>
        )}
      </section>
    </dialog>
  );
}

/**
 * Where one asset's bytes actually are, end to end.
 *
 * Reading downwards: the path in textdb, the pointer document that stands there, the store that
 * pointer names, where this machine reaches that store, the item inside it, and the file on this
 * server's disk. An asset is exactly this chain, and every puzzling state is a disagreement
 * between two of its links.
 */
function Whereabouts({ item, store, dir }: { item: AssetItem; store: AssetStoreRow | null; dir: string }) {
  const label = stateLabel(item.state);
  const drive = item.store && store?.driver === "rclone";
  return (
    <div className="assets-where">
      <h4>
        Where <code>{item.path}</code> is
      </h4>
      <ol className="where-chain">
        <li>
          <span className="where-what">in textdb</span>
          <code>{item.path}</code>
          <span className="muted">the asset's own path; its pointer is the document beside it</span>
        </li>
        <li>
          <span className="where-what">the pointer says</span>
          <code>
            {item.sha256 ? `${item.sha256.slice(0, 12)}…` : "—"} · {size(item.size)} · {item.type}
          </code>
          <span className="muted">{item.version ? `version ${item.version} of the pointer document` : "not committed yet"}</span>
        </li>
        <li>
          <span className="where-what">asset store</span>
          {item.store ? <code>{item.store}</code> : <span className="muted">none yet — a push would choose one</span>}
          {store && <span className="muted">{store.driver}</span>}
        </li>
        <li>
          <span className="where-what">the store's root</span>
          {store ? <code>{store.root}</code> : <span className="muted">—</span>}
          <span className="muted">the same for everyone who uses this store</span>
        </li>
        <li>
          <span className="where-what">this machine reaches it</span>
          {store?.bound_to ? <code>{store.bound_to}</code> : <span className="muted">at the root above</span>}
          {store?.bound_by && <span className="muted">{store.bound_by}</span>}
        </li>
        <li>
          <span className="where-what">the item</span>
          <code>{item.in_store ?? "the asset's own path in the store"}</code>
          <span className="muted">
            {drive ? "a drive keeps its own id for a file, so the item is that id and follows a move" : "a path in the store"}
          </span>
        </li>
        <li>
          <span className="where-what">on this server's disk</span>
          {item.file ? (
            <code>
              {dir}
              {dir.endsWith("/") || dir.endsWith("\\") ? "" : "/"}
              {item.file}
            </code>
          ) : (
            <span className="muted">not pulled here</span>
          )}
        </li>
      </ol>
      <p className={`assets-state asset-${label.tone}`}>
        <strong>{label.label}.</strong> {item.note ?? label.hint}
      </p>
    </div>
  );
}

function NewStore({ busy, onAdd }: { busy: boolean; onAdd: (body: { name: string; driver: string; root: string }) => Promise<boolean> }) {
  const [name, setName] = useState("");
  const [driver, setDriver] = useState("local");
  const [root, setRoot] = useState("");
  return (
    <form
      className="access-form"
      onSubmit={(e) => {
        e.preventDefault();
        if (!name.trim() || !root.trim()) return;
        // Cleared only once it is declared: a failed one is still on screen to be corrected.
        void onAdd({ name: name.trim(), driver, root: root.trim() }).then((declared) => {
          if (!declared) return;
          setName("");
          setRoot("");
        });
      }}
    >
      <input value={name} placeholder="store name" onChange={(e) => setName(e.target.value)} spellCheck={false} />
      <select value={driver} onChange={(e) => setDriver(e.target.value)} aria-label="Driver">
        <option value="local">local — a folder this machine reaches</option>
        <option value="rclone">rclone — a remote, with each person's own configuration</option>
      </select>
      <input
        value={root}
        placeholder={driver === "rclone" ? "REMOTE:path, e.g. teamdrive:textdb" : "a folder"}
        onChange={(e) => setRoot(e.target.value)}
        spellCheck={false}
      />
      <button type="submit" className="btn btn-small" disabled={busy || !name.trim() || !root.trim()}>
        Declare
      </button>
    </form>
  );
}
