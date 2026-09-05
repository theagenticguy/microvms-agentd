// SPDX-License-Identifier: Apache-2.0
import { defineHastPlugin } from "satteri"

/**
 * Makes every horizontally scrolling block keyboard-reachable.
 *
 * A `<pre>` from Expressive Code and a `<table>` from Starlight's Markdown renderer both scroll
 * sideways when their content is wider than the measure, and neither is focusable, so the overflowing
 * half is reachable with a pointer and unreachable with a keyboard. That is SC 2.1.1, and on this site
 * it is measured rather than hypothetical: the generated command pages carry a parameters table whose
 * help column exceeds the measure at 1280px, and `tests/a11y.test.ts` reported the table as scrolling
 * 60px with no focus before this plugin existed.
 *
 * `tabindex="0"` is the fix WCAG's own technique names. A `role`/`aria-label` pair is deliberately NOT
 * added: a focusable element with no accessible name is a smaller problem than a region announced with
 * a name invented by a build step, and the surrounding heading already says what the block is.
 *
 * Sätteri's plugins are visitor objects keyed by node type: an `element` subscription carrying a
 * `filter` of tag names, resolved once per document. The node is `Readonly`, so the visitor RETURNS a
 * replacement rather than mutating in place. Starlight pushes its own hast plugins after the ones
 * configured here, and Expressive Code replaces the whole `pre` subtree, so the attribute survives on
 * tables by construction and on code blocks only where Expressive Code carries it through (it does:
 * its frame is a `div` around the `pre` it renders, and the `pre` inside is its own).
 *
 * Ported from memhtml-public (apps/docs/src/lib/focusable-scrollers.ts).
 */
export const focusableScrollers = () =>
  defineHastPlugin({
    name: "docs-focusable-scrollers",
    element: {
      filter: ["pre", "table"],
      visit(node) {
        if (node.properties?.tabIndex !== undefined) return
        return { ...node, properties: { ...node.properties, tabIndex: 0 } }
      }
    }
  })
