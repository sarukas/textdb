import { Compartment, EditorState, Transaction } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { useEffect, useRef } from "react";
import type { DocController, DocState } from "../doc/controller";
import { chunkField, chunkGutter, setChunks } from "../editor/chunks";
import { addFlash, expireFlash, remoteAnnotation, remoteHighlights } from "../editor/remote";
import { baseExtensions, editableExtensions } from "../editor/setup";

interface Props {
  controller: DocController;
  state: DocState;
  visible: boolean;
  chunksOn: boolean;
  focus: { line: number; nonce: number } | null;
}

const FLASH_MS = 4500;

export function Editor({ controller, state, visible, chunksOn, focus }: Props) {
  const host = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const chunkSlot = useRef(new Compartment());

  useEffect(() => {
    const tracker = controller.tracker;
    if (!tracker || !host.current) return;
    const timers = new Set<ReturnType<typeof setTimeout>>();
    const view = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: tracker.current,
        extensions: [
          ...baseExtensions(),
          ...editableExtensions(),
          remoteHighlights(),
          chunkField,
          chunkSlot.current.of([]),
          EditorView.contentAttributes.of({ "aria-label": `Markdown source of ${controller.state.path}` }),
          EditorView.updateListener.of((u) => {
            for (const tr of u.transactions) {
              if (tr.docChanged && !tr.annotation(remoteAnnotation)) controller.userEdit(tr.changes);
            }
          }),
        ],
      }),
    });
    viewRef.current = view;
    // Dev-only handle for poking at the live document from the browser console.
    if (import.meta.env.DEV) (host.current as HTMLDivElement & { __view?: EditorView }).__view = view;

    const offRemote = controller.remote.on(({ applied, flash }) => {
      if (view.state.doc.length !== applied.changes.length) {
        // Should never happen; recover by showing the tracker's document as-is.
        view.dispatch({
          changes: { from: 0, to: view.state.doc.length, insert: applied.doc },
          annotations: [remoteAnnotation.of(true), Transaction.addToHistory.of(false)],
        });
        return;
      }
      view.dispatch({
        changes: applied.changes,
        annotations: [remoteAnnotation.of(true), Transaction.addToHistory.of(false)],
        effects: flash ? [addFlash.of({ id: flash.id, spans: applied.spans, hue: flash.hue, label: flash.label })] : [],
      });
      if (flash) {
        const t = setTimeout(() => {
          timers.delete(t);
          view.dispatch({ effects: expireFlash.of(flash.id) });
        }, FLASH_MS);
        timers.add(t);
      }
    });
    const offReset = controller.reset.on((doc) => {
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: doc },
        annotations: [remoteAnnotation.of(true), Transaction.addToHistory.of(false)],
      });
    });
    return () => {
      offRemote();
      offReset();
      timers.forEach(clearTimeout);
      view.destroy();
      viewRef.current = null;
    };
  }, [controller]);

  // Chunk bands.
  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const marks = chunksOn ? controller.chunkMarks() : null;
    const effects = [chunkSlot.current.reconfigure(chunksOn ? chunkGutter : [])];
    if (!chunksOn) effects.push(setChunks.of(null) as never);
    else if (marks) effects.push(setChunks.of(marks) as never);
    view.dispatch({ effects });
  }, [controller, chunksOn, state.chunks, state.chunkVersion, state.freshChunks, state.version]);

  useEffect(() => {
    if (visible) viewRef.current?.requestMeasure();
  }, [visible]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view || !focus) return;
    const n = Math.min(Math.max(1, focus.line), view.state.doc.lines);
    const pos = view.state.doc.line(n).from;
    view.dispatch({ selection: { anchor: pos }, effects: EditorView.scrollIntoView(pos, { y: "center" }) });
    view.focus();
  }, [focus]);

  return <div ref={host} className="editor-host" hidden={!visible} />;
}
