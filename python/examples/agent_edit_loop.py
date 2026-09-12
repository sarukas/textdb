"""How an agent edits safely next to other agents: anchored edit, base-version write,
conflict handling with the payload, and append for journals.

    python examples/agent_edit_loop.py sqlite:///kb.db
"""
import sys

from textdb import Conflict, Corpus, InvalidEdit

with Corpus.open(sys.argv[1], author="agent-7") as kb:
    path = "/clients/acme/notes.md"
    kb.write(path, "# Acme\n\n- renewal: pending\n- owner: dana\n")

    # 1. Anchored single change — no need to hold the whole document.
    v = kb.edit(path, "- renewal: pending", "- renewal: signed 2026-09-12")
    print("edit committed version", v)

    # 2. A batch of changes derived from a read, written against that version.
    text, version = kb.read_versioned(path)
    new_text = text.replace("dana", "dana (CFO)") + "\n## Next steps\n- pricing call\n"
    v = kb.update(path, new_text, base_version=version)
    print("update committed version", v)

    # 3. Simulate a stale write: another agent changed the owner line in between.
    text, version = kb.read_versioned(path)
    kb.edit(path, "dana (CFO)", "dana (CFO, since 2025)", author="agent-3")      # someone else
    try:
        kb.update(path, text.replace("dana (CFO)", "dana (CFO) — call Tue"), base_version=version)
    except Conflict as c:
        print("conflict on lines", c.region, "current text:", repr(c.theirs))
        # Rebuild the change from `theirs` and retry against the current version.
        v = kb.update(path, kb.read(path).replace("dana (CFO, since 2025)", "dana (CFO, since 2025) — call Tue"),
                      base_version=c.current_version)
        print("resolved at version", v)

    # 4. Disjoint concurrent edit from a stale base rebases cleanly (no conflict).
    text, version = kb.read_versioned(path)
    kb.append(path, "- send SOW\n", author="agent-3")
    v = kb.update(path, text.replace("# Acme", "# Acme (renewed)"), base_version=version)
    print("rebased over the append, version", v)

    # 5. Journals: append never conflicts.
    kb.append("/clients/acme/journal.md", "- 2026-09-12 agent-7: renewal signed\n") if kb.exists("/clients/acme/journal.md") \
        else kb.write("/clients/acme/journal.md", "- 2026-09-12 agent-7: renewal signed\n")

    # 6. A helper that does read → transform → write-with-base → retry on conflict.
    kb.edit_with_retry(path, lambda t: t.replace("pricing call", "pricing call (booked)"))

    try:
        kb.edit(path, "not in the document", "x")
    except InvalidEdit as e:
        print("invalid edit:", e)

    print(kb.diff(path, 1, kb.version(path)))
    print(kb.read(path))
