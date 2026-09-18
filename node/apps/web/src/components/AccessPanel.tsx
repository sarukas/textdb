import { useCallback, useEffect, useRef, useState } from "react";
import { type AccountRow, ApiError, api, type ShareRow, type TokenRow } from "../api";

interface Props {
  onClose: () => void;
  /** A folder to offer a share of straight away, when this was opened from one. */
  folder?: string | null;
}

type Tab = "accounts" | "shares" | "tokens";

/**
 * Who can reach what, and who says so.
 *
 * Every operation here is the owner's, and the store is what refuses anyone else -- so this shows
 * whatever the server answers and reports a refusal as it arrives, rather than deciding in advance
 * what to offer. An account that opens it sees the refusal, which is the honest thing for a UI
 * whose rules live somewhere else.
 *
 * The shape of the model, in the order it matters: an **account** holds **shares** (a folder and
 * everything below it, under a name of its own, read-only or writable), and reaches them with a
 * **token**. A bearer is shown exactly once, when it is minted.
 */
export function AccessPanel({ onClose, folder }: Props) {
  const [tab, setTab] = useState<Tab>(folder ? "shares" : "accounts");
  const [accounts, setAccounts] = useState<AccountRow[] | null>(null);
  const [shares, setShares] = useState<ShareRow[] | null>(null);
  const [tokens, setTokens] = useState<TokenRow[] | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [minted, setMinted] = useState<{ account: string; bearer: string } | null>(null);
  const [busy, setBusy] = useState(false);

  /** Of one account, or of a path: "whose shares" and "who can reach this" are one question here. */
  const [sharesOf, setSharesOf] = useState<{ account?: string; path?: string }>(folder ? { path: folder } : {});

  const guard = useCallback(async (run: () => Promise<void>) => {
    setBusy(true);
    setProblem(null);
    try {
      await run();
    } catch (error) {
      setProblem(error instanceof ApiError ? `${error.code}: ${error.message}` : String(error));
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    void guard(async () => {
      setAccounts(await api.accounts());
    });
  }, [guard]);

  useEffect(() => {
    if (tab !== "shares") return;
    void guard(async () => {
      setShares(await api.shares(sharesOf));
    });
  }, [tab, sharesOf, guard]);

  useEffect(() => {
    if (tab !== "tokens") return;
    void guard(async () => {
      setTokens(await api.tokens());
    });
  }, [tab, guard]);

  const reloadAccounts = (rows: AccountRow[]) => setAccounts(rows);

  const dialogRef = useRef<HTMLDialogElement | null>(null);
  useEffect(() => {
    dialogRef.current?.showModal();
  }, []);

  return (
    <dialog
      ref={dialogRef}
      className="dialog access-dialog"
      aria-labelledby="access-title"
      onCancel={(e) => {
        e.preventDefault();
        if (!busy) onClose();
      }}
    >
      <div className="dialog-head">
        <h2 id="access-title">Access</h2>
        <button type="button" className="btn btn-ghost btn-small" onClick={onClose} disabled={busy} aria-label="Close">
          ✕
        </button>
      </div>

        <nav className="access-tabs">
          {(["accounts", "shares", "tokens"] as Tab[]).map((t) => (
            <button key={t} type="button" className={`btn btn-small ${tab === t ? "btn-on" : ""}`} onClick={() => setTab(t)}>
              {t}
            </button>
          ))}
        </nav>

        {problem && <p className="access-problem">{problem}</p>}

        {tab === "accounts" && (
          <section className="access-section">
            <table className="access-table">
              <thead>
                <tr>
                  <th>Account</th>
                  <th>Kind</th>
                  <th>Root</th>
                  <th>Shares</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {(accounts ?? []).map((a) => (
                  <tr key={a.name} className={a.disabled ? "access-off" : undefined}>
                    <td>
                      <code>{a.name}</code>
                      {a.disabled && <span className="access-tag">disabled</span>}
                    </td>
                    <td>{a.kind}</td>
                    <td>{a.root ? <code>{a.root}</code> : <span className="muted">shares under aliases</span>}</td>
                    <td>{a.shares}</td>
                    <td className="access-row-actions">
                      <button
                        type="button"
                        className="btn btn-small"
                        disabled={busy}
                        onClick={() =>
                          void guard(async () => reloadAccounts((await api.setAccountEnabled(a.name, a.disabled)).accounts))
                        }
                      >
                        {a.disabled ? "Enable" : "Disable"}
                      </button>
                      {a.root && (
                        <button
                          type="button"
                          className="btn btn-small"
                          disabled={busy}
                          title="Hold its share under an alias instead of at the root: every path it sees gains a /<alias> prefix"
                          onClick={() => void guard(async () => reloadAccounts((await api.convertAccount(a.name)).accounts))}
                        >
                          Convert
                        </button>
                      )}
                      <button
                        type="button"
                        className="btn btn-small"
                        onClick={() => {
                          setSharesOf({ account: a.name });
                          setTab("shares");
                        }}
                      >
                        Shares
                      </button>
                    </td>
                  </tr>
                ))}
                {accounts?.length === 0 && (
                  <tr>
                    <td colSpan={5} className="muted">
                      No accounts. The store answers as its owner until one is made.
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
            <NewAccount busy={busy} onCreate={(body) => guard(async () => reloadAccounts((await api.createAccount(body)).accounts))} />
          </section>
        )}

        {tab === "shares" && (
          <section className="access-section">
            <div className="access-filter">
              <span>
                {sharesOf.path ? (
                  <>
                    who can reach <code>{sharesOf.path}</code>
                  </>
                ) : sharesOf.account ? (
                  <>
                    what <code>{sharesOf.account}</code> holds
                  </>
                ) : (
                  "every share"
                )}
              </span>
              {(sharesOf.account || sharesOf.path) && (
                <button type="button" className="btn btn-small" onClick={() => setSharesOf({})}>
                  Show all
                </button>
              )}
            </div>
            <table className="access-table">
              <thead>
                <tr>
                  <th>Account</th>
                  <th>Sees it as</th>
                  <th>Rights</th>
                  <th>Folder in the store</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {(shares ?? []).map((s) => (
                  <tr key={`${s.account}/${s.alias}`}>
                    <td>
                      <code>{s.account}</code>
                    </td>
                    <td>
                      <code>/{s.alias}</code>
                      {s.dormant && <span className="access-tag">its folder is in the trash</span>}
                    </td>
                    <td>
                      <span className={`rights rights-${s.rights}`}>{s.rights}</span>
                    </td>
                    <td>{s.path ? <code>{s.path}</code> : <span className="muted">—</span>}</td>
                    <td className="access-row-actions">
                      <button
                        type="button"
                        className="btn btn-small"
                        disabled={busy}
                        onClick={() => {
                          const to = prompt(`Rename ${s.account}'s share /${s.alias} to:`, s.alias);
                          if (to && to !== s.alias) {
                            void guard(async () => setShares((await api.renameShare(s.account, s.alias, to)).shares));
                          }
                        }}
                      >
                        Rename
                      </button>
                      <button
                        type="button"
                        className="btn btn-small btn-danger"
                        disabled={busy}
                        title="Its checkout keeps the files: the store answers forbidden for them, not not-found, so nothing on disk is deleted"
                        onClick={() => void guard(async () => setShares((await api.revokeShare(s.account, s.alias)).shares))}
                      >
                        Revoke
                      </button>
                    </td>
                  </tr>
                ))}
                {shares?.length === 0 && (
                  <tr>
                    <td colSpan={5} className="muted">
                      Nothing is shared here.
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
            <NewShare
              busy={busy}
              accounts={accounts ?? []}
              folder={sharesOf.path ?? folder ?? ""}
              onGrant={(body) =>
                guard(async () => {
                  await api.grant(body);
                  setShares(await api.shares(sharesOf));
                  setAccounts(await api.accounts());
                })
              }
            />
          </section>
        )}

        {tab === "tokens" && (
          <section className="access-section">
            {minted && (
              <div className="access-minted">
                <p>
                  A bearer for <code>{minted.account}</code>. It is stored hashed and cannot be shown again:
                </p>
                <code className="access-bearer">{minted.bearer}</code>
                <button type="button" className="btn btn-small" onClick={() => void navigator.clipboard?.writeText(minted.bearer)}>
                  Copy
                </button>
                <button type="button" className="btn btn-small" onClick={() => setMinted(null)}>
                  Done
                </button>
              </div>
            )}
            <table className="access-table">
              <thead>
                <tr>
                  <th>Account</th>
                  <th>Label</th>
                  <th>Created</th>
                  <th>Expires</th>
                  <th>Last used</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {(tokens ?? []).map((t) => (
                  // Not usable is not only revoked: a token past its expiry is as dead as one
                  // taken away, and the store already says which with `live`.
                  <tr key={t.id} className={t.live ? undefined : "access-off"}>
                    <td>
                      <code>{t.account}</code>
                    </td>
                    <td>{t.label ?? <span className="muted">—</span>}</td>
                    <td className="mono">{t.created_at.slice(0, 10)}</td>
                    <td className="mono">{t.expires_at ? t.expires_at.slice(0, 10) : <span className="muted">never</span>}</td>
                    <td className="mono">{t.last_used_at ? t.last_used_at.slice(0, 10) : <span className="muted">never</span>}</td>
                    <td className="access-row-actions">
                      {!t.live ? (
                        <span className="access-tag">{t.revoked_at ? "revoked" : "expired"}</span>
                      ) : (
                        <button
                          type="button"
                          className="btn btn-small btn-danger"
                          disabled={busy}
                          onClick={() => void guard(async () => setTokens((await api.revokeToken(t.id)).tokens))}
                        >
                          Revoke
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
                {tokens?.length === 0 && (
                  <tr>
                    <td colSpan={6} className="muted">
                      No tokens.
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
            <NewToken
              busy={busy}
              accounts={accounts ?? []}
              onMint={(body) =>
                guard(async () => {
                  const answered = await api.createToken(body);
                  setMinted({ account: body.account, bearer: answered.bearer });
                  setTokens(await api.tokens());
                })
              }
            />
          </section>
        )}
    </dialog>
  );
}

function NewAccount({ busy, onCreate }: { busy: boolean; onCreate: (body: { name: string; kind?: string; root?: string }) => void }) {
  const [name, setName] = useState("");
  const [kind, setKind] = useState("agent");
  const [root, setRoot] = useState("");
  return (
    <form
      className="access-form"
      onSubmit={(e) => {
        e.preventDefault();
        if (name.trim()) onCreate({ name: name.trim(), kind, root: root.trim() || undefined });
      }}
    >
      <input value={name} placeholder="account name" onChange={(e) => setName(e.target.value)} spellCheck={false} />
      <select value={kind} onChange={(e) => setKind(e.target.value)} aria-label="Kind">
        <option value="agent">agent</option>
        <option value="person">person</option>
        <option value="admin">admin — the whole store</option>
      </select>
      <input
        value={root}
        placeholder="single root, e.g. /handbook (optional)"
        onChange={(e) => setRoot(e.target.value)}
        spellCheck={false}
        disabled={kind === "admin"}
        title="With a root, its one share is that folder and it sees it at /"
      />
      <button type="submit" className="btn btn-small" disabled={busy || !name.trim()}>
        Create
      </button>
    </form>
  );
}

function NewShare({
  busy,
  accounts,
  folder,
  onGrant,
}: {
  busy: boolean;
  accounts: AccountRow[];
  folder: string;
  onGrant: (body: { account: string; path: string; rights: "ro" | "rw"; alias?: string }) => void;
}) {
  const [account, setAccount] = useState("");
  const [path, setPath] = useState(folder);
  const [rights, setRights] = useState<"ro" | "rw">("ro");
  const [alias, setAlias] = useState("");
  useEffect(() => setPath(folder), [folder]);
  return (
    <form
      className="access-form"
      onSubmit={(e) => {
        e.preventDefault();
        if (account && path.trim()) onGrant({ account, path: path.trim(), rights, alias: alias.trim() || undefined });
      }}
    >
      <select value={account} onChange={(e) => setAccount(e.target.value)} aria-label="Account">
        <option value="">account…</option>
        {accounts.map((a) => (
          <option key={a.name} value={a.name}>
            {a.name}
          </option>
        ))}
      </select>
      <input value={path} placeholder="/folder" onChange={(e) => setPath(e.target.value)} spellCheck={false} />
      <select value={rights} onChange={(e) => setRights(e.target.value as "ro" | "rw")} aria-label="Rights">
        <option value="ro">ro — read</option>
        <option value="rw">rw — read and write</option>
      </select>
      <input
        value={alias}
        placeholder="as (the folder's own name by default)"
        onChange={(e) => setAlias(e.target.value)}
        spellCheck={false}
      />
      <button type="submit" className="btn btn-small" disabled={busy || !account || !path.trim()}>
        Grant
      </button>
    </form>
  );
}

function NewToken({
  busy,
  accounts,
  onMint,
}: {
  busy: boolean;
  accounts: AccountRow[];
  onMint: (body: { account: string; label?: string; expires?: string }) => void;
}) {
  const [account, setAccount] = useState("");
  const [label, setLabel] = useState("");
  const [expires, setExpires] = useState("");
  return (
    <form
      className="access-form"
      onSubmit={(e) => {
        e.preventDefault();
        if (account) onMint({ account, label: label.trim() || undefined, expires: expires.trim() || undefined });
      }}
    >
      <select value={account} onChange={(e) => setAccount(e.target.value)} aria-label="Account">
        <option value="">account…</option>
        {accounts.map((a) => (
          <option key={a.name} value={a.name}>
            {a.name}
          </option>
        ))}
      </select>
      <input value={label} placeholder="label (optional)" onChange={(e) => setLabel(e.target.value)} spellCheck={false} />
      <input
        value={expires}
        placeholder="expires: 30d, 12h, or an instant"
        onChange={(e) => setExpires(e.target.value)}
        spellCheck={false}
      />
      <button type="submit" className="btn btn-small" disabled={busy || !account}>
        Mint
      </button>
    </form>
  );
}
