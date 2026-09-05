// SPDX-License-Identifier: Apache-2.0
import { existsSync, readdirSync, readFileSync } from "node:fs"
import { dirname, join, relative } from "node:path"
import { fileURLToPath } from "node:url"
import { describe, expect, it } from "vitest"

import { BASE } from "../src/gates.js"

/**
 * Every internal link an author wrote, resolved against the pages that will exist. Ported from
 * memhtml-public's `apps/docs/tests/authored-links.test.ts` and widened to this site's producers.
 *
 * `starlight-links-validator` runs at build time and gates the rendered tree, so this is not a second
 * copy of that check. It exists for the three things the validator cannot say:
 *
 * - it runs BEFORE a build, in milliseconds, against the sources a writer is editing, so a Learn
 *   author finds a broken link to a Reference page without waiting for a whole `astro build`;
 * - it refuses a hand-built base prefix. A link written as `/microvms-agentd/learn/` is correct today
 *   and wrong the moment the site moves to a custom domain, because `starlight-base-path` prefixes
 *   the base onto it a second time, and the validator sees the doubled path as a broken link only
 *   after the move;
 * - it refuses a raw `<a href>`, which bypasses `starlight-base-path` altogether and 404s under the
 *   site base while the validator, which reads Markdown links, never sees it.
 *
 * The resolution set is the union of three producers, because three tools write pages into the
 * content directory and a link into any of them is legitimate: `authored/**` (this file's subject,
 * mirrored 1:1 into the content root), the sync manifest's `owned` list (every path
 * `scripts/sync-docs.mjs` wrote, which is every route it published), and everything under the content
 * directory after a sync, which is where the generated Reference tier lands. The content directory
 * is gitignored and generated, so this suite refuses to run without it rather than resolving against
 * the authored pages alone and reporting every Reference link broken.
 */

const site = dirname(dirname(fileURLToPath(import.meta.url)))
const authored = join(site, "authored")
const content = join(site, "src", "content", "docs")
const publicDir = join(site, "public")
const MANIFEST = join(content, ".sync-manifest.json")

const walk = (dir: string, keep: (name: string) => boolean): ReadonlyArray<string> =>
  readdirSync(dir, { withFileTypes: true }).flatMap((entry) =>
    entry.isDirectory()
      ? walk(join(dir, entry.name), keep)
      : keep(entry.name)
        ? [join(dir, entry.name)]
        : []
  )

const isPage = (name: string): boolean => /\.mdx?$/.test(name)

/**
 * The route a content path serves, as a leading-and-trailing-slashed path: `learn/index.md` is
 * `/learn/`, `index.md` is `/`, `agents.md` is `/agents/`. Lowercased because Astro's glob loader
 * lowercases the slug, which is the one transformation `sync-docs.mjs` also applies.
 */
const routeOf = (relativePath: string): string => {
  const id = relativePath
    .replace(/\\/g, "/")
    .replace(/\.mdx?$/, "")
    .toLowerCase()
  const withoutIndex = id === "index" ? "" : id.replace(/\/index$/, "")
  return `/${withoutIndex}${withoutIndex === "" ? "" : "/"}`
}

if (!existsSync(content) || !existsSync(MANIFEST)) {
  throw new Error(
    `${relative(site, content)} or its ${relative(content, MANIFEST)} is absent. The content ` +
      "directory is generated: run `pnpm run sync` first. This suite resolves links against the " +
      "generated pages too, so it cannot run against the authored pages alone."
  )
}

const authoredFiles = walk(authored, isPage)
const authoredRoutes = new Set(authoredFiles.map((file) => routeOf(relative(authored, file))))

const manifest = JSON.parse(readFileSync(MANIFEST, "utf8")) as { readonly owned?: unknown }
const ownedRoutes = new Set(
  (Array.isArray(manifest.owned) ? manifest.owned : [])
    .filter((path): path is string => typeof path === "string" && isPage(path))
    .map(routeOf)
)

const contentRoutes = new Set(walk(content, isPage).map((file) => routeOf(relative(content, file))))

const routes = new Set([...authoredRoutes, ...ownedRoutes, ...contentRoutes])

/**
 * Files the site serves that are not pages: the `llms` bundles `starlight-llms-txt` emits at the site
 * root, and everything under `public/` (the wire schema `sync-docs.mjs` copies there, the favicons).
 * The agent page links these on purpose, and no page route resolves them.
 */
