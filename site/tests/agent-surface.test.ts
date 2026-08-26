// SPDX-License-Identifier: Apache-2.0
import { existsSync, readdirSync, readFileSync } from "node:fs"
import { dirname, join } from "node:path"
import { fileURLToPath } from "node:url"
import { describe, expect, it } from "vitest"

/*
 * The constants are IMPORTED rather than restated in `CONFIG`, and the difference matters: a test that
 * hardcodes the note's label or class still passes after someone renames it in the source, which is the
 * rename this probe exists to catch.
 */
import { AGENT_NOTE_CLASS, AGENT_NOTE_LABEL } from "../src/lib/agent-note.js"
import {
  DEEP_LINK_BUDGET,
  DEEP_LINK_TARGETS,
  DOCS_NAME,
  deepLink,
  deepLinks,
  discoveryLinks,
  rawMarkdownUrl,
  referencePrompt,
  siteUrl
} from "../src/lib/agent-surface.js"

/**
 * The dead-button lock, and the checks over the built agent surface.
 *
 * Every case here reads `dist/`. That is the point rather than an inconvenience: the subject of these
 * assertions is the bytes a browser and an agent actually receive, and a check against a component's
 * inputs passes while the emitted href is wrong.
 *
 * Every quantity is DERIVED from the build. An assertion that four controls ship stops being true the
 * day a fifth is added, and reports as a defect in the change rather than in the test. The one exception
 * is `CONFIG.verifiedDeepLinks`, whose literals are facts about systems this repo does not control,
 * established by probing them: a literal is the honest form there, and the failure it prevents is a
 * control whose href has drifted from its target's published shape.
 */

/* =================================================================================================
 * CONFIG — the whole site-specific surface of this file.
 * ================================================================================================= */

