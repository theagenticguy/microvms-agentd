// SPDX-License-Identifier: Apache-2.0
import { readdirSync, readFileSync, statSync } from "node:fs"
import { dirname, join, posix } from "node:path"
import { fileURLToPath } from "node:url"
import { describe, expect, it } from "vitest"

/**
 * The raw `.md` routes are the agent surface, and their links have to resolve THERE rather than only in
 * the HTML twin.
 *
 * This is the defect class the file exists for, and it is invisible from every angle a build reports on.
 * A site served from a base segment has TWO link rewriters: one for the rendered tree, one for the raw
 * twins, built from different inputs — the rendered tree from the compiled page, each twin from the
 * page's Markdown source. So a link comes out correct on whichever surface its producer touched and
 * wrong on the other, and nothing fails. `starlight-base-path` fixes the rendered half; every link in a
 * twin can point outside the base while the HTML is correct throughout and the link validator reports
 * every internal link valid.
 *
 * So: a passing link validator is not evidence about this surface. These assertions read the built bytes.
 */

/* =================================================================================================
 * CONFIG — the whole site-specific surface of this file.
 * ================================================================================================= */

const CONFIG = {
  /** Built output, relative to the package root. */
  dist: "dist",
  /** The base the build ran with, read from the same variable `astro.config.ts` reads. */
  base: process.env.DOCS_BASE ?? "/microvms-agentd/",
  /**
   * Path prefixes inside the built output that are not raw twins. `.md` files under these are assets
   * rather than pages, and holding them to the link contract reports a defect that is not one.
   */
  ignore: ["_astro/", "pagefind/"]
} as const

const distDir = join(dirname(dirname(fileURLToPath(import.meta.url))), CONFIG.dist)
const segment = CONFIG.base.endsWith("/") ? CONFIG.base : `${CONFIG.base}/`

/** Every raw Markdown route the build emitted, as absolute paths. */
const rawRoutes = (directory: string): ReadonlyArray<string> =>
  readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) return rawRoutes(path)
    if (!entry.name.endsWith(".md")) return []
    const relative = path.slice(distDir.length + 1)
    return CONFIG.ignore.some((prefix) => relative.startsWith(prefix)) ? [] : [path]
  })

/**
 * Every root-relative Markdown link target in a body.
 *
 * `\/(?!\/)` requires exactly one leading slash, so a protocol-relative target is excluded here and
 * caught by its own case below. Without that lookahead a `//host/path` reads as a root-relative link and
 * the case that exists to find it never sees it.
 */
const rootRelativeTargets = (body: string): ReadonlyArray<string> =>
  [...body.matchAll(/\]\((\/(?!\/)[^)]*)\)/g)].map(([, target]) => target as string)

/**
 * Every RELATIVE Markdown link target in a body, with the twin's own directory.
 *
 * The generated tree cross-links this way — `../insights/contract-map.md` and hundreds of siblings — and
 * a relative target is base-agnostic by construction, so nothing rewrites it and nothing needs to. That
 * makes it the shape most likely to go unchecked, which is why it gets its own case: a relative target
 * that resolves to nothing is a 404 for the agent following it, and the link validator is configured not
 * to look at relative links at all.
 */
