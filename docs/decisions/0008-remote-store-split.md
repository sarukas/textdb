# ADR 0008 — Split the CLI into a local client and a remote store, wire-optimal at the seam

Status: accepted; steps 1–3 implemented (step 3 without a carrier), steps 4–5 planned

## Context

textdb was designed with the store in-process (SQLite) or on localhost (Postgres). Its cost
model — chunk sharing, prolly trees, compare-and-swap commits, a 5 ms edit target — minimises
storage and commit latency, and it does that well. Bytes on a socket were never a term in it,
because on localhost a whole-file write costs the same round trip as a one-line one.

A **hosted central vault** (Postgres on Google Cloud, one store for every team, reached by
people, CI and AI agents from laptops and sandboxes with only a URL and a bearer token) makes
bytes and round trips the dominant cost. Three facts settle the shape of the split:

- Agent sandboxes cannot reach a Postgres port. Measured from a Claude Code on the web session:
  port 5432 outbound is blocked, port 443 is intercepted by a transparent HTTP proxy, and only
  HTTPS through an HTTP `CONNECT` proxy to allowlisted hosts works. Whatever carries the
  protocol must look like HTTPS from the outside, at least at first.
- Writes need the table owner today (`docs/USAGE.md`, "The trust boundary"), so a client that
  talks to Postgres directly holds the owner credential. The split must keep that credential on
  the server.
- The carrier is not decided. HTTPS reaches sandboxes now; WebSocket or Arrow Flight may win
  later for streams or tabular results. The seam must not know which.

## Where the waste is

The engine is already wire-shaped: every write becomes a diff, then chunks, then one root swap.
The waste is entirely in how `sync` drives the store: it reads and writes **whole files**, and
retains no base text, so a merge downloads the base and the current text and uploads the merged
one. With a file of lines `A B C`, agreed at version 1, and a local edit `C → D`, counting a line
crossing the wire as 1000 and a path, version, hash or hunk header as 10:

| Scenario | Theoretical floor | Before | After step 1 |
|---|---|---|---|
| Nothing changed | 20 | 100 | 70 |
| Local only, `C→D` | 1,030 | 3,140 | 1,120 |
| Remote only, `B→B'` | 1,030 | 3,120 | 1,130 |
| Both, different lines | 2,060 | 9,180 | 2,180 |
| Both, same line (conflict, then resolution) | 2,060 | 9,280 | 2,250 |
| Remote deleted, local edited | 1,080 | 3,140 | 1,120 (needs restore-then-apply) |
| Remote moved, local edited | 1,070 | 3,180 | 1,160 |
| Same edit made on both sides | 40 | 3,120 | 100 (needs hash on feed rows) |

Two terms make the "before" numbers, and neither grows with the *change*: every transfer grows
with the **file** (a 1,000-line file with one line edited costs a million points to push, three
million to merge), and the per-run heads listing grows with the **vault** (10,000 files cost
300,000 points before a line moves).

The primitives that fix the first term already exist in the `Store` trait
(`crates/textdb-cli/src/store/mod.rs`) on both engines: `replace_ranges` pushes line ranges
against a base version in one commit (`kb.replace_ranges` on Postgres), and `hunks` returns the
line hunks between two versions (`kb.hunks`). Only the CLI commands called them; `sync` did not.

## Decision

### 1. Fix the algorithm first, below the seam, before any protocol exists

- **Retain the base text.** A checkout keeps the content both sides agreed on at the last sync,
  keyed by the git blob id the base row already records, under `.textdb/base/`. It is the
  file as it stood before the edit; today it was thrown away and re-downloaded. With it, the
  local diff (base vs disk) needs no remote read. A file unchanged on disk *is* its own base,
  so pulls work without the cache too. The cache is verified by hash on read and is only ever
  an optimisation: missing or wrong, sync falls back to the whole-file path and repopulates it.
- **Pull by hunks.** The current text of a file that changed in the store is rebuilt from the
  retained base and `hunks(base_version, head_version)`, each hunk checked against the base
  text it claims to replace. `read` is the fallback.