const CONFIG = {
  /** Built output, relative to the package root. */
  dist: "dist",
  /** Generated content, relative to the package root, as path segments. */
  content: ["src", "content", "docs"],
  /** Authored content: what a writer typed, which is the subject of the authorship probes. */
  authored: ["authored"],

  /**
   * The deployed origin, with NO base segment — what `Astro.site` holds.
   *
   * Read from the same variable `astro.config.ts` reads, and the default matches its default, so the
   * two cannot disagree about which output is being inspected. Hardcoding it made the `llms.txt`
   * ordering probe compare against this repository's origin whatever origin the build used, which
   * silently passes the wrong assertion on a fork and on a custom domain.
   */
  origin: (process.env.DOCS_SITE ?? "https://theagenticguy.github.io").replace(/\/+$/, ""),

  /** The base the build ran with, from the same variable, for the same reason. */
  base: process.env.DOCS_BASE ?? "/microvms-agentd/",

  /**
   * A DIFFERENT non-root base for the pure-function cases, so a case that exercises base handling
   * cannot pass by coincidence against the one the build happened to use.
   */
  probeBase: "/probe",

  /** The agent page's content-collection entry id. */
  agentPage: "agents",

  /**
   * The floor on how many pages carry an agent note. Two, because two pages on this site are authored
   * and every other one is a generated output of a tool that owns its own body — a `:::agent` block
   * written into a generated page is discarded by the next sync. This is a floor on a practice, not a
   * count of the corpus.
   */
  minAgentNotePages: 2,

  /** The relations the head discovery block emits, in order. */
  discoveryRels: ["alternate", "index", "llms-full-txt"] as const,

  /** Which of those relations this site invented rather than adopted. */
  inventedRels: ["llms-full-txt"] as const,

  /**
   * Twins the build emits for something that is not a page.
   *
   * `starlight-md-txt` injects a `/404.md` route answering with HTTP 404 unless the collection already
   * holds a `404` entry. Named here so the orphan-twin probe stays strict about everything else.
   */
  twinsWithoutPages: ["404.md"] as const,

  /**
   * The verified deep-link formats, probed 2026-08-12. RE-PROBE AND UPDATE THE DATE when a control
   * changes: a vendor parameter that changed spelling produces a control that opens the assistant with
   * an empty prompt, and nothing on the page looks wrong.
   *
   * Codex is absent because it has no web prompt parameter — confirmed absent rather than merely
   * undocumented — so the missing row is an assertion, not an omission.
   */
  verifiedDeepLinks: [
    ["chatgpt", "https://chatgpt.com/?q="],
    ["claude", "https://claude.ai/new?q="],
    ["claude-code", "https://claude.ai/code?prompt="],
    ["cursor", "https://cursor.com/link/prompt?text="]
  ] as ReadonlyArray<readonly [string, string]>,

  /**
   * Nouns a registry owns. A digit-led quantity in front of one of these on the agent page is a
   * hand-written count, which is right on the day it is typed and wrong from the next commit — and the
   * reader least able to check it is the one that page addresses.
   */
  countedNouns: [
    "commands?",
    "crates?",
    "routes?",
    "endpoints?",
    "exit codes?",
    "response types?",
    "requirements?",
    "checks?",
    "pages?"
  ],

  /**
   * Class-name fragments and declarations that remove content from sight while leaving it in the DOM.
   * Used by the cloaking probe, which is the one check in this file that asserts an ABSENCE.
   */
  hidingClasses: ["sr-only", "visually-hidden", "screen-reader", "visuallyhidden", "hidden-text"],
  hidingDeclarations: [
    /display\s*:\s*none/i,
    /visibility\s*:\s*hidden/i,
    /clip\s*:\s*rect/i,
    /clip-path\s*:\s*inset\(\s*(?:100%|50%)/i,
    /font-size\s*:\s*0(?![.\d])/i,
    /opacity\s*:\s*0(?![.\d])/i,
    /(?:^|[;{\s])(?:width|height)\s*:\s*1px/i,
    /(?:left|top)\s*:\s*-\d{4,}px/i,
    /text-indent\s*:\s*-\d{4,}px/i
  ]
} as const

/* =================================================================================================
 * Reading the build.
 * ================================================================================================= */

const root = dirname(dirname(fileURLToPath(import.meta.url)))
const dist = join(root, CONFIG.dist)
const content = join(root, ...CONFIG.content)
const authored = join(root, ...CONFIG.authored)

/** The base as a segment with a trailing slash, which is what every path comparison below wants. */
const segment = CONFIG.base.endsWith("/") ? CONFIG.base : `${CONFIG.base}/`

/** The context the pure functions are exercised with: a real origin and a non-root base. */
const CONTEXT = { site: new URL(CONFIG.origin), base: CONFIG.probeBase }

/** Same, as the URL prefix everything built from it must start with. */
const PROBE_PREFIX = `${CONFIG.origin}${CONFIG.probeBase}/`

/**
 * The context matching the build being read. Used only where an assertion compares a produced URL
 * against a path on disk; every case that exercises base handling itself uses `CONTEXT`.
 */
const BUILT_CONTEXT = { site: new URL(CONFIG.origin), base: CONFIG.base }

const readDist = (relative: string): string => {
  const path = join(dist, relative)
  if (!existsSync(path)) {
    throw new Error(
      `\`${CONFIG.dist}/${relative}\` is absent — run \`pnpm run build\` before this suite`
    )
  }
  return readFileSync(path, "utf8")
}

/** A site-absolute path as a path inside `dist/`. */
const served = (path: string): string =>
  path.startsWith(segment) ? path.slice(segment.length) : path.replace(/^\/+/, "")

/** Every page directory the build emitted, as the site-absolute paths a browser would request. */
const builtPages = (): ReadonlyArray<string> => {
  const walk = (directory: string, prefix: string): ReadonlyArray<string> =>
    readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
      if (entry.name.startsWith("_") || entry.name === "pagefind") return []
      if (entry.isDirectory()) return walk(join(directory, entry.name), `${prefix}${entry.name}/`)
      return entry.name === "index.html" ? [prefix] : []
    })
  return walk(dist, segment)
}

/** Every raw Markdown twin the build emitted, as `dist/`-relative paths. */
const builtTwins = (): ReadonlyArray<string> => {
  const walk = (directory: string, prefix: string): ReadonlyArray<string> =>
    readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
      if (entry.name.startsWith("_") || entry.name === "pagefind") return []
      if (entry.isDirectory()) return walk(join(directory, entry.name), `${prefix}${entry.name}/`)
      return entry.name.endsWith(".md") ? [`${prefix}${entry.name}`] : []
    })
  return walk(dist, "")
}