const relativeTargets = (body: string): ReadonlyArray<string> =>
  [...body.matchAll(/\]\(([^)\s:#][^)\s]*)\)/g)]
    .map(([, target]) => target as string)
    .filter((target) => !target.startsWith("/") && !/^[a-z][a-z0-9+.-]*:/i.test(target))

const isFile = (path: string): boolean => {
  try {
    return statSync(path).isFile()
  } catch {
    return false
  }
}

/* =================================================================================================
 * ================================================================================================= */

describe("the raw Markdown routes", () => {
  const routes = rawRoutes(distDir)

  it("exist, so the rest of this file is not vacuously true", () => {
    /*
     * A floor of one rather than a number. The count belongs to the build, and the strong form of this
     * assertion — a twin for every built page, and no twin without one — lives in
     * `agent-surface.test.ts`, where the page list is already derived. Writing a number here would make
     * adding a page look like a broken test.
     */
    expect(routes.length).toBeGreaterThan(0)
  })

  it("carries the base segment on every root-relative link", () => {
    const offenders = routes.flatMap((file) =>
      rootRelativeTargets(readFileSync(file, "utf8"))
        .filter((target) => !target.startsWith(segment))
        .map((target) => `${file.slice(distDir.length)} -> ${target}`)
    )
    expect(offenders).toEqual([])
  })

  it("carries it exactly once, and never as a host", () => {
    /*
     * The assertion CHANGES SHAPE with the base rather than going vacuous at one of them.
     *
     * At a non-root base the failure is a doubled segment, `/microvms-agentd/microvms-agentd/…` — which
     * this repository has already shipped once, from a plugin that prefixed a base another plugin also
     * prefixed. At the root base there is no segment to double and the analogous defect is a
     * protocol-relative `//path`: a URL naming a HOST rather than a path, which is what concatenating an
     * empty base onto a leading slash produces. Both forms parse, resolve to nothing, and look right.
     */
    const doubled = routes.flatMap((file) => {
      const body = readFileSync(file, "utf8")
      const offenders =
        segment === "/"
          ? [...body.matchAll(/\]\((\/\/[^)]*)\)/g)].map(([, target]) => target as string)
          : rootRelativeTargets(body).filter((target) =>
              target.startsWith(`${segment}${segment.slice(1)}`)
            )
      return offenders.map((target) => `${file.slice(distDir.length)} -> ${target}`)
    })
    expect(doubled).toEqual([])
  })

  it("points every root-relative link at something that was actually built", () => {
    const missing = routes.flatMap((file) =>
      rootRelativeTargets(readFileSync(file, "utf8"))
        .filter((target) => target.startsWith(segment) && !target.includes("#"))
        .map((target) => target.slice(segment.length))
        .filter(
          (path) => !(isFile(join(distDir, path)) || isFile(join(distDir, path, "index.html")))
        )
        .map((path) => `${file.slice(distDir.length)} -> ${segment}${path}`)
    )
    expect(missing).toEqual([])
  })

  it("points every relative link at something that was actually built", () => {
    const missing = routes.flatMap((file) => {
      const here = posix.dirname(file.slice(distDir.length))
      return relativeTargets(readFileSync(file, "utf8"))
        .map((target) => posix.normalize(posix.join(here, (target.split("#")[0] ?? "").trim())))
        .filter((path) => path !== "" && path !== ".")
        .filter(
          (path) => !(isFile(join(distDir, path)) || isFile(join(distDir, path, "index.html")))
        )
        .map((path) => `${file.slice(distDir.length)} -> ${path}`)
    })
    expect(missing).toEqual([])
  })

  it("links twins to twins, so an agent following a link stays on the Markdown surface", () => {
    /*
     * A twin whose links all point at rendered pages walks the agent back into the HTML on the first
     * hop, which spends the tokens the twin was fetched to avoid. A link to a page route is not a defect
     * on its own — a twin legitimately references pages — so this asserts the surface is REACHABLE from
     * itself rather than that every link is a twin. Without it the whole surface can be a set of islands
     * and every case above passes.
     */
    const twinLinks = routes.flatMap((file) => {
      const here = posix.dirname(file.slice(distDir.length))
      return [
        ...rootRelativeTargets(readFileSync(file, "utf8"))
          .filter((target) => target.endsWith(".md"))
          .map((target) => target.slice(segment.length)),
        ...relativeTargets(readFileSync(file, "utf8"))
          .filter((target) => target.endsWith(".md"))
          .map((target) => posix.normalize(posix.join(here, target)))
      ]
    })
    expect(twinLinks.length).toBeGreaterThan(0)
    for (const target of twinLinks) {
      expect(isFile(join(distDir, target)), `${target} resolves to nothing`).toBe(true)
    }
  })
})
