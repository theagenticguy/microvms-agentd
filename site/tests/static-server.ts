// SPDX-License-Identifier: Apache-2.0
import { createReadStream, existsSync, statSync } from "node:fs"
import { createServer, type Server } from "node:http"
import { join, resolve, sep } from "node:path"
import { pathToFileURL } from "node:url"

/**
 * The built site, served from disk at the base segment it was built with.
 *
 * A browser gate needs an origin rather than a directory: `astro build` writes site-absolute URLs
 * (`/microvms-agentd/_astro/…`), so `file://` resolves every stylesheet against the filesystem root
 * and the page renders unstyled, which would make a contrast gate measure a page nobody will ever see.
 *
 * Serving from disk rather than from `astro preview` is deliberate: the bytes under test are then
 * exactly the bytes the Pages artifact contains, and the gate needs no dev server, no Astro config
 * load, and no network. Ported from memhtml-public's `apps/docs/tests/static-server.ts`.
 *
 * Two consumers. The vitest browser suites import `serveStatic`; `lighthouserc.json` runs this file
 * as a command (`startServerCommand`) so Lighthouse measures the same server, the same bytes and the
 * same content types as axe does, instead of lhci's own static server, which can only mount `dist/`
 * at the origin root and so cannot serve a site built under a base.
 */

/**
 * Content types by extension. `.md` is `text/markdown`, which is what the raw twins are and what the
 * head's `rel="alternate" type="text/markdown"` promises; a server answering `text/plain` there would
 * make the a11y tier and the budget tier measure a page whose alternates disagree with its headers.
 */
const TYPES: Readonly<Record<string, string>> = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".ico": "image/x-icon",
  ".jpg": "image/jpeg",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".map": "application/json; charset=utf-8",
  ".md": "text/markdown; charset=utf-8",
  ".pagefind": "application/octet-stream",
  ".pf_fragment": "application/octet-stream",
  ".pf_index": "application/octet-stream",
  ".pf_meta": "application/octet-stream",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".ttf": "font/ttf",
  ".txt": "text/plain; charset=utf-8",
  ".wasm": "application/wasm",
  ".webmanifest": "application/manifest+json",
  ".webp": "image/webp",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
  ".xml": "application/xml"
}

export type StaticSite = {
  readonly origin: string
  readonly close: () => Promise<void>
}

/**
 * @param root absolute path to the built output
 * @param base the site's base: `/` at the origin root, or `/microvms-agentd/` under a path. A
 *   trailing slash is optional and a missing one is tolerated.
 * @param port `0` (the default) lets the kernel pick a free port; the lhci command passes a fixed one.
 */
export const serveStatic = async (root: string, base: string, port = 0): Promise<StaticSite> => {
  const distRoot = resolve(root)

  /*
   * The base with any trailing slash removed, so the root base becomes the empty string.
   *
   * Comparing against the base as written breaks at the root: `${base}/` is `//` there, which no
   * request path starts with, so every page 404s and a11y audits a 404 instead of the page. The
   * suite's own status assertion catches that rather than reporting phantom violations, which is the
   * only reason it is cheap to find.
   */
  const prefix = base.replace(/\/+$/, "")

  const locate = (rawPath: string): string | undefined => {
    if (prefix !== "" && !rawPath.startsWith(`${prefix}/`) && rawPath !== prefix) return undefined
    const withinSite = rawPath.slice(prefix.length) || "/"
    const decoded = decodeURIComponent(withinSite)
    const candidate = resolve(
      distRoot,
      `.${decoded.endsWith("/") ? `${decoded}index.html` : decoded}`
    )
    // Refuse anything that escaped the output directory rather than serving it: the gate must not
    // be able to read a file the deployed site could not.
    if (candidate !== distRoot && !candidate.startsWith(distRoot + sep)) return undefined
    if (existsSync(candidate) && statSync(candidate).isFile()) return candidate
    const asDirectory = join(candidate, "index.html")
    if (existsSync(asDirectory) && statSync(asDirectory).isFile()) return asDirectory
    return undefined
  }

  const server: Server = createServer((request, response) => {
    const url = new URL(request.url ?? "/", "http://localhost")
    const file = locate(url.pathname)
    if (file === undefined) {
      response.writeHead(404, { "content-type": "text/plain; charset=utf-8" })
      response.end(`not in the built site: ${url.pathname}\n`)
      return
    }
    const extension = file.slice(file.lastIndexOf("."))
    response.writeHead(200, { "content-type": TYPES[extension] ?? "application/octet-stream" })
    createReadStream(file).pipe(response)
  })

  await new Promise<void>((done) => server.listen(port, "127.0.0.1", done))
  const address = server.address()
  if (address === null || typeof address === "string") throw new Error("server has no port")

  return {
    origin: `http://127.0.0.1:${address.port}`,
    close: () =>
      new Promise<void>((done, fail) => server.close((err) => (err ? fail(err) : done())))
  }
}

/*
 * Command mode, for `lighthouserc.json`'s `startServerCommand`.
 *
 * `node --experimental-strip-types tests/static-server.ts` serves `dist/` under the base at a fixed
 * port and prints one line lhci waits for. The base and port come from the environment rather than
 * from `src/gates.ts`, because plain node resolves `../src/gates.js` to nothing: that specifier is a
 * TypeScript convention vitest understands and a runtime does not. The normalization is the one
 * `astro.config.ts` applies, so `DOCS_BASE` may be `/x`, `/x/`, `x`, or `/` here as it may there.
 *
 * The guard compares module URLs so importing this file from a test never starts a server.
 */
if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const base =
    `/${(process.env.DOCS_BASE ?? "/microvms-agentd/").replace(/^\/+|\/+$/g, "")}/`.replace(
      /^\/{2,}/,
      "/"
    )
  const port = Number(process.env.STATIC_PORT ?? "4173")
  const dist = resolve(process.cwd(), process.env.DIST_DIR ?? "dist")
  if (!existsSync(dist)) {
    process.stderr.write(`static-server: no ${dist}; build the site first (mise run docs:build)\n`)
    process.exit(2)
  }
  serveStatic(dist, base, port).then((site) => {
    process.stdout.write(`serving ${dist} at ${site.origin}${base}\n`)
  })
}