- **Push by ranges.** A local change is sent as `replace_ranges` against the base version,
  computed by the core crate's Myers line diff. The ranges are applied locally to the base and
  compared byte for byte with the file on disk before they are sent; any mismatch (line-ending
  or final-newline subtleties) falls back to `write`, so the store never holds anything but the
  bytes on disk. A merge is pushed the same way against the head version.
- **Measure at the seam.** `TEXTDB_WIRE_STATS=<path>|-` wraps the store in a counter that
  records calls and bytes up and down per method, written on exit. It is the instrument the
  table above is checked with, on Postgres on localhost, before any carrier exists.

Measured with the counter on a 200-line file on SQLite (bytes, as the counter sizes them; the
first sync of the file is the 1,749-byte whole-file write every design pays):

| Scenario | Before | After | Byte-exact after |
|---|---|---|---|
| Local edit of one line | `write` 1,749 up | `replace_ranges` 83 up | yes |
| Store edit of one line | `read` 1,750 down | `hunks` 50 down | yes |
| Both, different lines | ≈ 5,300 | 50 down + 106 up | yes |
| Both, the same line, then resolved | ≈ 5,300 + 1,750 | 52 down, then 82 up | yes |
| Deletions, insertions, append, a lost or restored final newline, CRLF files | whole file | ranges or hunks | yes, every case |

`crates/textdb-cli/tests/cli.rs::sync_moves_changed_lines_not_whole_files` holds the assertions.

### 1b. Step 2: the vault-sized terms

Step 1 left three terms that grow with the vault rather than the change, each about fifty
bytes a file: the sync base's rows read at the start of every sync (`sync_base`), the listing
of every file's head (`file_heads`), and the whole base written back at the end
(`save_sync_base`). On a 2,000-file vault they were about 300 KB before a line moved. Step 2
removes all three without changing what sync decides:

- **The base rows live in the checkout too.** `.textdb/sync-rows.json` holds the directory's
  copy at the store's generation. A sync reads only the base's head row (`sync_head`) and uses
  the copy while the generation matches; another machine saving over the base bumps the
  generation, and the copy is read past and refreshed. An asset push, which writes rows
  without a generation bump, drops the copy.
- **The store answers the difference, not the listing.** `file_heads_delta(prefix, dir)`
  returns the files whose version is not their base row's (or that have no row) and the rows
  whose file is gone; sync rebuilds the full listing from its rows and that. On Postgres it is
  one join between `kb.entry` and `kb.sync_file`; on SQLite, where both sides are in-process,
  the difference is taken in Rust by the same helper. A renamed share alias falls back to the
  listing, since the rows' paths were rewritten locally.
- **The base is saved as a delta.** `save_sync_base_delta` carries the same compare-and-swap
  on the generation and only the rows that differ from the ones the sync started with, plus
  the paths to drop.
- **A rebased push lands on disk at once.** When the store rebases a commit over one that
  arrived meanwhile, `push` fetches the hunks from the held base to the new version, rebuilds
  the store's text and writes it to disk, recording the version. Before, the version was left
  unknown and the next sync read the file whole to find out; the content hash on write results
  that the table above asked for is not needed once the hunks are there.

Measured on the 2,000-file vault, SQLite, with the counter (bytes):

| Scenario | Before step 2 | After |
|---|---|---|
| Nothing changed | ≈ 300,000 | 470 (plus, once, the rows whose disk mtimes became trustworthy) |
| One local line edited | ≈ 300,000 + 1,750 | 1,081 |
| One store line edited | ≈ 300,000 + 1,750 | 1,295 |

Batch and the chunk have-and-put exchange move to step 3: a batch is a message-layer concern
and belongs with the `Request` enum, and the chunk exchange is the first piece of the replica.
Restore-then-apply for a re-created file stays open.

### 1c. Step 3: the seam as messages, with no wire yet

`crates/textdb-cli/src/proto.rs` is the message layer. Every `Store` method is a `Request`
variant with owned fields and a `Response` variant; a `Reply` is a response or a `StoreError`,
so a conflict crosses with its `TX001` code and detail intact. A `Codec` turns messages into
bytes and back (JSON first); a `Transport` carries them, with `call` and a `call_many` that
sends a `Batch` request answered reply by reply. `RemoteStore` is the `Store` on the near
side of a transport, and `dispatch` the far side: one request applied to a real store.

