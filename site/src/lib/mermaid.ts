// SPDX-License-Identifier: Apache-2.0
/**
 * Build-time Mermaid rendering: an Astro integration that claims the ```mermaid fence at mdast and
 * replaces it with rendered SVG, so no diagram runtime reaches the browser and the diagram is present
 * in a fetch that runs no JavaScript.
 *
 * **Why build time and not the client.** A client-rendered diagram is absent from every agent surface
 * at once: absent from the page's raw Markdown twin, absent from the llms bundles, and absent from a
 * plain `fetch` of the HTML. The diagram is the densest thing on the page and it is the one thing the
 * machine reader cannot see. Rendering at build time is what makes the surface honest.
 *
 * **Why an mdast visitor and not a hast one.** Expressive Code is itself a hast plugin, pushed as a
 * `hastPlugins` entry by its own integration, so it runs after any hast visitor written here and
 * replaces the whole `pre` subtree the visitor just edited. At mdast the fence is still a fence and
 * nothing downstream has claimed it.
 *
 * **Why the renderer is injected.** The renderer is the part with a dependency, a fidelity ceiling and
 * a licence, and it is the part most likely to be swapped. Passing it in keeps this file assertable
 * with a stub renderer and makes the swap one argument rather than a fork.
 */

import type { AstroIntegration } from "astro"
import { renderMermaidSVG } from "beautiful-mermaid"
import { defineMdastPlugin, type MdastPluginDefinition } from "satteri"

/** The fence language claimed. */
export const MERMAID_LANG = "mermaid"

/** What a renderer is told about one fence. */
export interface Diagram {
  /** The fence body, verbatim. */
  readonly source: string
  /** The fence's info string past the language, or `undefined`. */
  readonly meta: string | undefined
  /** Zero-based position among the mermaid fences in this document. */
  readonly index: number
  /**
   * A label for the document, for error messages. Never `undefined`: see the `fileURL` note on
   * `mermaidPlugin`. It is a filesystem path when the compile supplies one and a synthetic label
   * otherwise, so it is fit for a message and not for reading a file.
   */
  readonly label: string
}

/**
 * Turns one fence into an SVG string. SYNCHRONOUS, and that is a hard requirement rather than a
 * simplification.
 *
 * Sätteri switches its whole document walk to async as soon as any visitor returns a promise, and on
 * that path `starlight-links-validator` records no headings for the page: it seeds its per-file entry
 * from a hast visitor, and a page with no entry makes every link INTO it report `InvalidLink` while the
 * page itself builds and serves correctly. Measured on this corpus — an async renderer put all six
 * diagram-carrying pages in that state and nothing but the link validator noticed.
 *
 * `beautiful-mermaid` is synchronous, so nothing is lost. A renderer that needs a browser — the
 * `mermaid-isomorphic` route, for a diagram type `beautiful-mermaid` does not implement — cannot be
 * plugged in here without paying that cost, which is the honest trade to see at the type level.
 */
export type MermaidRenderer = (diagram: Diagram) => string

export interface MermaidOptions {
  /** Defaults to `beautifulMermaid()`. */
  readonly renderer?: MermaidRenderer
  /** Wrapper class on the emitted `<figure>`, for the CSS that constrains diagram width. */
  readonly className?: string
}

/**
 * Options for `beautiful-mermaid`, the default renderer.
 *
 * Chosen for three properties, each verified against the installed package at version 1.1.3 on
 * 2026-08-26:
 *
 *  1. **Synchronous and browserless.** `renderMermaidSVG(text, options) => string`. It computes its own
 *     text metrics and lays out with `elkjs`, so a build needs no Chromium download, no `playwright`
 *     install step, and no per-diagram browser round trip. A docs build stays a `node` process, which
 *     is what keeps the GitHub Pages workflow to one dependency install.
 *  2. **CSS-custom-property output.** The root element carries `style="--bg:…;--fg:…"` and every other
 *     colour is derived from those two with `color-mix()`. Passing the page theme's own variables makes
 *     ONE rendered asset track light and dark without a second render and without a media query inside
 *     the SVG.
 *  3. **It throws on input it does not understand** rather than producing an empty drawing, which is
 *     what lets the guard below be a guard.
 *
 * Rendering only reaches CSS variables when the SVG is INLINED in the document. A `var()` inside an SVG
 * referenced as `<img src="diagram.svg">` resolves in the image's own document, where the page's custom
 * properties do not exist, and the diagram renders with no colour at all. That is why this integration
 * returns inline markup and never writes a file plus an `<img>`.
 *
 * Supported diagram types, probed by rendering each one: `graph`/`flowchart` (all four directions),
 * `stateDiagram-v2`, `sequenceDiagram`, `classDiagram`, `erDiagram`, `xychart-beta`. NOT supported, and
 * each throws: `gantt`, `pie`, `mindmap`, `journey`, and every other header. A page needing one of those
 * supplies `renderer: …` built on `mermaid-isomorphic`, which runs the real `mermaid` package in a
 * headless browser and takes a `playwright` peer dependency plus a browser download in CI.
 */
