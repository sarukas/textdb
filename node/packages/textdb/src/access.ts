import { Cli, type CliOptions } from './cli.ts';

/**
 * Delegated access: accounts, the tokens they arrive with, and the shares they hold (#12).
 *
 * Every one of these is the owner's, and it is the **store** that says so -- `admin_only` on
 * SQLite, `kb.*` and a trigger on Postgres. So a session's own token is presented and the answer
 * is whatever the store gives: a rule re-implemented here would be a second opinion, and an
 * `admin`-kind account's token works, which is the deployment that kind exists for.
 */

/**
 * One account, as `textdb account ls --json` reports it.
 *
 * A field the CLI has nothing to put in is left out rather than sent as null, so the optional ones
 * here are optional in the answer too.
 */
export interface AccountRow {
  name: string;
  /** `agent`, `person` or `admin`. */
  kind: string;
  /** The store path of a single-root account's root, when it has one instead of aliased shares. */
  root?: string;
  created_at: string;
  disabled: boolean;
  shares: number;
}

/** One token, without its bearer -- which exists once, in the answer that minted it. */
export interface TokenRow {
  id: number;
  account: string;
  label?: string;
  created_at: string;
  expires_at?: string;
  revoked_at?: string;
  last_used_at?: string;
  /** Usable right now: not revoked, not expired. */
  live: boolean;
}

/** One share: which account holds which folder, under which name, by which rights. */
export interface ShareRow {
  account: string;
  alias: string;
  rights: string;
  /**
   * The share root's store path. The owner sees it; an account's own `whoami` leaves it out, where
   * naming it would disclose the layout the alias exists to hide.
   */
  path?: string;
  node_id: number;
  /** Its folder is in the trash: still held, not reachable today. */
  dormant: boolean;
}

/**
 * Every value a caller supplies goes after `--`, or in a `--flag=value`.
 *
 * A name, alias, label or path is data, and `-weird` is a name somebody chose. Passed as a bare
 * argument it is a flag to the CLI's parser, which exits with a usage message and no JSON at all --
 * a 500 out of a server, for what is a perfectly ordinary bad request. `--` ends the options, and
 * `--flag=value` keeps a value that starts with a dash attached to its flag.
 */
export class Access {
  private readonly cli: Cli;

  constructor(options: CliOptions | Cli) {
    this.cli = options instanceof Cli ? options : new Cli(options);
  }

  /** Whether a CLI was found to run these with. */
  get available(): boolean {
    return this.cli.available;
  }

  /** The same commands as somebody else: a bearer belongs to a session, not to this object. */
  as(who: { token?: string; author?: string }): Access {
    return new Access(this.cli.as(who));
  }

  accounts(): Promise<AccountRow[]> {
    return this.cli.json<AccountRow[]>(['account', 'ls']);
  }

  createAccount(name: string, options: { kind?: string; root?: string } = {}): Promise<{ account: string; kind: string; root: string | null }> {
    const args = ['account', 'create'];
    if (options.kind) args.push(`--kind=${options.kind}`);
    if (options.root) args.push(`--root=${options.root}`);
    args.push('--', name);
    return this.cli.json(args);
  }

  /** Disabled: its tokens stop working, and its shares are kept for when it is enabled again. */
  setAccountEnabled(name: string, enabled: boolean): Promise<{ account: string; disabled: boolean }> {
    return this.cli.json(['account', enabled ? 'enable' : 'disable', '--', name]);
  }

  /**
   * Turn a single-root account into one that holds shares under aliases.
   *
   * One way only, and `--multi` is spelled out because it changes every path that account sees: its
   * root gains a `/<alias>` prefix, `alias` naming it (the folder's own name otherwise).
   */
  convertAccount(name: string, options: { alias?: string } = {}): Promise<unknown> {
    const args = ['account', 'convert', '--multi'];
    if (options.alias) args.push(`--root-alias=${options.alias}`);
    args.push('--', name);
    return this.cli.json(args);
  }

  tokens(account?: string): Promise<TokenRow[]> {
    const args = ['token', 'ls'];
    if (account) args.push('--', account);
    return this.cli.json<TokenRow[]>(args);
  }

  /** The bearer is in this answer and nowhere else, ever again: it is stored as a hash. */
  createToken(
    account: string,
    options: { label?: string; expires?: string } = {},
  ): Promise<{ bearer: string; id: number; account: string; expires_at: string | null }> {
    const args = ['token', 'create'];
    if (options.label) args.push(`--label=${options.label}`);
    if (options.expires) args.push(`--expires=${options.expires}`);
    args.push('--', account);
    return this.cli.json(args);
  }

  revokeToken(id: number): Promise<{ revoked: number }> {
    return this.cli.json(['token', 'revoke', '--', String(id)]);
  }

  /** With an account, that account's shares; with a store path, who can reach it. */
  shares(of: { account?: string; path?: string } = {}): Promise<ShareRow[]> {
    const args = ['access', 'ls'];
    const which = of.account ?? of.path;
    if (which) args.push('--', which);
    return this.cli.json<ShareRow[]>(args);
  }

  grant(account: string, path: string, rights: 'ro' | 'rw', options: { alias?: string } = {}): Promise<ShareRow> {
    const args = ['access', 'grant'];
    if (options.alias) args.push(`--as=${options.alias}`);
    args.push('--', account, path, rights);
    return this.cli.json<ShareRow>(args);
  }

  /** The name the account knows the share by, which is not the path in the store. */
  renameShare(account: string, from: string, to: string): Promise<unknown> {
    return this.cli.json(['access', 'rename', '--', account, from, to]);
  }

  revokeShare(account: string, alias: string): Promise<unknown> {
    return this.cli.json(['access', 'revoke', '--', account, alias]);
  }
}