/** One built page's HTML. */
const pageHtml = (path: string): string => readDist(join(served(path), "index.html"))

/** Every `href` in a built document. */
const hrefs = (html: string): ReadonlyArray<string> =>
  [...html.matchAll(/href="([^"]*)"/g)].map((match) => match[1] ?? "")

/** Every stylesheet the build emitted. */
const builtStylesheets = (): ReadonlyArray<string> => {
  const walk = (directory: string): ReadonlyArray<string> =>
    readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
      const path = join(directory, entry.name)
      if (entry.isDirectory()) return walk(path)
      return entry.name.endsWith(".css") ? [path] : []
    })
  return walk(dist)
}

/** The generated source of one entry, whichever extension it uses. */
const contentSource = (page: string): string => {
  const md = join(content, `${page}.md`)
  return readFileSync(existsSync(md) ? md : join(content, `${page}.mdx`), "utf8")
}

/* =================================================================================================
 * The deep-link format lock.
 * ================================================================================================= */

describe("the deep-link format lock", () => {
  const VERIFIED = new Map(CONFIG.verifiedDeepLinks)

  const page = {
    title: "For agents",
    pageUrl: `${PROBE_PREFIX}${CONFIG.agentPage}/`,
    markdownUrl: `${PROBE_PREFIX}${CONFIG.agentPage}.md`
  }

  it("ships a control for every verified target and for no other", () => {
    expect(DEEP_LINK_TARGETS.map((target) => target.id).sort()).toEqual([...VERIFIED.keys()].sort())
  })

  it("ships no control on a desktop URL scheme, which does nothing without the app installed", () => {
    for (const target of DEEP_LINK_TARGETS) {
      expect(target.endpoint.startsWith("https://")).toBe(true)
      expect(new URL(target.endpoint).hostname).not.toBe("")
    }
  })

  it.each([...VERIFIED])("builds %s's href in its verified format", (id, format) => {
    const target = DEEP_LINK_TARGETS.find((candidate) => candidate.id === id)
    if (target === undefined) throw new Error(`no target \`${id}\``)
    for (const body of ["", "a short body", "x".repeat(50_000)]) {
      const link = deepLink(target, page, body)
      expect(link.href.startsWith(format)).toBe(true)
      // A format prefix with an empty payload behind it is the dead button in its purest form.
      expect(link.href.length).toBeGreaterThan(format.length)
      expect(link.href.length).toBeLessThanOrEqual(DEEP_LINK_BUDGET)
    }
  })

  it("labels every control as opening the target, never as asking it", () => {
    for (const target of DEEP_LINK_TARGETS) {
      expect(target.label.startsWith("Open in ")).toBe(true)
      expect(target.label.toLowerCase()).not.toContain("ask")
    }
  })

  it("respects a vendor ceiling stricter than ours", () => {
    const [first] = DEEP_LINK_TARGETS
    if (first === undefined) throw new Error("the target table is empty")
    const link = deepLink({ ...first, vendorLimit: 400 }, page, "y".repeat(5_000))
    expect(link.href.length).toBeLessThanOrEqual(400)
    expect(link.carriesContent).toBe(false)
  })

  it("carries the page's Markdown when it fits and its URL when it does not", () => {
    /*
     * Read back through `searchParams` rather than `decodeURIComponent`: a query string encodes a space
     * as `+`, which `decodeURIComponent` leaves alone, so decoding by hand compares the payload against
     * a string it can never equal.
     */
    const payload = (link: (typeof short)[number]): string =>
      new URL(link.href).searchParams.get(link.target.parameter) ?? ""

    const short = deepLinks(page, "one small claim")
    expect(short.every((link) => link.carriesContent)).toBe(true)
    for (const link of short) expect(payload(link)).toContain("one small claim")

    const long = deepLinks(page, "z".repeat(50_000))
    expect(long.every((link) => link.carriesContent)).toBe(false)
    // The fallback is a redirection rather than a truncation: the page is still named.
    for (const link of long) {
      expect(payload(link)).toContain(page.markdownUrl)
      expect(payload(link)).not.toContain("zzzz")
    }
  })

  it("refuses to ship a truncated prompt when even the reference form overflows", () => {
    const [first] = DEEP_LINK_TARGETS
    if (first === undefined) throw new Error("the target table is empty")
    expect(() => deepLink({ ...first, vendorLimit: 40 }, page, "body")).toThrow(/over the 40 ceiling/)
  })

  it("names the page and this corpus in every prompt, so a prefill is never contextless", () => {
    const prompt = referencePrompt(page)
    expect(prompt).toContain(page.title)
    expect(prompt).toContain(page.markdownUrl)
    expect(prompt).toContain(page.pageUrl)
    // An unreplaced placeholder would be visible in the first prompt anybody opens.
    expect(prompt).toContain(DOCS_NAME)
    expect(DOCS_NAME).not.toBe("PROJECT")
  })
})

