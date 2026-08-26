// SPDX-License-Identifier: Apache-2.0
import { readdir, readFile, writeFile } from "node:fs/promises"
import { join } from "node:path"
import type { AstroIntegration } from "astro"

/**
 * Prefixes the base segment onto root-relative links inside the emitted raw `.md` routes.
 *
 * The defect this closes: this site is published under `/microvms-agentd/`, and a root-relative link
 * authored as `/agents/` comes out of the twin builder unchanged, so an agent following it fetches
 * `https://theagenticguy.github.io/agents/` and gets a 404 — while the same link in the HTML is
 * correct. The two surfaces are built from different things. `starlight-base-path` rewrites the
 * rendered tree, while `starlight-md-txt` builds each raw route from the page's Markdown source, so a
 * link is correct on whichever surface its producer touched and wrong on the other, and nothing fails.
 *
 * Rewriting the store's entry bodies does not reach it: the raw route re-reads a file-backed page from
 * disk. The emitted output is the one place every kind of page has converged, which is why this runs
 * on `astro:build:done` rather than earlier.
 *
 * It is deliberately not a general link rewriter. Anchors, external URLs, protocol-relative hrefs and
 * already-prefixed paths are left exactly as written — the last of those is what makes a second run a
 * no-op, so this composes with an incremental build instead of compounding. It is also safe on a
 * root-base build, where it returns without reading a file.
 */

const markdownFiles = async (dir: string): Promise<ReadonlyArray<string>> => {
  const found: Array<string> = []
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name)
    if (entry.isDirectory()) found.push(...(await markdownFiles(path)))
    else if (entry.name.endsWith(".md")) found.push(path)
  }
  return found
}

/** `](/path)` and `]: /path` — the two forms Markdown writes a root-relative target in. */
const rewrite = (body: string, segment: string): string =>
  body
    .replace(/\]\((\/(?!\/)[^)]*)\)/g, (match, target: string) =>
      target.startsWith(segment) ? match : `](${segment}${target.slice(1)})`
    )
    .replace(/^(\[[^\]]+\]:[ \t]+)(\/(?!\/)\S*)/gm, (match, label: string, target: string) =>
      target.startsWith(segment) ? match : `${label}${segment}${target.slice(1)}`
    )

/**
 * @param base the site's base segment. Passed in rather than read from the hook: `astro:build:done`
 *   carries `dir`, `routes`, `pages`, `assets` and `logger`, and no base — reading one from there
 *   yields `undefined`, and the first thing that touches it throws.
 */
export const baseRawLinks = (base: string): AstroIntegration => ({
  name: "docs:base-raw-links",
  hooks: {
    "astro:build:done": async ({ dir, logger }) => {
      const segment = base.endsWith("/") ? base : `${base}/`
      // A root base already makes every link correct, so there is nothing to rewrite.
      if (segment === "/") return
      let rewritten = 0
      for (const file of await markdownFiles(dir.pathname)) {
        const body = await readFile(file, "utf8")
        const next = rewrite(body, segment)
        if (next === body) continue
        await writeFile(file, next)
        rewritten += 1
      }
      logger.info(`prefixed ${segment} into root-relative links across ${rewritten} raw routes`)
    }
  }
})
