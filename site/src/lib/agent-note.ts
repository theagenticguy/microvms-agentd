// SPDX-License-Identifier: Apache-2.0
import { defineMdastPlugin, type MdastPluginDefinition } from "satteri"

/**
 * The `:::agent` container directive: an inline note addressed to an agent rather than to a human.
 *
 * **Why a directive and not a component.** `starlight-md-txt` builds each raw `.md` route from the
 * entry's *source* body and deletes every custom component from it — a self-closing one becomes empty
 * — while `:::agent` passes through byte-for-byte. A component-based note would therefore render in
 * HTML and vanish from the surface it exists to address, leaving the agent-facing text contradicting
 * the human-facing one on exactly the point being made, to the reader least able to notice.
 *
 * **Why the label is authored in the body and not injected here.** `starlight-llms-txt` renders the
 * page and flattens it back to Markdown, so only visible text survives into the bundle — a class name
 * does not, and colour could not be the signal anyway (SC 1.4.1). A label injected by this plugin
 * would reach the HTML and the llms bundle but NOT the raw `.md` route, which is built from source.
 * One authored line reaches all three surfaces as the same bytes, and `AGENT_NOTE_LABEL` is what a
 * test asserts so the convention cannot quietly rot.
 *
 * **Why no directive attributes.** `:::agent{class="x"}` would carry a brace into the raw Markdown,
 * and that route parses every body through `remark-mdx`, where a brace opens a JSX expression. One
 * occurrence fails the whole route.
 *
 * **Why a plain `div` and not an `aside`.** An `<aside>` needs an accessible name to be a landmark,
 * and a page carrying several identically-named complementary landmarks is an axe `landmark-unique`
 * violation. The authored label already carries the semantics as text.
 *
 * **Why the label is not written with a directive label.** `:::agent[For an agent]` reaches the HTML,
 * but remark-stringify escapes the bracket on the way to the raw route, which then reads
 * `:::agent\[For an agent]` — a backslash injected into the agent-facing surface.
 */

/**
 * The literal label every `:::agent` block opens with, on every surface.
 *
 * The constant exists because every authored block repeats this string by hand, so a gate asserts it
 * rather than trusting the pages to agree.
 */
export const AGENT_NOTE_LABEL = "For an agent"

/** The class the note is styled through. The `docs-` prefix is matched by `styles/docs.css`. */
export const AGENT_NOTE_CLASS = "docs-agent-note"

/** The directive name, which is authored syntax on every page that carries a note. */
export const AGENT_NOTE_DIRECTIVE = "agent"

/*
 * The paragraph node type is derived from the visitor's own return type rather than imported from
 * `mdast`, which is transitive here and so does not resolve under pnpm's isolated layout.
 */
type DirectiveVisitor = NonNullable<MdastPluginDefinition["containerDirective"]>
type ParagraphNode = Extract<Awaited<ReturnType<DirectiveVisitor>>, { type: "paragraph" }>

/**
 * Claims the `agent` container directive.
 *
 * Registration order matters and is in this site's favour: Starlight *pushes* its own mdast plugins
 * after the ones configured here, and its `starlight-directives-restoration` pass — which would
 * otherwise turn an unclaimed directive into an unstyled bare `<div>` — skips any node another plugin
 * has already claimed. Returning a replacement node is that claim.
 *
 * The visitor receives a readonly node, so the directive is replaced rather than mutated.
 */
export const agentNotePlugin = (): MdastPluginDefinition =>
  defineMdastPlugin({
    name: "docs-agent-note",
    containerDirective(node) {
      if (node.name !== AGENT_NOTE_DIRECTIVE) return
      // A paragraph carrying `hName` is how Sätteri names an arbitrary element. A directive's children
      // are block content while a paragraph's are phrasing content, so this makes the same cast that
      // Starlight's own aside plugin makes for the same reason.
      return {
        type: "paragraph",
        data: { hName: "div", hProperties: { class: AGENT_NOTE_CLASS } },
        children: [...node.children] as ParagraphNode["children"]
      }
    }
  })