/* =================================================================================================
 * Every shipped href, read from dist.
 * ================================================================================================= */

describe("every shipped href in the build", () => {
  const targetPrefixes = DEEP_LINK_TARGETS.map(
    (target) => `${target.endpoint}${target.endpoint.includes("?") ? "&" : "?"}${target.parameter}=`
  )

  const shipped = (): ReadonlyArray<{ page: string; href: string }> =>
    builtPages().flatMap((path) =>
      hrefs(pageHtml(path))
        .filter((href) => targetPrefixes.some((prefix) => href.startsWith(prefix)))
        .map((href) => ({ page: path, href }))
    )

  it("gives every built page one control per target", () => {
    const pages = builtPages()
    // `toBeGreaterThan(0)` is what stops `0 === 0` from passing on an empty `dist/`.
    expect(pages.length).toBeGreaterThan(0)
    // Derived on both sides: neither the page count nor the target count is written down here.
    expect(shipped()).toHaveLength(pages.length * DEEP_LINK_TARGETS.length)
  })

  it("keeps every shipped href inside the budget", () => {
    for (const { page, href } of shipped()) {
      expect(href.length, `${page} exceeds the budget`).toBeLessThanOrEqual(DEEP_LINK_BUDGET)
    }
  })

  it("carries a payload behind every shipped href", () => {
    for (const { page, href } of shipped()) {
      const prefix = targetPrefixes.find((candidate) => href.startsWith(candidate)) ?? ""
      expect(href.length, `${page} has a control with an empty payload`).toBeGreaterThan(
        prefix.length
      )
    }
  })

  it("emits no protocol-relative href anywhere", () => {
    for (const path of builtPages()) {
      for (const href of hrefs(pageHtml(path))) {
        expect(href.startsWith("//"), `${path} emits a protocol-relative href`).toBe(false)
      }
    }
  })
})

/* =================================================================================================
 * The head discovery block.
 * ================================================================================================= */

describe("the head discovery block", () => {
  it("points at this page's raw route, and at both machine surfaces", () => {
    const links = discoveryLinks("architecture/module-map", CONTEXT)
    expect(links.map((link) => link.rel)).toEqual([...CONFIG.discoveryRels])
    for (const link of links) {
      expect(link.type).toBe("text/markdown")
      expect(new URL(link.href).origin).toBe(CONTEXT.site.origin)
      expect(link.href.startsWith(PROBE_PREFIX)).toBe(true)
    }
  })

  it("declares which relations are conventions and which are ours", () => {
    const links = discoveryLinks(CONFIG.agentPage, CONTEXT)
    const invented = links.filter((link) => link.warrant === "invention")
    expect(invented.map((link) => link.rel)).toEqual([...CONFIG.inventedRels])
  })

  it("adds no relation that has no adopters", () => {
    const rels = discoveryLinks(CONFIG.agentPage, CONTEXT).map((link) => link.rel)
    expect(rels).not.toContain("describedby")
  })

  it("is present on every built page, with a target that resolves on disk", () => {
    for (const path of builtPages()) {
      const document = pageHtml(path)
      for (const rel of CONFIG.discoveryRels) {
        const match = new RegExp(`<link rel="${rel}"[^>]*href="([^"]+)"`).exec(document)
        expect(match, `${path} has no rel="${rel}"`).not.toBeNull()
        const href = match?.[1] ?? ""
        expect(href.startsWith("//")).toBe(false)
        const target = served(new URL(href).pathname)
        expect(existsSync(join(dist, target)), `${path}: ${href} resolves to nothing`).toBe(true)
      }
    }
  })
})

