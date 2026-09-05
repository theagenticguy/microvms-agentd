// SPDX-License-Identifier: Apache-2.0
import { readdirSync, readFileSync, statSync } from "node:fs"
import { join, relative } from "node:path"
import { fileURLToPath } from "node:url"

import { describe, expect, it } from "vitest"
import lighthouserc from "../lighthouserc.json" with { type: "json" }
import {
  AUDITED_PAGES,
  BASE,
  DENYLIST,
  DIST_DIR,
  LAYOUT_SHIFT_CEILING,
  TOTAL_BYTE_WEIGHT_BUDGET
} from "../src/gates.js"
import browserConfig from "../vitest.a11y.config.js"
import unitConfig from "../vitest.config.js"

/**
 * Gates over the BUILT site. Ported from memhtml-public's `apps/docs/tests/built-site.test.ts`.
 *
 * Every case here reads `dist/`, which `mise run docs:check` builds first. Calling vitest directly
 * skips that, so `distFiles` fails loudly on an absent `dist/` rather than reporting a clean scan of
 * nothing.
 *
 * The scan targets the three surfaces a reader or an agent can fetch: rendered HTML, the raw `.md`
 * route of every page, and the `.txt` bundles. The `_astro/` chunks are excluded: they are compiled
 * vendor JavaScript, no prose of ours survives in them, and minified identifiers are a false-positive
 * farm. `pagefind/` is the search index, a binary encoding of the same pages.
 */

const packageRoot = fileURLToPath(new URL("..", import.meta.url))
const dist = join(packageRoot, DIST_DIR)

const walk = (directory: string): ReadonlyArray<string> => {
  const found: string[] = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const full = join(directory, entry.name)
    if (entry.isDirectory()) found.push(...walk(full))
    else if (entry.isFile()) found.push(full)
  }
  return found
}

const distFiles = (): ReadonlyArray<string> => {
  try {
    statSync(dist)
  } catch {
    throw new Error(
      `no ${DIST_DIR}/ under ${packageRoot}; build the site first (\`mise run docs:build\`). ` +
        "These gates read the built output, and a missing directory is not an empty one."
    )
  }
  return walk(dist)
}

/** The three fetchable surfaces, as {path, text} pairs read once and shared by every case. */
const surfaces = (() => {
  const wanted = (file: string): boolean => {
    const name = relative(dist, file)
    if (name.startsWith("_astro/") || name.startsWith("pagefind/")) return false
    return name.endsWith(".html") || name.endsWith(".md") || name.endsWith(".txt")
  }
  return distFiles()
    .filter(wanted)
    .map((file) => ({ path: relative(dist, file), text: readFileSync(file, "utf8") }))
})()

/**
 * Astro's `redirects` emit one HTML stub per retired URL: a `<meta http-equiv="refresh">` and one
 * anchor, a few hundred bytes, with no `.md` twin because there is no page. They are scanned for
 * denylisted tokens like everything else and set aside for every census that counts PAGES, where a
 * stub would read either as a page without a twin or as a page without content.
 */
const isRedirectStub = (text: string): boolean =>
  /<meta http-equiv="refresh"/i.test(text) && text.length < 2_000

const htmlPages = surfaces.filter((one) => one.path.endsWith(".html") && !isRedirectStub(one.text))
const redirectStubs = surfaces.filter(
  (one) => one.path.endsWith(".html") && isRedirectStub(one.text)
)