export interface BeautifulMermaidOptions {
  /** Background, written into `--bg`. A `var(...)` reference is passed through verbatim. */
  readonly bg?: string
  /** Foreground, written into `--fg`. A `var(...)` reference is passed through verbatim. */
  readonly fg?: string
  /** Canvas padding in px. The package's own default is 40. */
  readonly padding?: number
  /**
   * Whether to keep the webfont `@import` the renderer writes into the SVG's `<style>`.
   *
   * It is stripped by default, and the reason is a property of the package rather than a preference:
   * the SVG carries `@import url('https://fonts.googleapis.com/css2?family=…')` UNCONDITIONALLY, built
   * by interpolating the `font` option into the URL. There is no option that suppresses it — passing
   * `font: "inherit"` requests a Google font literally named `inherit`. So a build-time render ships a
   * third-party font request on every page carrying a diagram, which is a privacy fact and a
   * render-block that is invisible in the integration's configuration. The declaration it feeds is
   * `font-family: '<font>', system-ui, sans-serif`, so with the `@import` removed the text falls back
   * to the system stack and the layout, computed at build time, does not move.
   */
  readonly webfontImport?: boolean
}

const GOOGLE_FONT_IMPORT =
  /@import\s+url\((['"])https:\/\/fonts\.googleapis\.com[^'"]*\1\);?[ \t]*\n?/g

/**
 * `renderMermaidSVG` is the non-deprecated synchronous entry point.
 *
 * `renderMermaidSync` is a deprecated alias of it, and `renderMermaid` is a deprecated alias of the
 * ASYNC one — so importing the shortest-looking name and forgetting to await it stringifies a `Promise`
 * into the page.
 */
export const beautifulMermaid = (options: BeautifulMermaidOptions = {}): MermaidRenderer => {
  const settings = {
    ...(options.bg === undefined ? {} : { bg: options.bg }),
    ...(options.fg === undefined ? {} : { fg: options.fg }),
    ...(options.padding === undefined ? {} : { padding: options.padding }),
    /*
     * The renderer's own background rectangle is suppressed so the diagram sits on the page's
     * background. `--bg` is still written to the root element and is still what every derived colour
     * mixes against, so contrast is preserved.
     */
    transparent: true
  }

  return (diagram) => {
    const svg = renderMermaidSVG(diagram.source, settings)
    return options.webfontImport === true ? svg : svg.replace(GOOGLE_FONT_IMPORT, "")
  }
}

/**
 * The Sätteri mdast plugin. One instance per document, which is what makes `index` per-document.
 *
 * **The `fileURL` trap.** `ctx.fileURL` is `URL | undefined`: it holds the compile's `fileURL` option,
 * and a content-layer loader that synthesizes an entry body does not have to supply one. The obvious
 * guard — `if (!ctx.fileURL) return` — therefore makes the plugin silently skip every loader-injected
 * page, and skipping is invisible: the page builds, the fence renders as a code block, and the only
 * symptom is a diagram-shaped listing on some pages and not others. So `fileURL` is treated as what it
 * is, a label that may be absent, and it gates nothing:
 *
 *   * The MDX branch reads `ctx.sourceFormat`, which is always `"markdown"` or `"mdx"` and never
 *     undefined, rather than testing the filename extension.
 *   * The error label falls back to a synthetic string, so a message still identifies the document.
 *
 * **Why the MDX branch exists.** An mdast `html` node is raw HTML, which MDX has no node for: in an
 * `.mdx` document the SVG has to arrive as JSX. `set:html` on a `Fragment` is Astro's own accessor for
 * that, and it carries the markup as a string attribute, so a brace inside an SVG `<style>` element is
 * never read as an expression.
 */
export const mermaidPlugin = (options: MermaidOptions = {}): MdastPluginDefinition => {
  const render = options.renderer ?? beautifulMermaid()
  const className = options.className ?? "docs-mermaid"
  let index = 0

  return defineMdastPlugin({
    name: "mermaid",
    /*
     * NOT async. Returning a promise from any visitor switches Sätteri's whole walk to the async path,
     * on which `starlight-links-validator` records no per-file entry for the document — so every link
     * INTO a diagram-carrying page reports `InvalidLink` while the page itself builds and serves
     * correctly. The renderer's type forbids a promise for that reason.
     */
    code(node, ctx) {
      if (node.lang !== MERMAID_LANG) return

      const label = ctx.fileURL === undefined ? "<generated entry>" : ctx.fileURL.pathname
      const at = node.position?.start.line
      const where = at === undefined ? label : `${label}:${at}`

      let svg: string
      try {
        svg = render({
          source: node.value,
          meta: node.meta ?? undefined,
          index: index++,
          label: where
        })
      } catch (cause) {
        /*
         * THROW. NEVER PASS THE FENCE THROUGH.
         *
         * Returning `undefined` here leaves the node alone, and the fence then ships as a code block
         * full of Mermaid source: a build that succeeded, a page that looks plausible, and a diagram
         * the author believes is rendered. That is the single failure this file exists to prevent, and
         * it is worse than a red build because it is not reported anywhere. `beautiful-mermaid` throws
         * `Invalid mermaid header: "gantt". Expected "graph TD", "flowchart LR", "stateDiagram-v2",
         * etc.` for a diagram type it does not implement, and `Empty mermaid diagram` for an empty
         * fence, so an unsupported type is a named build failure with a page and a line number.
         */
        throw new Error(
          `${where}: mermaid diagram ${index - 1} did not render. ` +
            "A fence that cannot be rendered is never passed through as a code block: fix the diagram, " +
            "or supply a renderer that supports its type.",
          { cause }
        )
      }

      if (svg.trim() === "") {
        throw new Error(`${where}: the renderer returned an empty string for diagram ${index - 1}`)
      }

      /*
       * A `<figure>` rather than a bare `<svg>`: the diagram is a figure in the document's own terms,
       * and it gives the width-constraining CSS one stable hook.
       *
       * `tabindex="0"` because that CSS makes the figure a horizontal scroll container on a narrow
       * viewport, and a scrollable region with no focusable descendant cannot be scrolled without a
       * pointer (SC 2.1.1). It is written here, at mdast, rather than applied by a hast visitor: a
       * late hast pass that returns a replacement for the subtree discards any earlier patch on it.
       */
      const value = `<figure class="${className}" tabindex="0">${svg}</figure>`

      return ctx.sourceFormat === "mdx"
        ? {
            type: "mdxJsxFlowElement",
            name: "Fragment",
            attributes: [{ type: "mdxJsxAttribute", name: "set:html", value }],
            children: []
          }
        : { type: "html", value }
    }
  })
}

/**
 * The processor a project configured, narrowed structurally.
 *
 * The published option type declares `mdastPlugins` as an array this code has no write access to, and
 * appending is the documented way an integration contributes a plugin. A local structural type is how
 * that append is expressed without asserting through `any`, and it doubles as the identity test below.
 */
interface SatteriProcessor {
  readonly name: string
  readonly options: { mdastPlugins: unknown[] }
}

interface UnifiedProcessor {
  readonly name: string
  readonly options: { remarkPlugins: unknown[] }
}

const isSatteriProcessor = (processor: unknown): processor is SatteriProcessor => {
  if (typeof processor !== "object" || processor === null) return false
  const candidate = processor as { name?: unknown; options?: { mdastPlugins?: unknown } }
  return candidate.name === "satteri" && Array.isArray(candidate.options?.mdastPlugins)
}

const isUnifiedProcessor = (processor: unknown): processor is UnifiedProcessor => {
  if (typeof processor !== "object" || processor === null) return false
  const candidate = processor as { name?: unknown; options?: { remarkPlugins?: unknown } }
  return candidate.name === "unified" && Array.isArray(candidate.options?.remarkPlugins)
}

/**
 * Attaches the plugin to whichever processor the project configured, and refuses the rest.
 *
 * Dispatch is on the processor's own identity rather than on the Astro version, because a project may
 * configure either engine at any version. The push is a factory rather than a plugin instance, so
 * Sätteri calls it once per document and each document gets its own diagram counter.
 *
 * The unified branch throws with its own message instead of falling back to a remark plugin. That is a
 * deliberate absence: a remark implementation is a second code path with a second set of node types and
 * a second failure mode, and shipping one that is never exercised is how the untested path becomes the
 * one that breaks. The message names the fix, which is one line of configuration.
 */
export const attachMermaidPlugin = (processor: unknown, options: MermaidOptions): void => {
  if (isSatteriProcessor(processor)) {
    processor.options.mdastPlugins.push(() => mermaidPlugin(options))
    return
  }
  if (isUnifiedProcessor(processor)) {
    throw new Error(
      "The mermaid integration renders at mdast and needs the Sätteri processor, but " +
        "`markdown.processor` is the unified engine. Set " +
        "`markdown: { processor: satteri() }` in astro.config.ts, which is Astro 7's own default."
    )
  }
  throw new Error(
    "`markdown.processor` is not a processor the mermaid integration recognises. It supports the " +
      'Sätteri engine, identified by `processor.name === "satteri"`.'
  )
}

/**
 * Adds `mermaid` to the syntax highlighter's exclude list, preserving whatever is configured.
 *
 * Without this the highlighter reaches the fence first. The default exclude list is `["math"]` and
 * nothing else, so a mermaid fence is highlighted as an unknown language before this plugin sees it —
 * and under Expressive Code that warns once per unrecognised language and floods `astro check`.
 *
 * Two behaviours of the surrounding machinery make the merge below safe:
 *
 *  * The processor tests `excludeLangs.includes(lang) || defaultExcludeLanguages.includes(lang)`, so
 *    `math` stays excluded whatever this function writes. Appending cannot drop it.
 *  * `updateConfig` merges arrays by CONCATENATION and does not re-validate the config, so passing only
 *    the delta appends to a project's own list rather than replacing it. A repeated entry is harmless
 *    because the test is `includes`.
 *
 * The branch on the value's shape is the part that is easy to get wrong. `markdown.syntaxHighlight` is
 * a union of an object, the strings `"shiki"` and `"prism"`, and `false`. When a project wrote
 * `syntaxHighlight: "prism"`, merging an object over a string REPLACES it — so passing
 * `{ excludeLangs: […] }` alone would silently drop `type` and turn highlighting off site-wide. The
 * string case therefore re-states `type` explicitly.
 */
export const excludeMermaidFromHighlighting = (
  current: unknown,
  updateConfig: (config: {
    markdown: { syntaxHighlight: { type?: string; excludeLangs: string[] } }
  }) => unknown
): void => {
  // `false` disables highlighting outright: nothing claims the fence, so there is nothing to exclude.
  if (current === false) return

  if (typeof current === "string") {
    updateConfig({
      markdown: { syntaxHighlight: { type: current, excludeLangs: [MERMAID_LANG] } }
    })
    return
  }

  const existing =
    typeof current === "object" &&
    current !== null &&
    Array.isArray((current as { excludeLangs?: unknown }).excludeLangs)
      ? (current as { excludeLangs: string[] }).excludeLangs
      : []
  if (existing.includes(MERMAID_LANG)) return
  updateConfig({ markdown: { syntaxHighlight: { excludeLangs: [MERMAID_LANG] } } })
}

/**
 * The integration.
 *
 * Everything happens at `astro:config:setup`, which is the last hook that runs before the Markdown
 * processor is built and therefore the only one where a plugin can still be added to it.
 */
export default function mermaid(options: MermaidOptions = {}): AstroIntegration {
  return {
    name: "docs:mermaid",
    hooks: {
      "astro:config:setup": ({ command, config, updateConfig }) => {
        /*
         * `sync` and `preview` build no Markdown. Attaching in `sync` would run every renderer during
         * type generation, for no output.
         */
        if (command !== "build" && command !== "dev") return

        excludeMermaidFromHighlighting(
          config.markdown.syntaxHighlight,
          updateConfig as Parameters<typeof excludeMermaidFromHighlighting>[1]
        )
        attachMermaidPlugin(config.markdown.processor, options)
      }
    }
  }
}
