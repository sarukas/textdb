# Who may do what, and what the asset stores do not yet know about it

textdb has a delegated-access model: accounts, shares, tokens, and a view the store itself
enforces. The asset stores were built before it and know nothing about it. This note says
what the model is, where the assets work does not fit it, and what to do in what order.

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

Nothing under `crates/textdb-cli/src/assets/` asks who the caller is — there is not one
`whoami` in it — and no test pairs an asset with an account or a token. Four consequences,
the first of which loses data.

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

The fix is not to widen the account's view. It is to ask the store a question that is *not*
view-scoped — "is this item named by any pointer anywhere?" — because the question is about a
provider's bytes, which are shared across every account of the store. That belongs in the
`Store` trait and in both engines' SQL, for the same reason the rest of the rule does.

Until that exists, a token session must not make any of those three decisions.

### 2. A pointer names bytes; nothing checks that they are the account's to name

A pointer's `item` is whatever the pointer says. For a path-addressed store (`local`, or an
rclone remote that is not Drive) an account with `rw` on its own share can write a pointer
naming `/accounts/someone-else/secret.pdf` and pull those bytes into its checkout. For a
Drive store the item is a file id, so the same move needs an id the account has seen — which
narrows it without closing it. The store root itself is the only boundary the driver enforces
(`find_id` refuses ids outside it), and that root is exactly where every account's assets are.

### 3. The provider is a second authority, and the two say nothing about each other

rclone remotes are configured per computer with a person's own token. An account's rights in
textdb neither grant nor withhold provider access: `ro` on a share does not stop someone who
has the drive from fetching the bytes, and `rw` does not mean the provider will accept a
write. Consequently a store-side state (`changed-in-store`, `trashed-in-store`, …) is a fact
about *this computer's* access, not about the asset — which is not how it currently reads.

Related, and unchanged from before: a provider's "you may not" arrives looking exactly like
"it did not work". A denial is durable and needs saying; a failure is worth retrying. They
should not share a message.

### 4. Store rows have no owner

`kb.asset_store(name, driver, root, options)` is data in the store, and a pointer names its
store by name. Whoever can write those rows can point a name somewhere else and thereby
change where every asset of that name is read from and written to. Whether an account can see
or change those rows at all is not currently stated anywhere, and should be.

## What to do, in order

1. **Refuse the three unsafe decisions on a token session** — trashing or moving a file in a
   provider during a sync, replacing bytes in place during a push, and listing a store's
   unnamed files — each with a message saying why rather than a silent skip. This is a small
   change and it closes the data-loss path.
2. **Add the store-answered question** `is this item named by any pointer anywhere?`,
   unscoped, in the `Store` trait and both engines, and let a token session make those
   decisions again on its answer. Test it with two accounts naming one file.
3. **Bind a pointer's item to what the account may name**: refuse to pull or push an item that
   is neither the asset's own path nor an item already recorded by a pointer in the caller's
   view.
4. **Decide whether `asset_store` rows are visible to accounts**, and record who changed one.
5. **Give a provider's denial its own state**, distinct from a store that could not be
   reached, and present the store-side states as facts about this computer's access.

Steps 1 and 5 need no decisions from anyone. Steps 2–4 are design choices, and 2 is the one
that makes delegated access and asset stores actually compatible rather than merely coexisting.

## Not in scope here

Whether textdb should mediate the bytes itself — an asset store the store proxies, so that a
share's rights govern the bytes as well as the documents — is a product question. Today the
provider is the authority on its own files, and a rule inside textdb cannot constrain it.