describe("no denylisted token reaches a public page", () => {
  /*
   * One case per term rather than one case for the list: the report then names the term that leaked,
   * and a second leak does not hide behind the first.
   *
   * `starlight-llms-txt` has its own `exclude` option and it is NOT a substitute for this: that
   * option filters `llms-small.txt` alone, leaving `llms.txt`, `llms-full.txt`, and every `.md`
   * route untouched. Exclusion cannot be delegated to plugin config, so it is asserted over the
   * whole output instead.
   */
  for (const { pattern, why } of DENYLIST) {
    it(`refuses ${pattern.source}`, () => {
      const hits = surfaces
        .filter((one) => pattern.test(one.text))
        .map((one) => {
          const line = one.text.split("\n").findIndex((text) => pattern.test(text)) + 1
          return `${one.path}:${line}`
        })
      expect(hits, `${pattern.source} must not reach a public page: ${why}`).toEqual([])
    })
  }

  it("scans every page of the site, on all three surfaces", () => {
    // Independently derived: Starlight writes one `index.html` per route plus `404.html`, and
    // `starlight-md-txt` writes one `.md` per route. Equal counts is the invariant; a plugin that
    // stopped emitting `.md` would otherwise shrink the scan silently. The floor is there so an
    // empty or half-built `dist/` cannot pass as a clean one.
    const md = surfaces.filter((one) => one.path.endsWith(".md"))
    expect(htmlPages.length).toBeGreaterThan(20)
    // `x/index.html` twins as `x.md`; the root's `index.html` twins as `index.md` because `rootTwin()`
    // renames the dotfile `starlight-md-txt` emits; `404.html` twins as `404.md`.
    const twinOf = (page: string): string =>
      page === "index.html" ? "index.md" : page.replace(/(?:\/index)?\.html$/, ".md")
    expect(
      md.map((one) => one.path).sort(),
      "every page has one .md twin and every .md twin has a page"
    ).toEqual(htmlPages.map((one) => twinOf(one.path)).sort())
    expect(
      surfaces
        .filter((one) => one.path.startsWith("llms"))
        .map((one) => one.path)
        .sort()
    ).toEqual(["llms-full.txt", "llms-small.txt", "llms.txt"])
  })

  /*
   * A redirect stub that points at nothing is the URL plan's worst outcome: the old address stops
   * answering with the page and starts answering with a hop to a 404, on every link anyone ever
   * published. `astro.config.ts` declares the redirects by hand, so nothing but this checks them
   * against what the build emitted.
   */
  it("points every redirect stub at a page the build emitted", () => {
    const missing: string[] = []
    for (const stub of redirectStubs) {
      const target = /content="0;url=([^"]+)"/.exec(stub.text)?.[1]
      if (target === undefined) {
        missing.push(`${stub.path}: no refresh target`)
        continue
      }
      const within = target.startsWith(BASE)
        ? target.slice(BASE.length)
        : target.replace(/^\/+/, "")
      const page = `${within}${within === "" || within.endsWith("/") ? "" : "/"}index.html`
      if (!htmlPages.some((one) => one.path === page)) missing.push(`${stub.path} -> ${target}`)
    }
    expect(missing).toEqual([])
  })
})

describe("what axe cannot see", () => {
  /*
   * axe-core passes `alt="Diagram"`: `image-alt` asks whether an accessible name exists, never
   * whether it says anything. This is the census that does, over every page rather than the few the
   * browser tier audits, because a placeholder alt can be authored anywhere.
   */
  const PLACEHOLDER_ALT =
    /^(image|img|picture|photo|screenshot|diagram|figure|chart|graph|illustration|graphic|icon)s?$/i

  it("gives every image an alt that says something", () => {
    const offences: string[] = []
    for (const page of htmlPages) {
      for (const tag of page.text.match(/<img\b[^>]*>/g) ?? []) {
        const alt = /\balt\s*=\s*"([^"]*)"/.exec(tag)
        if (!alt) offences.push(`${page.path}: no alt: ${tag.slice(0, 90)}`)
        else if (alt[1] !== undefined && PLACEHOLDER_ALT.test(alt[1].trim()))
          offences.push(`${page.path}: placeholder alt "${alt[1]}"`)
      }
    }
    expect(offences).toEqual([])
  })

  /*
   * axe ignores an inline `<svg>` that carries no `role="img"`, and inline SVG is exactly how this
   * site draws every icon and every build-time Mermaid figure. `tests/a11y.test.ts` decides each one
   * in a real DOM, where an `aria-hidden` ancestor is visible; Starlight wraps the heading-anchor icon
   * in `<span aria-hidden="true">`, so a check that could only read the `<svg>` tag itself would
   * report a violation that does not exist.
   *
   * A small sample could still under-report, and this closes that: every distinct `<svg>` SHAPE the
   * whole site emits also appears on one of the audited pages. A shape is the opening tag with its
   * geometry removed: every Mermaid figure has its own `viewBox`, `width` and `height`, and a census
   * over the literal tags would demand a page per diagram, where what the browser tier needs to see
   * is one figure of that kind. The tags are component output, so the distinct set is small and
   * stable, and a new icon shape that lands only on an unaudited page fails here instead of going
   * unexamined.
   */
  it("emits no inline SVG shape the browser tier never sees", () => {
    const openings = (text: string): ReadonlyArray<string> =>
      (text.match(/<svg\b[^>]*>/g) ?? []).map((tag) =>
        tag.replace(/\s+(?:style|viewBox|width|height)="[^"]*"/g, "")
      )

    const auditedPaths = new Set(AUDITED_PAGES.map((url) => `${url.slice(BASE.length)}index.html`))
    const present = htmlPages.filter((page) => auditedPaths.has(page.path))
    expect(
      present.map((page) => page.path).sort(),
      "every AUDITED_PAGES entry in src/gates.ts must be a page of the built site"
    ).toEqual([...auditedPaths].sort())
    const audited = new Set(present.flatMap((page) => openings(page.text)))
    expect(audited.size).toBeGreaterThan(0)

    const unseen = new Map<string, string>()
    for (const page of htmlPages)
      for (const tag of openings(page.text)) if (!audited.has(tag)) unseen.set(tag, page.path)
    expect(
      [...unseen].map(([tag, page]) => `${page}: ${tag.slice(0, 120)}`),
      "add the page carrying this icon to AUDITED_PAGES, or audit the icon where it already appears"
    ).toEqual([])
  })

  /*
   * The third blind spot, SC 1.4.12 text spacing, axe does not test at all: it needs the page
   * re-laid-out under the WCAG spacing overrides, which only a browser can do. `tests/a11y.test.ts`
   * does it there; nothing about it is checkable from the bytes, so this case asserts only that the
   * probe still exists rather than pretending to cover the criterion.
   */
  it("keeps a text-spacing probe in the browser tier, since nothing here can cover SC 1.4.12", () => {
    const suite = readFileSync(join(packageRoot, "tests/a11y.test.ts"), "utf8")
    expect(suite).toContain("letter-spacing: 0.12em")
    expect(suite).toContain("word-spacing: 0.16em")
    expect(suite).toContain("line-height: 1.5")
  })
})

