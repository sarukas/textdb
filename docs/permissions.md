# Who may do what, and what the asset stores do not yet know about it

textdb has a delegated-access model: accounts, shares, tokens, and a view the store itself
enforces. The asset stores were built before it and knew nothing about it. This note says what the
model is, where the assets work did not fit it, what has been done about that, and what is left.

`docs/cli.md` has the user-facing side ("Delegating folders to accounts"); this is the
implementer's view.

## What the store enforces

- **The owner** is whoever opens the store without a token: on SQLite anyone who can open the
  file, on Postgres a superuser. Both already mean "you own the store".
- **An account** (`--kind agent` or `person`) is given whole folders and sees nothing else. A
  share is a folder and everything below it — no partial folders, no deny rules — under an
  alias belonging to the grant, with rights `ro` or `rw`.
- **A token** is a bearer, printed once and stored hashed, optionally expiring, revocable.
  `authenticate` is called once per connection and everything afterwards is answered in that
  account's view.
- **The rule lives in the store, not the CLI**, because the CLI is not the only caller:
  SQLite applies a visibility predicate to its queries, Postgres answers through `kb.entry`
  and row-level security, and both refuse the admin operations for a token session rather
  than trusting a client to do it.
- **Paths are per view; ids are not.** `id:1234` is what one view can hand another.
- **`forbidden` is not `not found`**: TX005 (exit 7) under an alias the account has or had,
  TX003 (exit 5) otherwise — the distinction that stops a sync deleting a checkout when a
  share is revoked.
- **`--author` is refused on a token session**: an account writes as itself. (For the owner
  `--author` remains self-asserted, which is provenance, not identity.)

## Where the assets work does not fit it

Until this branch there was not one `whoami` under `crates/textdb-cli/src/assets/`, and no test
paired an asset with an account or a token. Five consequences, four of them now answered: the first
lost data, the second laid a store out differently for every caller, the third let a pointer name
bytes it was never granted, and the fifth left a store's own row ownerless. What is open is the
fourth, the provider being a second authority: rclone runs with each person's own credentials, and
no rule inside textdb can constrain a drive. What follows from that is said rather than solved — a
refusal is told apart from a failure, and each state says whose fact it is.

### 1. A delete can take away bytes another account still points at

`InUse` reads `st.file_heads("/")` to answer "does any pointer other than this one name these
bytes?". That read is account-scoped by design (SQLite `visible()`, Postgres `kb.entry`), so
on a token session it answers from the pointers **this account can see**. Three decisions are
made from that answer, and each is wrong in the unsafe direction when a pointer is invisible:

- **A sync trashes the file in the provider** when a pointer is deleted and "nothing else
  names it". Account A deletes its pointer; account B's pointer to the same bytes is not in
  A's view; the file goes to the drive's trash, and B's asset is left naming bytes that are
  on a 30-day clock. Nothing textdb offers puts them back: `push` only takes `new` and
  `modified` assets.
- **A push replaces bytes in place** when no other pointer names the location.
- **`verify` lists a store's files that "no pointer names"**, which is read as "nothing needs
  these", and someone acts on it by hand.

**Done.** `Store::asset_item_users(store, location, own)` asks exactly that, over every pointer the
store holds rather than the caller's view, and answers in counts: how many other pointers name the
location, and how many pointers could not be read at all. Which asset needs the bytes, and where it
is, never leave the binding — a session that cannot see a folder does not learn its paths from an
answer about somebody else's file.

SQLite reads its own tables with no visibility predicate and no translation into the caller's
paths. Postgres asks `row_security_active('kb.node')` first: where the policy is enforced for that
connection the raw table is filtered too, so it answers "cannot tell" and nothing is taken away;
an owner's connection, which is what the CLI has, reads the table whole. Either way the asset's own
path is translated out of the caller's namespace before it is compared, or an account's own pointer
counts as a stranger's and its bytes are never its own to replace.

`verify` asks the set-shaped form of the same question, `Store::asset_items_named`: which of the
places a store holds any pointer names, over all of them, yes or no and nothing else. So an account
is told what its store holds that nothing needs, without a file another account's pointer names
being called unnamed -- M13.

