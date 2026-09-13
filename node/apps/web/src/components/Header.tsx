import type { ConnectionState, Info } from "../api";
import { authorStyle } from "../live/color";
import { baseName } from "../live/paths";
import { effectiveAuthor } from "../state/useAuthor";

interface Props {
  info: Info | null;
  connection: ConnectionState;
  lastSeq: number;
  author: string;
  onAuthor: (name: string) => void;
  onImport: () => void;
}

const STATE_LABEL: Record<ConnectionState, string> = {
  live: "Live",
  connecting: "Connecting…",
  offline: "Offline",
};

export function Header({ info, connection, lastSeq, author, onAuthor, onImport }: Props) {
  const db = info?.db ?? "";
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
          </>
        )}
      </div>
      <button type="button" className="btn btn-small import-button" onClick={onImport}>
        Import folder…
      </button>
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
