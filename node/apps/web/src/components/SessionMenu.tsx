import { useEffect, useRef, useState } from "react";
import { ApiError, api, token, type Whoami } from "../api";

interface Props {
  who: Whoami | null;
  onChange: (who: Whoami | null) => void;
}

/**
 * Who this session is, and how to be somebody else.
 *
 * The owner needs nothing: opening the store is being its owner, which is what a local server
 * answers as. An account arrives with a bearer, and what it holds -- its shares and the rights it
 * holds them by -- is the first thing worth showing, because every path in the app is then that
 * account's own and not the store's.
 *
 * The token is pasted once and sent to `POST /api/session`, which checks it before handing out the
 * session cookie the change feed and the asset bytes travel on. It is never rendered back.
 */
export function SessionMenu({ who, onChange }: Props) {
  const [open, setOpen] = useState(false);
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const panel = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!open) return;
    const away = (e: MouseEvent) => {
      if (panel.current && !panel.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", away);
    return () => document.removeEventListener("mousedown", away);
  }, [open]);

  const label = who?.account ?? (who?.admin ? "owner" : "not signed in");
  const signIn = async (next: string | null) => {
    setBusy(true);
    setProblem(null);
    try {
      const answered = await api.login(next);
      onChange(answered);
      setValue("");
      setOpen(false);
    } catch (error) {
      setProblem(error instanceof ApiError ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="session" ref={panel}>
      <button
        type="button"
        className="btn btn-small session-button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        title={who?.account ? `Signed in as ${who.account}` : "The store's owner"}
      >
        <span className={`session-dot ${who?.account ? "session-account" : "session-owner"}`} aria-hidden="true" />
        {label}
      </button>
      {open && (
        <div className="session-panel" role="dialog" aria-label="Session">
          <div className="session-who">
            <strong>{label}</strong>
            {who && (
              <span className="session-kind">
                {who.kind}
                {who.namespace !== "store" && ` · ${who.namespace}`}
              </span>
            )}
          </div>
          {who?.shares.length ? (
            <ul className="session-shares">
              {who.shares.map((s) => (
                <li key={s.alias}>
                  <code>/{s.alias}</code>
                  <span className={`rights rights-${s.rights}`}>{s.rights}</span>
                  {s.dormant && <span className="session-dormant">its folder is in the trash</span>}
                </li>
              ))}
            </ul>
          ) : (
            <p className="session-note">
              {who?.admin
                ? "The whole store: the owner reaches everything directly and holds no shares."
                : "No shares. Paste a token to work as an account."}
            </p>
          )}
          <label className="session-field">
            <span className="visually-hidden">Bearer token</span>
            <input
              type="password"
              value={value}
              placeholder="tdb_…"
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && value.trim()) void signIn(value.trim());
              }}
            />
          </label>
          {problem && <p className="session-problem">{problem}</p>}
          <div className="session-actions">
            <button type="button" className="btn btn-small" disabled={busy || !value.trim()} onClick={() => void signIn(value.trim())}>
              Sign in
            </button>
            {token() && (
              <button type="button" className="btn btn-small" disabled={busy} onClick={() => void signIn(null)}>
                Sign out
              </button>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