One thing stays conservative, deliberately: a Postgres connection under an enforced policy is told
"cannot tell", and then nothing is taken away and nothing is called unneeded.

### 2. What a push records is the owner's path

**Done.** A push puts an asset's bytes where the owner's path for it says, whoever pushed: the
caller's path goes through `Store::owner_paths` first, which is lexical on both engines
(`access::to_store`, `kb.to_store`), so it answers for a path nothing is at yet — which is where a
push is when it asks.

Before this, the same document's asset landed at `<root>/legal/contracts/x.png` for the owner and
`<root>/contracts/x.png` for an account holding that folder as `/contracts`. A store's layout
depended on who pushed to it, two accounts with different aliases for one folder filled two places
with one asset, and an alias could collide with a real folder of the same name.

What it costs: a pointer names its item explicitly, so an account's pointer carries the owner's
path for its bytes — the layout its own paths are a projection of. Leaving the item out where it
equals the asset's own place would hide that, and is wrong: a pointer is a document, so it is
copied and it is moved, and an item meaning "wherever this pointer is now" would have a copy naming
bytes nobody put there and a move quietly changing what an asset is made of. That disclosure is
accepted, and worth knowing before granting a share to somebody who must not learn the central
layout.

### 3. A pointer names bytes; nothing checks that they are the account's to name

A pointer's `item` is whatever the pointer says. For a path-addressed store (`local`, or an
rclone remote that is not Drive) an account with `rw` on its own share can write a pointer
naming `/accounts/someone-else/secret.pdf` and pull those bytes into its checkout. For a
Drive store the item is a file id, so the same move needs an id the account has seen — which
narrows it without closing it. The store root itself is the only boundary the driver enforces
(`find_id` refuses ids outside it), and that root is exactly where every account's assets are.

**Done.** A store lays its files out in the owner's paths, so an item is a path like any other, and
`Store::may_name` asks the store the question it already answers about one: may this caller address
it (`view.to_view` on SQLite, `kb.to_view` on Postgres). A pull refuses to fetch where it says no, a
push refuses to write there — which matters more, since it would put bytes over somebody else's
— and `verify` says so rather than reading it. M12 holds a pointer naming a folder the account
was never granted, and watches the owner pull those same bytes afterwards.

A drive's file id is not a place in a namespace, so it is not what the question is asked of: the
drive is asked where it keeps that file, and its answer — a store path, since the store is laid
out in the owner's paths — is what has to be nameable. An id taken from another share's
pointer therefore buys nothing: it resolves to a path this session may not name.

A store with no file of that id has no place to ask after, and no bytes to reach either, so that
passes and whatever comes next fails on its own account. A store that could not be *asked* is not
the same answer and is not read as one: a listing that failed says nothing about who may name what,
and it is carried up as the failure it is. Nothing is refused that a fetch would have gone on to do,
since the fetch needs that same listing.

### 4. The provider is a second authority, and the two say nothing about each other

rclone remotes are configured per computer with a person's own token. An account's rights in
textdb neither grant nor withhold provider access: `ro` on a share does not stop someone who
has the drive from fetching the bytes, and `rw` does not mean the provider will accept a
write. Consequently a store-side state (`changed-in-store`, `trashed-in-store`, …) is a fact
about *this computer's* access, not about the asset — which is not how it currently reads.

Related, and unchanged from before: a provider's "you may not" arrives looking exactly like
"it did not work". A denial is durable and needs saying; a failure is worth retrying. They
should not share a message.

### 5. Store rows have no owner

`kb.asset_store(name, driver, root, options)` was data in the store like any other, and a pointer
names its store by name. Whoever could write those rows could point a name somewhere else and
thereby change where every asset of that name is read from and written to. Whether an account could
see or change them at all was stated nowhere.

**Done, in two halves.** *Writing* a row is the owner's: `put_asset_store` and `remove_asset_store`
are refused for a token session by the store rather than by the CLI — on SQLite in its store
module, on Postgres by a trigger on `kb.asset_store` (`asset_store_owner_only`), so a hand-written
`INSERT` meets the rule as well. M14 holds an account trying all three moves: a store of its own, the
team's store pointed at a folder of its own, and the declaration taken away.