describe("the two gate configurations name the same pages", () => {
  /*
   * `lighthouserc.json` is JSON and cannot import `src/gates.ts`, so the page set is written twice.
   * This is the drift gate that makes the duplication safe: two files that must agree, and no
   * mechanism forcing them to, is how the same shape of defect shipped elsewhere in this repository
   * (the `docs.yml` node pin against `engines.node`, gated by `tests/toolchain.test.ts`).
   *
   * lhci is pointed at `tests/static-server.ts` rather than at its own `staticDistDir`, because that
   * server can only mount `dist/` at the origin root while this site is built under a base. So the
   * URLs carry an origin, and the comparison is on their paths.
   */
  it("audits the same URLs for accessibility and for the performance budget", () => {
    const paths = lighthouserc.ci.collect.url.map((url) => new URL(url).pathname)
    expect(paths).toEqual([...AUDITED_PAGES])
  })

  it("keeps every audited URL under the base the site is built with", () => {
    for (const url of AUDITED_PAGES) expect(url.startsWith(BASE)).toBe(true)
  })

  it("serves Lighthouse from the same static server the accessibility tier uses", () => {
    expect(lighthouserc.ci.collect.startServerCommand).toContain("tests/static-server.ts")
    const origins = new Set(lighthouserc.ci.collect.url.map((url) => new URL(url).origin))
    expect(origins.size).toBe(1)
    const [origin] = origins
    expect(lighthouserc.ci.collect.startServerCommand).toContain(
      `STATIC_PORT=${new URL(origin ?? "").port}`
    )
  })

  /**
   * An assertion's options object, or a failure naming the id that has none.
   *
   * lhci accepts two spellings, `"error"` alone or `["error", { … }]`, so reading `[1]` off the
   * short form would be `undefined` and every expectation below would silently pass on it.
   */
  const optionsOf = (
    id: string
  ): { minScore?: number; maxNumericValue?: number; aggregationMethod?: string } => {
    const declared: unknown = (lighthouserc.ci.assert.assertions as Record<string, unknown>)[id]
    if (!Array.isArray(declared) || typeof declared[1] !== "object" || declared[1] === null) {
      throw new Error(`lighthouserc.json declares ${id} with no options object`)
    }
    return declared[1] as {
      minScore?: number
      maxNumericValue?: number
      aggregationMethod?: string
    }
  }

  /**
   * The performance category is asserted `optimistic`, and every other category `median`.
   *
   * The asymmetry is the whole point and it is a measured decision, so it is gated rather than left
   * as a line in a JSON file nobody diffs. Lighthouse's CLS reading competes with its own viewport
   * emulation on a loaded machine (`tests/layout-stability.test.ts` records the measurement).
   * Contention can only depress a static page's score, never inflate it, so the best of three runs is
   * the reading least polluted by the harness, and layout stability is gated deterministically by
   * that probe instead.
   *
   * `median` on the other categories is deliberate in the same breath: accessibility, best practices
   * and SEO are computed from the DOM rather than from timings, so they do not move under load, and
   * `optimistic` there would let one lucky run hide a real regression.
   */
  it("aggregates the performance category optimistically and every other one at the median", () => {
    expect(optionsOf("categories:performance").aggregationMethod).toBe("optimistic")
    for (const category of [
      "categories:accessibility",
      "categories:best-practices",
      "categories:seo"
    ]) {
      expect(optionsOf(category).aggregationMethod, category).toBe("median")
    }
  })

  it("holds the category floors the brief set: 0.9 performance, 0.95 a11y and best practices, 1 SEO", () => {
    expect(optionsOf("categories:performance").minScore).toBe(0.9)
    expect(optionsOf("categories:accessibility").minScore).toBe(0.95)
    expect(optionsOf("categories:best-practices").minScore).toBe(0.95)
    expect(optionsOf("categories:seo").minScore).toBe(1)
  })

  /**
   * The byte budget is written in two places for the same reason the page list is, and it is the
   * number most likely to be edited in a hurry: a dependency ships a bigger client bundle, the gate
   * goes red, and the fix is one digit in a JSON file. Keeping it beside its measurement in
   * `src/gates.ts` means the edit lands next to the comment that says what the budget was measured
   * against and why it has the headroom it has.
   */
  it("keeps the byte budget equal to the measured one recorded in src/gates.ts", () => {
    expect(TOTAL_BYTE_WEIGHT_BUDGET).toBeGreaterThan(0)
    expect(optionsOf("total-byte-weight").maxNumericValue).toBe(TOTAL_BYTE_WEIGHT_BUDGET)
    expect(optionsOf("total-byte-weight").aggregationMethod).toBe("pessimistic")
  })

  it("does not gate on Lighthouse's own CLS reading, which the layout probe replaces", () => {
    const assertions = lighthouserc.ci.assert.assertions as Record<string, unknown>
    expect(assertions["cumulative-layout-shift"]).toBeUndefined()
    // The number lives in one place; this is only here so a future CLS assertion in lhci is a
    // deliberate decision made against the probe's ceiling rather than a default nobody chose.
    expect(LAYOUT_SHIFT_CEILING).toBe(0.1)
  })

  it("runs the desktop preset three times and skips only the two audits a static host cannot pass", () => {
    expect(lighthouserc.ci.collect.numberOfRuns).toBe(3)
    expect(lighthouserc.ci.collect.settings.preset).toBe("desktop")
    // `uses-http2`: the static server speaks HTTP/1.1 and Pages decides the real protocol.
    // `canonical`: Starlight emits `rel="canonical"` against the production origin, which the audit
    // reads as pointing off-site because the measured page is served from 127.0.0.1.
    expect([...lighthouserc.ci.collect.settings.skipAudits].sort()).toEqual([
      "canonical",
      "uses-http2"
    ])
    expect(lighthouserc.ci.assert.assertions["unminified-css"]).toBe("error")
    expect(lighthouserc.ci.assert.assertions["unminified-javascript"]).toBe("error")
  })

  /**
   * Every suite that drives a browser is registered in BOTH vitest configs.
   *
   * A browser suite has to be named twice, excluded from the default tier and included in the
   * browser tier, and nothing but this case forces the pair. Getting it wrong is silent in the
   * worst direction: the suite still passes, having run twice, the second time in a tier with no
   * `fileParallelism: false`, so two Chromiums share a runner while one of them measures when a
   * layout settles.
   *
   * An `import … from "playwright"` at the start of a line is the tell, which is exactly how the
   * mistake happens: someone writes the import and the discovery glob picks the file up for free. The
   * pattern is anchored so this file, which has to name the module to look for it, is not itself a
   * browser suite.
   */
  it("registers every browser suite in both vitest configs", () => {
    const DRIVES_A_BROWSER = /^import [^\n]*from "playwright"/m
    const testsDir = join(packageRoot, "tests")
    const driversOfBrowsers = readdirSync(testsDir)
      .filter((name) => name.endsWith(".test.ts"))
      .filter((name) => DRIVES_A_BROWSER.test(readFileSync(join(testsDir, name), "utf8")))
      .map((name) => `tests/${name}`)

    expect(driversOfBrowsers.length).toBeGreaterThan(0)
    for (const suite of driversOfBrowsers) {
      expect(unitConfig.test?.exclude, `${suite} must be excluded from the default tier`).toContain(
        suite
      )
      expect(browserConfig.test?.include, `${suite} must run in the browser tier`).toContain(suite)
    }
  })
})
