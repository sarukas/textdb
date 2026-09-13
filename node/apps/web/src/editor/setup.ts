import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { markdown } from "@codemirror/lang-markdown";
import { defaultHighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { highlightSelectionMatches, searchKeymap } from "@codemirror/search";
import { EditorState, type Extension } from "@codemirror/state";
import {
  drawSelection,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  keymap,
  lineNumbers,
} from "@codemirror/view";

/** Theme bound to the app's CSS variables, so it follows light/dark with the page. */
export const appTheme = EditorView.theme({
  "&": { height: "100%", fontSize: "13px", backgroundColor: "var(--bg)", color: "var(--text)" },
  ".cm-scroller": { fontFamily: "var(--mono)", lineHeight: "1.55" },
  ".cm-content": { caretColor: "var(--accent)" },
  ".cm-gutters": { backgroundColor: "var(--bg)", color: "var(--faint)", borderRight: "1px solid var(--border)" },
  ".cm-activeLine": { backgroundColor: "var(--hover)" },
  ".cm-activeLineGutter": { backgroundColor: "var(--hover)", color: "var(--muted)" },
  "&.cm-focused": { outline: "none" },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": { backgroundColor: "var(--selection) !important" },
  ".cm-cursor": { borderLeftColor: "var(--accent)" },
  ".cm-panels": { backgroundColor: "var(--panel)", color: "var(--text)" },
});

/** Extensions shared by every CodeMirror instance: `\n` only, so offsets match the store's text. */
export function baseExtensions(): Extension[] {
  return [
    EditorState.lineSeparator.of("\n"),
    lineNumbers(),
    highlightActiveLineGutter(),
    drawSelection(),
    highlightActiveLine(),
    highlightSelectionMatches(),
    markdown(),
    syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
    EditorView.lineWrapping,
    appTheme,
  ];
}

export function editableExtensions(): Extension[] {
  return [history(), keymap.of([...defaultKeymap, ...historyKeymap, ...searchKeymap, indentWithTab])];
}

export function readOnlyExtensions(): Extension[] {
  return [EditorState.readOnly.of(true), EditorView.editable.of(false), keymap.of([...searchKeymap])];
}