*Reading* a row stays open to accounts, deliberately: the store names come from that table, so an
account that cannot see it cannot pull at all, and what it learns — a folder or a remote path like
`teamdrive:textdb` — is not a credential. Those stay per person (problem 4).

Repointing is refused separately, and for a different reason: `--add` on a name whose row points
elsewhere, and `--remove` of a name any pointer names, are refused while pointers name it, because
the bytes are where the old root says. That is the owner being stopped from breaking their own
store, not a boundary against an account; the thing that would do it properly is a store-to-store
move of the bytes, written up as "Moving a store (not built)" in [`assets.md`](assets.md).

## What to do, in order

1. ~~Refuse the unsafe decisions on a token session.~~ **Done**, and still what happens wherever
   the store cannot answer the question below.
2. ~~Add the store-answered question.~~ **Done**, in both shapes: `Store::asset_item_users` for
   one place (M11, two accounts naming one file) and `Store::asset_items_named` for a store's
   whole listing (M13, `verify` telling an account what nothing needs).
3. ~~Bind a pointer's item to what the account may name.~~ **Done**: `Store::may_name`, refused in
   pull, push and verify, with M12 covering it — and through a drive's id too, by resolving the
   id to the place the drive keeps it and asking after that.
4. ~~Decide whether `asset_store` rows are visible to accounts, and record who changed one.~~
   **Done**: readable by design (an account cannot pull otherwise), writable only by the owner, and
   a repoint refused while pointers name the store. No audit row: after those two, the only caller
   who can change one is the owner, and a repoint that would matter no longer happens. What a
   record would be *for* arrives with the move that makes repointing legitimate.
5. ~~Give a provider's denial its own state.~~ **Done**: the driver reads what the provider said
   (a 403 naming its reason, which rclone passes through), carries it as `forbidden` rather than
   as a failure, and an asset whose store refused this computer is `not-permitted` — over an `ok`
   one, as the other store-side states are. `verify` says `refused:` where it used to say
   `unchecked:`. Whose fact it is, is said on every surface that shows it: the note beside the
   state in `assets status` ("its asset store team refused *this computer* the file"), `verify`'s
   two columns, the state table in [`assets.md`](assets.md), and in the web pane both the note and
   the badge's own words.

   The one structural question this left — whether an asset's `state` should split into an
   asset-side field and a store-side one, as `verify`'s two columns do — is **decided: it stays one
   field.** `conflict` is why. It is a here-state that only a store-side fact produces (changed here
   *and* there), so two fields would have to be recombined by every reader to get back the one
   summary they are handed now, and the surface that most needs a single answer — the web pane's
   badge — would be the one recombining. `state` is the worst thing known about an asset, `note`
   says whose fact it is, and `verify` is where the two sides are shown apart.

Nothing here is left open. Outside this file: the store-to-store move of an asset store's bytes,
designed and deliberately not built (see [`assets.md`](assets.md), "Moving a store").

## What runs it

`tests/access.rs` — the acceptance catalogue of delegated access, and M11, M12 and M14 with it — sits
behind the `access-tests` feature, which for a while no CI job asked for: `TEXTDB_TEST_PG` was set
only for `cli_pg`, so L5's own requirement that every scenario run on both engines was met nowhere,
and the Postgres halves of these answers were exercised by nothing. It runs in the `pg-extension`
job now, which is the one with the extension installed and a server up, so both engines answer
there.

## In the web app

The *Access* panel (the owner's) is these commands: accounts, shares with their alias and
rights, tokens. The session chip is who the browser is -- the owner, or the account whose token
it presented -- and every path shown is then that account's own, with a `ro` marked where a
share is read-only. Nothing there re-derives the rule: what an account may not do arrives as
the store's refusal, and a folder it was never granted is missing rather than forbidden. What
the server keeps to itself is the machine it runs on: sync and every asset route are the owner's
-- or an `admin`-kind token's, which is what a hosted deployment has instead of an owner -- and
that is §4 above made visible rather than hidden ([demo-app.md](demo-app.md),
[live-app.md](live-app.md#who-is-asking)).

## Not in scope here

Whether textdb should mediate the bytes itself — an asset store the store proxies, so that a
share's rights govern the bytes as well as the documents — is a product question. Today the
provider is the authority on its own files, and a rule inside textdb cannot constrain it.
