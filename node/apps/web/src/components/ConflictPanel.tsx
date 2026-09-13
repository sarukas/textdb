import type { ConflictState } from "../doc/controller";

interface Props {
  conflict: ConflictState;
  busy: boolean;
  onReloadTheirs: () => void;
  onOverwrite: () => void;
  onCopyMine: () => void;
  onDismiss: () => void;
}

function Pane({ title, text, hint }: { title: string; text: string; hint: string }) {
  return (
    <section className="conflict-pane" aria-label={title}>
      <header>
        <strong>{title}</strong> <span className="muted">{hint}</span>
      </header>
      <pre>{text || <span className="muted">(empty)</span>}</pre>
    </section>
  );
}

export function ConflictPanel({ conflict, busy, onReloadTheirs, onOverwrite, onCopyMine, onDismiss }: Props) {
  const region =
    conflict.region_line_from > 0
      ? `lines ${conflict.region_line_from}–${conflict.region_line_to}`
      : "the same lines";
  return (
    <div className="conflict" role="alertdialog" aria-labelledby="conflict-title">
      <div className="conflict-head">
        <div>
          <strong id="conflict-title">Conflict</strong> — {region} were changed in v{conflict.current_version} while you
          edited. Your text is kept in the editor.
        </div>
        <div className="conflict-actions">
          <button type="button" className="btn" onClick={onReloadTheirs} disabled={busy}>
            Reload theirs
          </button>
          <button type="button" className="btn btn-danger" onClick={onOverwrite} disabled={busy}>
            Overwrite with mine
          </button>
          <button type="button" className="btn" onClick={onCopyMine}>
            Copy mine
          </button>
          <button type="button" className="btn btn-ghost" onClick={onDismiss} aria-label="Dismiss conflict">
            ✕
          </button>
        </div>
      </div>
      <div className="conflict-panes">
        <Pane title="Base" hint="what you started from" text={conflict.base} />
        <Pane title="Theirs" hint={`current, v${conflict.current_version}`} text={conflict.theirs} />
        <Pane title="Ours" hint="your version of the region" text={conflict.ours} />
      </div>
    </div>
  );
}