`Loopback` is the transport with no network: encode, decode, dispatch to a store in this
process, encode the reply, decode it. `TEXTDB_TEST_LOOPBACK=1` puts it under whatever store
the CLI opens, and the whole CLI test suite runs through it unchanged on both engines. That is
the proof the seam needed: every row type has been through the codec and back, sync and the
counter above the trait cannot tell the difference, and the numbers of steps 1 and 2 hold.

Two things the messages settle for the carriers to come. A sync base crosses as
`SyncBaseWire`, every field including the three its report form skips. An `import` is cut
into batches by the client, each one a message answered with its stats and its per-file
errors, so progress is reported from the replies. `wait` is a request with a timeout, which
a carrier will turn into a long poll or a stream.

The batch message exists and is tested; nothing above the trait sends one yet. The chunk
have-and-put exchange is not started.

### 2. The seam is the `Store` trait, defined as messages and streams, never as a wire

Two seams exist in the code. The **operation seam** is the `Store` trait, already implemented
twice (SQLite, Postgres) with everything above it engine-blind: sync, the commands, front
matter, assets, and the Node server through the CLI. The **chunk seam** is
`textdb_core::Storage` (get/put chunk and node, read and compare-and-swap root), which a local
replica would sync across with a have-and-want exchange. The split is at the operation seam; the
chunk exchange arrives later as two more methods on it, not as a second seam.

To be carrier-agnostic:

- **Operations as data.** Every method becomes a request and a response struct, plain
  `serde` types, wrapped in one `Request` and one `Response` enum. A macro over the trait
  generates the enums, the client (`RemoteStore`, one more `Store` implementation) and the
  server dispatcher (in front of `PgStore`), so the mirror cannot drift.
- **Encoding as a choice.** Types are codec-neutral: bytes as bytes, integers as integers, flat
  structs. JSON first (debuggable with curl); CBOR or MessagePack by flag; Arrow for the
  row-shaped responses, whose schema is `docs/shapes.md`.
- **Carrier as a trait.** `Transport` has three operations: call one request, call a batch,
  subscribe to changes from a sequence number. HTTPS, WebSocket and Flight implement it; so does
  an in-process loopback, which is how the seam is tested with no network.
- **Batches are first-class**, a list of requests answered in order with an atomic-or-independent
  flag, so sync sends its plan in one call and the per-file round trip disappears without
  touching sync's logic.
- **Streams are resumable by sequence number**, so `watch` on any carrier is "changes since N"
  and reconnection costs one number.
- **Compression is a message property**: large fields carry a content-encoding tag, so every
  carrier compresses alike, and one that compresses on its own switches it off.
- **Identity and errors are the store's**: the token is set once at connect (as `kb.auth`
  today), and errors stay `StoreError` with its `TX` codes and structured detail.

### 3. Order of work

1. **Done:** base cache, hunks down, ranges up, wire counter.
2. **Done:** the vault-sized terms (§1b): base rows kept in the checkout, heads as a delta,
   the base saved as a delta, a rebased push applied to disk at once.
3. **Done** as a module of the CLI crate (§1c): the message types, the JSON codec, `Transport`
   with batch, the loopback running the whole suite. Left: sync sending its plan as a batch,
   the chunk have-and-put exchange, and the split into a `textdb-proto` crate, which waits
   for the gateway binary that will share it.
4. First carrier: HTTPS. WebSocket and Flight as later `Transport` implementations.
5. Gateway deployment and the asset-store delegation endpoint on the same service.

## Consequences

- Step 1 lands the "after" column for SQLite, Postgres and every future remote store alike,
  because all three sit under the same trait. It is measurable today.
- A checkout gains a base cache of roughly one compressed copy of its text files. It is
  disposable: deleting it costs one whole-file sync.
- The spec's byte-exact round trip (A4) is preserved by construction: ranges are verified
  against the disk bytes before they are sent, and rebuilt text is verified hunk by hunk.
- A rebased write still leaves the base version unknown (as before), which forces the
  whole-file path once; step 2's hash on `Written` removes that.
- The carrier decision is deferred without cost: nothing in steps 1–3 knows which wire wins.
