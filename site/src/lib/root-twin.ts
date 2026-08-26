// SPDX-License-Identifier: Apache-2.0
import { readdir, rename, stat } from "node:fs/promises"
import { join } from "node:path"
import type { AstroIntegration } from "astro"

/**
 * Moves the landing page's raw Markdown twin off a dotfile path, because GitHub Pages will not serve one.
 *
 * `starlight-md-txt` maps the root collection entry — whose id is the empty string or `index` — to an
 * undefined slug, so the twin it injects for the landing page is `<base>/.md`. That file is emitted
 * correctly and is present in the uploaded artifact; the host then refuses it. Measured against the
 * deployed site: every non-dot twin answers 200 with `text/markdown`, and `/.md` answers 404 while
 * `dist/.md` is 4,613 bytes on disk.
 *
 * That failure is invisible to a gate over `dist/`, which is the limit of "assert over the built output":
 * the built output is not the same thing as what the host agrees to serve. So the fix is to stop
 * depending on the host's tolerance — the twin is renamed to `index.md`, which every static host serves,
 * and `rawMarkdownUrl` points the landing page's `rel="alternate"` and page actions at that path.
 *
 * A rename rather than a copy, so there is exactly one root twin. Leaving `.md` behind would need an
 * exception in the orphan-twin probe for a file nothing can fetch.
 */

/** The dotfile path `starlight-md-txt` injects for the root entry, relative to the output directory. */
const INJECTED = ".md"

/** Where it is moved to. Must match `rawMarkdownUrl`'s mapping for the root entry. */
const SERVED = "index.md"

export const rootTwin = (): AstroIntegration => ({
  name: "docs:root-twin",
  hooks: {
    "astro:build:done": async ({ dir, logger }) => {
      const from = join(dir.pathname, INJECTED)
      const to = join(dir.pathname, SERVED)

      const source = await stat(from).catch(() => undefined)
      if (source === undefined) {
        /*
         * Loud rather than silent. If the plugin stops injecting this route, or renames it, the landing
         * page's twin is missing and its `rel="alternate"` points at a 404 — the exact defect this file
         * exists to close, reintroduced by an upstream change nobody would connect to it.
         */
        const emitted = (await readdir(dir.pathname)).filter((name) => name.endsWith(".md"))
        throw new Error(
          `docs:root-twin: expected the landing page's twin at \`${INJECTED}\` and found no such ` +
            `file. \`starlight-md-txt\` maps the root entry to an undefined slug, so this is where it ` +
            `lands. Markdown at the output root: ${emitted.join(", ") || "none"}. If the plugin's ` +
            "mapping changed, update INJECTED here and `rawMarkdownUrl` together."
        )
      }

      if ((await stat(to).catch(() => undefined)) !== undefined) {
        throw new Error(
          `docs:root-twin: refusing to overwrite \`${SERVED}\`, which already exists. A page whose ` +
            "own twin is that path would be silently replaced by the landing page's."
        )
      }

      await rename(from, to)
      logger.info(`moved the landing page's twin from ${INJECTED} to ${SERVED} (a host serves no dotfile)`)
    }
  }
})
