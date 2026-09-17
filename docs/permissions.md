# Who may do what

This note is about permissions in textdb: what the store enforces today, what the asset
stores added to the question, and what would have to be decided before textdb could claim
to control access at all. It proposes options; it settles nothing.

## What textdb enforces today: nothing

There is no permission model, and it is worth being plain about that rather than implying a
gap in an otherwise-complete scheme:

- No principals, roles, groups or owners. A document has authors, not an owner.
- No access-control lists, and no row-level security on the Postgres side. A connection that
  can read the store can read every document in it, including every version in its history
  and everything in its trash.
- No server authentication. `textdb-server` serves whoever reaches it; its only concession is
  that CORS allows local origins.
- `--author` is self-asserted. Nothing verifies it, and nothing stops one person writing as
  another. It is provenance for humans reading history, not identity.
- The `textdb` SQL views and functions run with the caller's rights. There is no
  `SECURITY DEFINER` boundary, so no privilege is held on a caller's behalf.

The enforcement that does exist is the enforcement of whatever holds the store: filesystem
permissions on a SQLite file, and database grants (`CONNECT`, table privileges) on Postgres.
That is real, but it is all-or-nothing per store -- there is no "this folder, this person".

So "supporting central/local vault permission handling like the rest of textdb now does" has
no existing model to match. Anything here is new design.

## What the asset stores changed

Assets moved bytes out of the store and into somebody's provider -- a Google Drive, an rclone
remote, a folder on disk. That introduces a second authority with its own idea of who may do
what, and four problems follow from it.

### 1. An `asset_store` row has no owner

`kb.asset_store` holds a name, a driver and a root. Anyone who can write the store can add a
store, point it anywhere that machine can reach, or repoint an existing one. A pointer names
its store by name, so repointing a name silently changes where every asset of that name is
read from and written to.

On a shared (central) store this is the sharpest edge in the current design: it is a write
that redirects other people's reads and writes, and nothing records who made it.

Options:

- **Rows are data, and the store's write permission is the control.** Simplest, and honest
  about the fact that a shared store is already a shared trust boundary. Add an audit trail
  (who added or changed a store row, when) so a redirect is at least visible.
- **Rows are owned.** Add a declared owner to a store row and refuse changes from anyone
  else. Requires identity, which textdb does not have, so it would be self-asserted like
  `--author` -- a speed bump, not a control.
- **Rows are configuration, not data.** Move store definitions out of the store and into each
  computer's own configuration, so a central store carries pointers but never the definition
  of where the bytes live. Strongest separation; costs the convenience of a vault that works
  as soon as you clone it, and means a pointer's `store` name must resolve per machine.

### 2. Provider credentials are per person, and textdb does not hold them

An rclone remote is configured on a computer, by a person, with that person's token. Two
people syncing one central store will read the same pointers through different credentials
and therefore see different things: one may read an asset, another may not, and a third may
have write access where the first has read-only.

This is arguably correct -- the provider is the authority on its own files, and textdb should
not be in the business of holding a team's credentials. But it means a store-side state is a
statement about *this* computer's access, not about the asset. It should be presented that
way, and it currently is not.

### 3. A pointer tells every reader where the bytes are

A `.tdbasset` pointer carries `store`, `item` and now `item-path`. Anyone who can read the
document can read those -- a Drive file id, and a path that may itself be informative
(`/clients/acme/2026-layoffs.pdf`). The pointer does not grant access: a reader still needs
the provider's permission to fetch the bytes. But it does leak metadata, and on Drive a file
id is exactly what a share link is built from, so it is not nothing.

The rule worth keeping, whatever else is decided: **a pointer must never carry anything that
grants access.** No tokens, no signed URLs, no share links. Today it does not, and that
should be a stated invariant rather than an accident.

### 4. "You may not" and "it did not work" look the same

A provider's denial arrives as a failed rclone run. textdb currently reports that as a store
that could not be reached -- the same as a network failure, an expired token, or a drive that
is simply down. The consequences differ sharply: a transient failure should be retried and
says nothing about the asset, while a denial is a durable fact about this person and this
file that no amount of retrying will change, and that the person needs to see in order to go
and ask someone for access.

This is the one item here with an obvious answer and no dependency on a permission model:
distinguish a denial from a failure, give it a state of its own (`not-permitted`, say),
report it with the file it concerns, and never retry it silently. It also needs to be a state
that only ever replaces `ok` in the same way the other store-side states do, and must never
be mistaken for "the asset is gone" -- a denial is not grounds for trashing or re-pushing
anything.

## If textdb were to control access itself

Not proposed -- recorded so the shape is known before anybody starts:

- Identity has to come first, and has to be verified rather than asserted. Until then every
  other control is a suggestion.
- Any document scope (a "this folder, these people" rule) belongs in the `Store` trait and in
  SQL, not in the CLI. Enforcement in a client is not enforcement: the store is reachable by
  other clients, by `textdb sql`, and by psql.
- Postgres could enforce a scope with row-level security and `SECURITY DEFINER` functions;
  SQLite cannot, so the two backends would stop being equivalent. That divergence needs a
  decision, not a discovery.
- History and trash must be in scope from the start. A rule that hides a document but leaves
  its versions, its hunks and its trashed copies readable hides nothing.
- Assets are the hard part: a rule inside textdb cannot constrain a provider, so a document
  nobody may read can still have bytes anybody with the drive may fetch. Either the provider
  is the authority (and textdb's rules are advisory for assets), or assets need a store that
  textdb itself mediates.

## What this work should do next, in order

1. Distinguish a provider's denial from a transient failure, with its own state and message.
   No design decisions blocked on anybody.
2. State the pointer invariant in the format documentation: a pointer carries what finds
   bytes, never what grants access to them.
3. Record who changed an `asset_store` row, and say in the documentation that adding or
   repointing a store is a write that redirects everyone else's reads.
4. Present store-side states as facts about this computer's access where that is what they
   are, rather than as facts about the asset.

Anything past that waits on a decision about identity, which is a product question rather
than an implementation one.
