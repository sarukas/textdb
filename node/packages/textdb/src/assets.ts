import { Cli, type CliOptions } from './cli.ts';
import { errorOf } from './errors.ts';

/**
 * Asset stores: where the bytes of an asset actually live (`docs/assets.md`).
 *
 * Two halves that are easy to confuse and must not be. The **declaration** -- name, driver, root --
 * is a row in the textdb store, the same for everyone who opens it, and only the owner may write
 * one: every pointer naming that store would otherwise be pointed somewhere else for everybody.
 * The **binding** is this computer's, in this computer's config file, because the same shared drive
 * is mounted at a different place by each person.
 *
 * Only the stores here. The assets themselves belong to a directory synced with a folder, which is
 * a machine's own business rather than the store's, and stays where that machine's code is.
 */

export interface AssetStoreRow {
  name: string;
  /** `local` (a folder this computer reaches) or `rclone` (a remote, per person's configuration). */
  driver: string;
  /** The store-side identity, shared by everyone. */
  root: string;
  /** Where *this computer* reaches it, when it was bound here. */
  bound_to: string | null;
  /** The file or environment variable that said so. */
  bound_by: string | null;
  reachable: boolean;
  /** Why it is not reachable, when it is not: a fact about this computer, not about the store. */
  problem: string | null;
}

export class AssetStores {
  private readonly cli: Cli;

  constructor(options: CliOptions | Cli) {
    this.cli = options instanceof Cli ? options : new Cli(options);
  }

  get available(): boolean {
    return this.cli.available;
  }

  as(who: { token?: string; author?: string }): AssetStores {
    return new AssetStores(this.cli.as(who));
  }

  /** Every declared store, with whether this computer can reach each one. */
  list(): Promise<AssetStoreRow[]> {
    return this.cli.json<AssetStoreRow[]>(['assets', 'stores']);
  }

  /**
   * Declare a store, or point an existing one somewhere else.
   *
   * Refused while any pointer names its files: the bytes are where the old root says, and moving a
   * store means copying them and settling each pointer as it goes. Answers the whole list, because
   * that is what the command prints.
   */
  put(name: string, root: string, options: { driver?: string } = {}): Promise<AssetStoreRow[]> {
    const args = ['assets', 'stores', `--add=${name}`, `--driver=${options.driver ?? 'local'}`, `--root=${root}`];
    return this.cli.json<AssetStoreRow[]>(args);
  }

  /** Take a declaration away. Refused while any pointer names it, which would orphan those bytes. */
  remove(name: string): Promise<AssetStoreRow[]> {
    return this.cli.json<AssetStoreRow[]>(['assets', 'stores', `--remove=${name}`]);
  }

  /**
   * Bind a store to where **this computer** reaches it; `''` clears the binding.
   *
   * `--bind` takes `NAME=LOCATION` and splits at the first `=`, so a name carrying one would bind
   * some other store: refused here rather than silently rebinding a store nobody named.
   */
  async bind(name: string, location: string): Promise<AssetStoreRow[]> {
    if (name.includes('=')) throw errorOf('TX004', 'an asset store name cannot contain "="');
    return this.cli.json<AssetStoreRow[]>(['assets', 'stores', `--bind=${name}=${location}`]);
  }
}
