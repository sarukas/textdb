import type { ErrorCode } from '@textdb/node';
import { CodedError, badRequest } from './errors.ts';
import { runCli } from './sync.ts';

/** One account, as `textdb account ls --json` reports it. */
export interface AccountRow {
  name: string;
  kind: string;
  root: string | null;
  created_at: string;
  disabled: boolean;
  shares: number;
}

/** One token, without its bearer -- which exists once, in the answer that minted it. */
export interface TokenRow {
  id: number;
  account: string;
  label: string | null;
  created_at: string;
  expires_at: string | null;
  last_used_at: string | null;
  revoked_at: string | null;
}

/** One share: which account holds which folder, under which name, by which rights. */
export interface ShareRow {
  account: string;
  alias: string;
  rights: string;
  /** Where the share root is in the store. The owner sees this; an account never does. */
  store_path: string | null;
  node_id: number;
  /** Its folder is in the trash: still held, not reachable today. */
  dormant?: boolean;
}

/**
 * Accounts, tokens and shares, run through the CLI.
 *
 * Every one of these is the owner's, and the *store* is what refuses a token session -- `admin_only`
 * in the SQLite store module, `kb.*` in the Postgres extension. So this passes the request's bearer
 * to the CLI rather than running as the owner and deciding for itself: a rule re-implemented here
 * would be a second opinion, and the one that matters is the store's. It also means an `admin`-kind
 * account's token works, which is the deployment that kind exists for.
 *
 * The CLI is used rather than the SDK because that is where these commands live, and because it
 * speaks both engines: a central store on Postgres is configured exactly like a local one.
 */
export class AccessService {
  private readonly store: string;
  private readonly cli: string | null;

  constructor(store: string, cli: string | null) {
    this.store = store;
    this.cli = cli;
  }

  get available(): boolean {
    return this.cli !== null;
  }

  accounts(bearer?: string): Promise<AccountRow[]> {
    return this.run<AccountRow[]>(['account', 'ls'], bearer);
  }

  createAccount(name: string, kind: string | undefined, root: string | undefined, bearer?: string): Promise<unknown> {
    const args = ['account', 'create', name];
    if (kind) args.push('--kind', kind);
    if (root) args.push('--root', root);
    return this.run(args, bearer);
  }

  setAccountEnabled(name: string, enabled: boolean, bearer?: string): Promise<unknown> {
    return this.run(['account', enabled ? 'enable' : 'disable', name], bearer);
  }

  convertAccount(name: string, alias: string | undefined, bearer?: string): Promise<unknown> {
    const args = ['account', 'convert', name];
    if (alias) args.push('--as', alias);
    return this.run(args, bearer);
  }

  tokens(account: string | undefined, bearer?: string): Promise<TokenRow[]> {
    const args = ['token', 'ls'];
    if (account) args.push(account);
    return this.run<TokenRow[]>(args, bearer);
  }

  /** The bearer is in this answer and nowhere else, ever again. */
  createToken(account: string, label: string | undefined, expires: string | undefined, bearer?: string): Promise<{ bearer: string }> {
    const args = ['token', 'create', account];
    if (label) args.push('--label', label);
    if (expires) args.push('--expires', expires);
    return this.run<{ bearer: string }>(args, bearer);
  }

  revokeToken(id: number, bearer?: string): Promise<unknown> {
    return this.run(['token', 'revoke', String(id)], bearer);
  }

  /** With an account, that account's shares; with a store path, who can see it. */
  shares(of: { account?: string; path?: string }, bearer?: string): Promise<ShareRow[]> {
    const args = ['access', 'ls'];
    if (of.account) args.push(of.account);
    else if (of.path) args.push(of.path);
    return this.run<ShareRow[]>(args, bearer);
  }

  grant(account: string, path: string, rights: string, alias: string | undefined, bearer?: string): Promise<ShareRow> {
    if (rights !== 'ro' && rights !== 'rw') throw badRequest('rights must be ro or rw');
    const args = ['access', 'grant', account, path, rights];
    if (alias) args.push('--as', alias);
    return this.run<ShareRow>(args, bearer);
  }

  renameShare(account: string, from: string, to: string, bearer?: string): Promise<unknown> {
    return this.run(['access', 'rename', account, from, to], bearer);
  }

  revokeShare(account: string, alias: string, bearer?: string): Promise<unknown> {
    return this.run(['access', 'revoke', account, alias], bearer);
  }

  /**
   * One CLI run, in the caller's own name.
   *
   * `--token` goes first among the global options and the bearer is one argument, so a token that
   * starts with a dash is a token and not a flag. The CLI answers JSON on stdout, errors included,
   * which is what carries a refusal's code up unchanged.
   */
  private async run<T>(args: string[], bearer?: string): Promise<T> {
    const cli = this.cli;
    if (!cli) {
      throw badRequest('The textdb CLI was not found: build it (cargo build --release -p textdb-cli) or set TEXTDB_CLI.');
    }
    const global = ['--store', this.store, '--json'];
    if (bearer) global.push(`--token=${bearer}`);
    const { stdout, stderr, status } = await runCli(cli, [...global, ...args]);
    const answered = parseJson(stdout) ?? parseJson(stderr);
    if (status !== 0) {
      const error = (answered as { error?: { code?: string; message?: string } } | undefined)?.error;
      const code = (error?.code ?? 'TX000') as ErrorCode;
      throw new CodedError(code, error?.message ?? (stderr.trim() || `textdb ${args.join(' ')} exited ${status}`));
    }
    return answered as T;
  }
}

function parseJson(text: string): unknown {
  const trimmed = text.trim();
  if (!trimmed) return undefined;
  try {
    return JSON.parse(trimmed);
  } catch {
    // The CLI prints notes on stderr as plain text; only one of the two streams is the answer.
    return undefined;
  }
}