const MACHINE_SURFACES: ReadonlySet<string> = new Set([
  "/llms.txt",
  "/llms-full.txt",
  "/llms-small.txt",
  ...(existsSync(publicDir)
    ? walk(publicDir, () => true).map((file) => `/${relative(publicDir, file).replace(/\\/g, "/")}`)
    : [])
])

/**
 * Markdown links only: `[label](target)` and the reference form `[label]: target`.
 *
 * Fenced and inline code are masked first. A link written inside a fence is being SHOWN, not made,
 * and resolving it would report a documentation example of a URL as a broken link.
 */
const linksIn = (body: string): ReadonlyArray<string> => {
  const prose = body
    .replace(/^ {0,3}(`{3,}|~{3,})[^\n]*\n[\s\S]*?^ {0,3}\1[^\n]*$/gm, "")
    .replace(/`[^`\n]*`/g, "")
  return [
    ...[...prose.matchAll(/\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g)].map((match) => match[1] ?? ""),
    ...[...prose.matchAll(/^ {0,3}\[[^\]]+\]:\s*(\S+)/gm)].map((match) => match[1] ?? "")
  ]
}

const internal = (link: string): boolean => link.startsWith("/")

/**
 * A link to a page's raw Markdown route, which `starlight-md-txt` serves at the page's own path with a
 * `.md` suffix. `/reference/commands/run.md` is the raw route of the page at
 * `/reference/commands/run/`, so it resolves exactly when that page does; `/index.md` is the root's.
 */
const rawRouteTarget = (link: string): string | undefined => {
  if (!link.endsWith(".md")) return undefined
  const stem = link.slice(0, -".md".length)
  return stem === "/index" ? "/" : `${stem}/`
}

const at = (file: string, link: string): string => `${relative(site, file)} -> ${link}`

const authoredLinks = authoredFiles.flatMap((file) =>
  linksIn(readFileSync(file, "utf8"))
    .filter(internal)
    .map((link) => ({ file, link }))
)

describe("every authored internal link", () => {
  it("finds files to check, links inside them, and generated pages to resolve against", () => {
    expect(authoredFiles.length, "authored/ holds no pages").toBeGreaterThan(1)
    expect(authoredLinks.length, "no internal links were found in authored/").toBeGreaterThan(0)
    expect(contentRoutes.size, "the content directory holds no pages").toBeGreaterThan(
      authoredRoutes.size
    )
  })

  it("resolves to a page that will exist", () => {
    const broken: Array<string> = []
    for (const { file, link } of authoredLinks) {
      const target = link.split("#")[0] ?? link
      if (MACHINE_SURFACES.has(target)) continue
      const asRawRoute = rawRouteTarget(target)
      if (asRawRoute !== undefined) {
        if (!routes.has(asRawRoute)) broken.push(at(file, link))
        continue
      }
      if (!routes.has(target)) broken.push(at(file, link))
    }
    expect(broken, "each link names a route no producer emits").toEqual([])
  })

  it("names a page with a trailing slash, the one shape both surfaces serve", () => {
    // `trailingSlash: "always"` in astro.config.ts: `/learn` redirects on Pages and 404s on the raw
    // twin surface, where nothing rewrites it. A link to a file (`.md`, `.json`, `.txt`) is exempt.
    const bare = authoredLinks
      .filter(({ link }) => {
        const target = link.split("#")[0] ?? link
        return !target.endsWith("/") && !/\.[a-z0-9]+$/i.test(target)
      })
      .map(({ file, link }) => at(file, link))
    expect(bare).toEqual([])
  })

  it("is written without the site base, which the base-path plugin prefixes", () => {
    // At the root base every link starts with `/`, so there is nothing to refuse there; the check
    // is against the base this site is actually built under.
    if (BASE === "/") return
    const prefixed = authoredLinks
      .filter(({ link }) => link === BASE.slice(0, -1) || link.startsWith(BASE))
      .map(({ file, link }) => at(file, link))
    expect(prefixed, `links must not start with the base ${BASE}`).toEqual([])
  })

  /*
   * A `<a href>` in raw HTML bypasses `starlight-base-path`, so it would 404 under the site base. The
   * check is on authored files only; the components under `src/` build their URLs with `new URL()`
   * over `import.meta.env.BASE_URL`, which is the accessor that includes the base.
   */
  it("is never a hand-written href in raw HTML", () => {
    for (const file of authoredFiles) {
      expect(readFileSync(file, "utf8"), relative(site, file)).not.toMatch(/<a\s[^>]*href=/i)
    }
  })
})