/* =================================================================================================
 * The raw-Markdown route.
 * ================================================================================================= */

describe("the raw-Markdown route", () => {
  it("maps the site root to the route the raw-twin plugin actually injects", () => {
    // The root entry's id is the empty string or `index`, and both map to `<base>/.md` — a dotfile,
    // which is why a census built on a shell glob under-reports by exactly the landing page.
    expect(rawMarkdownUrl("", CONTEXT).pathname).toBe(`${CONFIG.probeBase}/.md`)
    expect(rawMarkdownUrl("index", CONTEXT).pathname).toBe(`${CONFIG.probeBase}/.md`)
    expect(rawMarkdownUrl("reference/cli", CONTEXT).pathname).toBe(
      `${CONFIG.probeBase}/reference/cli.md`
    )
  })

  it("keeps the base segment out of the origin, however the base is written", () => {
    for (const base of [CONFIG.probeBase, `${CONFIG.probeBase}/`]) {
      const url = siteUrl("llms.txt", { site: CONTEXT.site, base })
      expect(url.href).toBe(`${PROBE_PREFIX}llms.txt`)
      // The bug this forbids: a URL naming a HOST, from a base joined by concatenation.
      expect(url.href.startsWith("//")).toBe(false)
    }
  })

  it("has a twin on disk for every built page", () => {
    for (const path of builtPages()) {
      const id = served(path).replace(/\/$/, "")
      // The twin's location is derived through the module under test, not restated here.
      const twin = served(rawMarkdownUrl(id, BUILT_CONTEXT).pathname)
      expect(existsSync(join(dist, twin)), `${path} has no \`.md\` twin at ${twin}`).toBe(true)
    }
  })

  it("emits no twin for something that is not a page, beyond the injected 404 route", () => {
    /*
     * The other direction of the parity check. Without it a stale twin left by a removed page keeps
     * being served, and `llms.txt` keeps indexing it, while every probe above passes.
     */
    const pages = new Set(
      builtPages().map((path) => {
        const id = served(path).replace(/\/$/, "")
        return served(rawMarkdownUrl(id, BUILT_CONTEXT).pathname)
      })
    )
    const orphans = builtTwins().filter(
      (twin) => !pages.has(twin) && !CONFIG.twinsWithoutPages.includes(twin as never)
    )
    expect(orphans).toEqual([])
  })
})

/* =================================================================================================
 * Silent render failures.
 * ================================================================================================= */

