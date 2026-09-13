import MarkdownIt from "markdown-it";

/** The one markdown renderer: raw HTML disabled, since documents come from anyone who can write. */
export const md = new MarkdownIt({ html: false, linkify: true, typographer: false });

// Open links in a new tab and keep them from reaching back to this page.
const defaultLinkOpen =
  md.renderer.rules.link_open ?? ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
  const t = tokens[idx]!;
  t.attrSet("target", "_blank");
  t.attrSet("rel", "noopener noreferrer");
  return defaultLinkOpen(tokens, idx, options, env, self);
};
