import { Access, type AccountRow, type ShareRow, type TokenRow } from '@textdb/node';
import { badRequest } from './errors.ts';

export type { AccountRow, ShareRow, TokenRow };

/**
 * Accounts, tokens and shares, for the HTTP routes.
 *
 * The commands themselves are the SDK's `Access` (`@textdb/node`), which runs the CLI: the rule for
 * each of these lives in the store -- `admin_only` on SQLite, `kb.*` and a trigger on Postgres --
 * and the CLI is what already knows how to ask. This server adds the one thing that is its own: the
 * request's bearer, so a command runs in the name of whoever made the request rather than as the
 * owner this server happens to have opened the store as. That is the whole of the difference
 * between a refusal and a privilege escalation here.
 *
 * It also means an `admin`-kind account's token works, which is the deployment that kind exists for.
 */
export class AccessService {
  private readonly access: Access;

  constructor(store: string, cli: string | null) {
    this.access = new Access({ store, cli });
  }

  /** Whether a CLI was found: without one, none of these routes can answer. */
  get available(): boolean {
    return this.access.available;
  }

  /** The commands as the session that asked for them. No bearer is the owner, as everywhere. */
  private as(bearer?: string): Access {
    return bearer ? this.access.as({ token: bearer }) : this.access;
  }

  accounts(bearer?: string): Promise<AccountRow[]> {
    return this.as(bearer).accounts();
  }

  createAccount(name: string, kind: string | undefined, root: string | undefined, bearer?: string): Promise<unknown> {
    return this.as(bearer).createAccount(name, { kind, root });
  }

  setAccountEnabled(name: string, enabled: boolean, bearer?: string): Promise<unknown> {
    return this.as(bearer).setAccountEnabled(name, enabled);
  }

  convertAccount(name: string, alias: string | undefined, bearer?: string): Promise<unknown> {
    return this.as(bearer).convertAccount(name, { alias });
  }

  tokens(account: string | undefined, bearer?: string): Promise<TokenRow[]> {
    return this.as(bearer).tokens(account);
  }

  /** The bearer is in this answer and nowhere else, ever again. */
  createToken(account: string, label: string | undefined, expires: string | undefined, bearer?: string): Promise<{ bearer: string }> {
    return this.as(bearer).createToken(account, { label, expires });
  }

  revokeToken(id: number, bearer?: string): Promise<unknown> {
    return this.as(bearer).revokeToken(id);
  }

  /** With an account, that account's shares; with a store path, who can see it. */
  shares(of: { account?: string; path?: string }, bearer?: string): Promise<ShareRow[]> {
    return this.as(bearer).shares(of);
  }

  grant(account: string, path: string, rights: string, alias: string | undefined, bearer?: string): Promise<ShareRow> {
    if (rights !== 'ro' && rights !== 'rw') throw badRequest('rights must be ro or rw');
    return this.as(bearer).grant(account, path, rights, { alias });
  }

  renameShare(account: string, from: string, to: string, bearer?: string): Promise<unknown> {
    return this.as(bearer).renameShare(account, from, to);
  }

  revokeShare(account: string, alias: string, bearer?: string): Promise<unknown> {
    return this.as(bearer).revokeShare(account, alias);
  }
}