describe("no page rendered empty", () => {
  /**
   * Starlight's docs loader reports a page whose Markdown threw as a WARNING and serves the page with an
   * empty body. That is the worst available outcome: the build is green, the route exists, the twin is
   * complete because it comes from the source, and the rendered page is blank. It happened on this
   * corpus — a citation plugin threw on a reference into gitignored build scratch — and the only thing
   * that noticed was the link validator, by accident, because a page with no recorded headings makes
   * every link INTO it report invalid.
   *
   * The twin is the reference for what the page should contain, because it is produced from the source
   * and cannot be empty when the source is not.
   */
  it("renders a heading in the HTML for every page whose source has one", () => {
    const empty: Array<string> = []
    for (const path of builtPages()) {
      const id = served(path).replace(/\/$/, "")
      const twin = readDist(served(rawMarkdownUrl(id, BUILT_CONTEXT).pathname))
      if (!/^##\s/m.test(twin)) continue
      if (!pageHtml(path).includes("<h2")) empty.push(path)
    }
    expect(empty).toEqual([])
  })

  it("is not vacuous: some page's source carries a heading", () => {
    const withHeadings = builtPages().filter((path) => {
      const id = served(path).replace(/\/$/, "")
      return /^##\s/m.test(readDist(served(rawMarkdownUrl(id, BUILT_CONTEXT).pathname)))
    })
    expect(withHeadings.length).toBeGreaterThan(0)
  })
})

/* =================================================================================================
 * Diagrams reach both surfaces.
 * ================================================================================================= */

describe("every mermaid fence renders at build time and survives into the twin", () => {
  /**
   * Two surfaces, two mechanisms, so two assertions. The rendered page gets an inline SVG because the
   * plugin claims the fence at mdast; the twin keeps the fence verbatim because it is built from the
   * entry's source and never enters that pipeline. Both are correct, and asserting only one lets a
   * client-rendered diagram — absent from every agent surface — pass.
   */
  const fences = (twin: string): number => (twin.match(/^```mermaid/gm) ?? []).length

  it("emits one figure per fence, counted off the build rather than written down", () => {
    let figures = 0
    let sources = 0
    for (const path of builtPages()) {
      const id = served(path).replace(/\/$/, "")
      const twin = readDist(served(rawMarkdownUrl(id, BUILT_CONTEXT).pathname))
      const found = pageHtml(path).split(`class="docs-mermaid"`).length - 1
      expect(found, `${path}: ${found} figures for ${fences(twin)} fences`).toBe(fences(twin))
      figures += found
      sources += fences(twin)
    }
    expect(sources).toBeGreaterThan(0)
    expect(figures).toBe(sources)
  })

  it("ships no mermaid runtime to the browser", () => {
    /*
     * The positive claim above is satisfied by a client-side renderer that injects the figure at
     * runtime, which would put the diagram on exactly the surface that already has the prose. This is
     * the half that forbids it.
     */
    for (const path of builtPages()) {
      expect(pageHtml(path).toLowerCase()).not.toContain("mermaid.min.js")
      expect(pageHtml(path)).not.toContain("mermaid.initialize")
    }
  })
})

/* =================================================================================================
 * The agent note reaches all three surfaces, not one.
 * ================================================================================================= */

describe("the agent note survives into every surface", () => {
  /** Every page carrying a note, found rather than listed. */
  const carriers = (): ReadonlyArray<string> => {
    const walk = (directory: string, prefix: string): ReadonlyArray<string> =>
      readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
        if (entry.isDirectory()) return walk(join(directory, entry.name), `${prefix}${entry.name}/`)
        if (!/\.mdx?$/.test(entry.name)) return []
        const body = readFileSync(join(directory, entry.name), "utf8")
        if (!body.includes(":::agent")) return []
        return [`${prefix}${entry.name.replace(/\.mdx?$/, "")}`]
      })
    return walk(content, "")
  }

  it("is used on the agent page and where behavior genuinely differs", () => {
    const pages = carriers()
    expect(pages).toContain(CONFIG.agentPage)
    expect(pages.length).toBeGreaterThanOrEqual(CONFIG.minAgentNotePages)
  })

  it("opens every block with the label, since only visible text survives the bundle", () => {
    for (const page of carriers()) {
      const source = contentSource(page)
      const blocks = source.split(":::agent").slice(1)
      expect(blocks.length).toBeGreaterThan(0)
      for (const block of blocks) {
        expect(block.trimStart().startsWith(`**${AGENT_NOTE_LABEL}.**`), page).toBe(true)
      }
      // A directive label would be escaped into `:::agent\[…]` on the raw route.
      expect(source).not.toMatch(/:::agent\[/)
    }
  })

  it("renders as a marked block in the HTML, not as a bare div", () => {
    for (const page of carriers()) {
      const document = readDist(join(page === "index" ? "" : page, "index.html"))
      const notes = document.split(`class="${AGENT_NOTE_CLASS}"`).length - 1
      expect(notes, `${page} lost its note in the HTML`).toBeGreaterThan(0)
      expect(document).toContain(AGENT_NOTE_LABEL)
    }
  })

  it("passes verbatim into the page's `.md` twin, directive and label both", () => {
    for (const page of carriers()) {
      const twin = readDist(`${page === "index" ? "" : page}.md`)
      expect(twin, `${page}.md lost the directive`).toContain(":::agent")
      expect(twin, `${page}.md lost the label`).toContain(`**${AGENT_NOTE_LABEL}.**`)
      expect(twin).not.toContain(":::agent\\[")
    }
  })

  it("reaches `llms-full.txt` with the label intact, once per authored note", () => {
    const bundle = readDist("llms-full.txt")
    const expected = carriers().reduce(
      (total, page) => total + contentSource(page).split(":::agent").length - 1,
      0
    )
    expect(expected).toBeGreaterThan(0)
    expect(bundle.split(`**${AGENT_NOTE_LABEL}.**`).length - 1).toBe(expected)
  })
})

/* =================================================================================================
 * The cloaking probe. The one check here that asserts an absence.
 * ================================================================================================= */

describe("no agent-addressed content is hidden from human readers", () => {
  /**
   * Content served to a machine and hidden from every human is cloaking wearing an accessibility class
   * name. The rule is easy to state in prose and easy to violate by accident — a note that looks noisy
   * gets an `.sr-only` and the diff reads like a styling change — so it is asserted instead. A principle
   * in prose is an opinion; a principle in a test is a rule.
   *
   * Scoped to the agent vocabulary rather than banning the class: Starlight labels every heading anchor
   * with a 1x1 `.sr-only` span whose whole purpose is naming the anchor for assistive technology, and a
   * blanket ban fails on the framework's own correct output and gets deleted instead of fixed.
   */
  const hidingSignals = [
    ...CONFIG.hidingClasses,
    'aria-hidden="true"',
    "hidden=",
    "display:none",
    "display: none"
  ]

  it("opens no note inside an element that removes it from sight", () => {
    /*
     * Every note opens with the label as its first visible text, so the element enclosing it opens
     * within a short window before the label. The window is bounded rather than parsed because a regex
     * cannot track nesting; a hiding wrapper further out is caught by the stylesheet case below, which
     * needs no nesting at all.
     */
    for (const path of builtPages()) {
      const document = pageHtml(path)
      let at = document.indexOf(AGENT_NOTE_LABEL)
      while (at !== -1) {
        const window = document.slice(Math.max(0, at - 400), at)
        for (const signal of hidingSignals) {
          expect(window.includes(signal), `${path}: a note is hidden by \`${signal}\``).toBe(false)
        }
        at = document.indexOf(AGENT_NOTE_LABEL, at + 1)
      }
    }
  })

  it("never combines the note class with a hiding class", () => {
    for (const path of builtPages()) {
      for (const attribute of pageHtml(path).matchAll(/class="([^"]*)"/g)) {
        const classes = (attribute[1] ?? "").split(/\s+/)
        if (!classes.includes(AGENT_NOTE_CLASS)) continue
        for (const hiding of CONFIG.hidingClasses) {
          expect(
            classes.some((name) => name.includes(hiding)),
            `${path}: ${attribute[1]}`
          ).toBe(false)
        }
      }
    }
  })

  it("ships no stylesheet rule that hides the note class", () => {
    /*
     * The teeth of this probe. A block visible in the HTML and `display:none` in the CSS is the same
     * cloak with the evidence moved one file over, and it is the form that survives a review of the
     * markup. Declaration blocks are read individually, so a rule nested in an at-rule is still checked.
     */
    const sheets = builtStylesheets()
    expect(sheets.length, "no stylesheet was emitted, so this case is vacuous").toBeGreaterThan(0)
    for (const sheet of sheets) {
      const css = readFileSync(sheet, "utf8")
      for (const block of css.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
        const selector = block[1] ?? ""
        const body = block[2] ?? ""
        if (!selector.includes(`.${AGENT_NOTE_CLASS}`)) continue
        for (const declaration of CONFIG.hidingDeclarations) {
          expect(
            declaration.test(body),
            `${sheet}: \`${selector.trim()}\` hides the note with \`${body.trim()}\``
          ).toBe(false)
        }
      }
    }
  })

  it("fails on a poisoned stylesheet, so a clean sheet and a broken probe are not the same green", () => {
    const poisoned = `@media screen { .${AGENT_NOTE_CLASS} { display: none } }`
    const offenders = [...poisoned.matchAll(/([^{}]+)\{([^{}]*)\}/g)].filter(
      (block) =>
        (block[1] ?? "").includes(`.${AGENT_NOTE_CLASS}`) &&
        CONFIG.hidingDeclarations.some((declaration) => declaration.test(block[2] ?? ""))
    )
    expect(offenders.length).toBeGreaterThan(0)
  })
})

