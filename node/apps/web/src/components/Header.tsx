import type { ConnectionState, Info, Whoami } from "../api";
import { authorStyle } from "../live/color";
import { baseName } from "../live/paths";
import { effectiveAuthor } from "../state/useAuthor";
import { SessionMenu } from "./SessionMenu";

interface Props {
  info: Info | null;
  connection: ConnectionState;
  lastSeq: number;
  author: string;
  onAuthor: (name: string) => void;
  onImport: () => void;
  who: Whoami | null;
  onWho: (who: Whoami | null) => void;
  onAccess: () => void;
  /** Open the assets panel: the stores, and where every asset’s bytes are. */
  onAssets: () => void;
}

const STATE_LABEL: Record<ConnectionState, string> = {
  live: "Live",
  connecting: "Connecting…",
  offline: "Offline",
};

export function Header({ info, connection, lastSeq, author, onAuthor, onImport, who, onWho, onAccess, onAssets }: Props) {
  const db = info?.db ?? "";
  // The folder the store file is in. Which vault this is, is not the file's name -- every one of
  // them is `kb.db` -- it is where it sits, and a Postgres store sits in no folder at all.
  const cut = Math.max(db.lastIndexOf("/"), db.lastIndexOf("\\"));
  const dir = info?.backend === "sqlite" && cut > 0 ? db.slice(0, cut) : null;
  return (
    <header className="header">
      <div className="brand">
        <span className="brand-mark" aria-hidden="true" />
        textdb
      </div>
      <div className="store" aria-label="Store">
        <span className={`conn conn-${connection}`} role="status" aria-label={`Change feed: ${STATE_LABEL[connection]}`} title={STATE_LABEL[connection]}>
          <span className="conn-dot" aria-hidden="true" />
          {STATE_LABEL[connection]}
        </span>
        {info && (
          <>
            <span className="sep" aria-hidden="true" />
            <span className="store-db" title={db}>
              {baseName(db.replace(/\\/g, "/")) || db}
            </span>
            <span className="sep" aria-hidden="true" />
            <span>
              <strong>{info.files.toLocaleString()}</strong> files
            </span>
            <span className="sep" aria-hidden="true" />
            <span title="Last change sequence number">
              seq <strong className="mono">{lastSeq}</strong>
            </span>
            <span className="sep" aria-hidden="true" />
            <span title={`This store is ${info.backend}`} className="store-backend">
              {info.backend}
            </span>
            {dir && (
              <>
                <span className="sep" aria-hidden="true" />
                {/* Clipped at the front, because the end of a path is the part that says which
                    vault this is; the whole of it is in the tooltip and selects as one piece. */}
                <span className="store-dir" title={db} aria-label={`Folder: ${dir}`}>
                  <bdi>{dir}</bdi>
                </span>
              </>
            )}
          </>
        )}
      </div>
      <button type="button" className="btn btn-small import-button" onClick={onImport}>
        Import folder…
      </button>
      {/* The owner's alone: every asset route works on directories and drives of the server's own
          machine, and a token session is refused all of them. Offering it would be a dead end. */}
      {!who?.account && (
        <button type="button" className="btn btn-small" onClick={onAssets} title="Asset stores, and where every asset’s bytes actually are">
          Assets…
        </button>
      )}
      <SessionMenu who={who} onChange={onWho} onAccess={onAccess} />
      <label className="author-field">
        <span className="author-chip" style={authorStyle(effectiveAuthor(author))} aria-hidden="true" />
        <span className="visually-hidden">Author name</span>
        <input
          value={author}
          onChange={(e) => onAuthor(e.target.value)}
          placeholder="human"
          spellCheck={false}
          aria-label="Author name used for your saves"
        />
      </label>
    </header>
  );
}