/* =================================================================================================
 * The agent page's own entry points.
 * ================================================================================================= */

describe("the agent page's own entry points", () => {
  /** What a writer typed, which is the subject of the authorship probes. */
  const authoredSource = (): string => readFileSync(join(authored, `${CONFIG.agentPage}.md`), "utf8")

  it("is the first entry `llms.txt` lists", () => {
    const listed = [...readDist("llms.txt").matchAll(/^- \[[^\]]+\]\(([^)]+)\)/gm)].map(
      (match) => match[1] ?? ""
    )
    expect(listed.length).toBeGreaterThan(0)
    expect(listed[0]).toBe(`${CONFIG.origin}${segment}${CONFIG.agentPage}.md`)
  })

  it("is reachable from the site navigation on every page", () => {
    for (const path of builtPages()) {
      expect(hrefs(pageHtml(path)), `${path} cannot reach the agent page`).toContain(
        `${segment}${CONFIG.agentPage}/`
      )
    }
  })

  it("resolves every internal link it carries to a real file in the build", () => {
    /*
     * Stricter than the link validator, and for a reason the validator cannot get around: it can only
     * judge a target whose headings it recorded in its own Markdown pass, so every link to an injected
     * `.md` route is excluded from it by configuration. This resolves against the bytes on disk instead,
     * which needs no side table.
     *
     * Scoped to the body: a link a reader or an agent can follow. The `<head>` carries the
     * machine-surface relations, covered above on their own terms.
     */
    const document = readDist(join(CONFIG.agentPage, "index.html"))
    const body = document.slice(document.indexOf("<body"))
    const internal = hrefs(body).filter(
      (href) => href.startsWith(segment) && !href.startsWith("//")
    )
    expect(internal.length).toBeGreaterThan(10)
    for (const href of internal) {
      const target = served(href.split("#")[0] ?? "")
      const candidates = [target, join(target, "index.html")]
      expect(
        candidates.some((candidate) => existsSync(join(dist, candidate))),
        `${href} resolves to nothing in the build`
      ).toBe(true)
    }
  })

  it("carries a read-next row for every built page", () => {
    /*
     * The table is generated from the sync manifest, so this asserts the generator and the build agree
     * rather than asserting a number. A page added to `docs/` that never reaches the table is a page an
     * agent reading only this file never learns about.
     */
    const twin = readDist(`${CONFIG.agentPage}.md`)
    const missing = builtPages()
      .map((path) => served(path).replace(/\/$/, ""))
      .filter((id) => id !== CONFIG.agentPage && id !== "")
      .filter((id) => !twin.includes(`(${segment}${id}/)`))
    expect(missing).toEqual([])
  })

  it("states no count, because a hand-written count is a lie told to the reader who trusts it", () => {
    const counted = new RegExp(`\\b\\d+\\s+(?:${CONFIG.countedNouns.join("|")})\\b`, "i")
    expect(authoredSource()).not.toMatch(counted)
  })

  it("writes no brace anchor, which would break its own raw route", () => {
    expect(authoredSource()).not.toMatch(/\{\s*#[a-z0-9-]+\s*\}/)
  })

  it("numbers its sections from one, contiguously, in the heading text", () => {
    /*
     * The number lives in the heading TEXT so every surface agrees on it: the rendered page, the table
     * of contents, the search index, `llms.txt`, and the raw Markdown. The alternative — an explicit
     * `{ #anchor }` — carries a brace into the raw route, where the MDX parser reads it as an expression
     * and one occurrence fails the whole route.
     */
    const headings = authoredSource()
      .split("\n")
      .filter((line) => line.startsWith("## "))
    expect(headings.length).toBeGreaterThan(1)
    expect(headings.map((heading) => heading.split(" ")[1])).toEqual(
      headings.map((_, at) => `${at + 1}.`)
    )
  })
})
