// SPDX-License-Identifier: Apache-2.0
import { join } from "node:path"
import { fileURLToPath } from "node:url"

import AxeBuilder from "@axe-core/playwright"
import { type Browser, type BrowserContext, chromium } from "playwright"
import { afterAll, beforeAll, describe, expect, it } from "vitest"

import { AUDITED_PAGES, BASE, DIST_DIR, KNOWN_A11Y_FAILURES } from "../src/gates.js"
import { type StaticSite, serveStatic } from "./static-server.js"

/**
 * The accessibility gate: axe-core over five representative pages, plus four probes for things axe
 * does not do or does not do reliably. Ported from memhtml-public's `apps/docs/tests/a11y.test.ts`.
 *
 * Five pages rather than the whole corpus, and that bound is the reason this gate will still be here
 * in a year: a check whose cost grows with the corpus gets deleted the first time it makes someone
 * wait. `src/gates.ts` says why these five, and `tests/built-site.test.ts` proves the sample is not
 * silently narrower than the site.
 *
 * WCAG 2.2 AA is asserted through axe's own tag set. `best-practice` is deliberately absent: it is
 * Deque's advice rather than a conformance requirement, and a gate that cannot distinguish the two
 * teaches its readers that AA failures are matters of taste.
 *
 * Every page is visited ONCE, in `beforeAll`, and each case reads the findings collected there. Two
 * reasons: five page loads instead of twenty-five, and the spacing probe has to restyle the page,
 * which no case that ran after it could be trusted to un-see.
 *
 * A page that is not in the build fails here by name, with its HTTP status, rather than auditing the
 * 404 page in its place. Nothing is skipped.
 */

/**
 * Every rule tagged for WCAG 2.0/2.1/2.2 at levels A and AA, with two named adjustments.
 *
 * `label-content-name-mismatch` carries `wcag21a` and is still skipped by a tag selection, because
 * axe ships it as `experimental` and an experimental rule is disabled in its own definition. SC 2.5.3
 * Label in Name is a Level A criterion, so the rule is switched on explicitly rather than left to a
 * default that would hide the finding.
 *
 * `scrollable-region-focusable` is switched OFF, and not because the defect it reports is acceptable.
 * memhtml-public measured it 2026-08-12 against a Starlight build: five axe runs over one unchanged
 * page, identical layout each time, and the rule reported a violation on one run of the five. A
 * blocking gate cannot hold a check that flips at 20 percent. The criterion is covered instead by
 * `scrollableRegions` below, which reads the same geometry directly and is deterministic, and which
 * reports under the same rule id so a baseline entry governs both.
 */
const AXE_OPTIONS = {
  runOnly: {
    type: "tag" as const,
    values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"]
  },
  rules: {
    "label-content-name-mismatch": { enabled: true },
    "scrollable-region-focusable": { enabled: false }
  }
}

/** A single accessibility finding, from axe or from a probe here, in one shape. */
type Finding = {
  readonly rule: string
  /** What a baseline `signature` is matched against: the node's markup, then the explanation. */
  readonly evidence: string
  /** One line, for a failure message a reader can act on. */
  readonly report: string
}

const dist = join(fileURLToPath(new URL("..", import.meta.url)), DIST_DIR)

/*
 * 1280x800 is where the sticky header, the sidebar and the table of contents are all mounted at
 * once, which is the layout that can obscure a focused element. The narrow layout collapses the
 * sidebar into a dialog and has strictly fewer overlapping surfaces.
 */
const VIEWPORT = { width: 1280, height: 800 }

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select, textarea, summary, [tabindex]:not([tabindex^="-"])'

let site: StaticSite
let browser: Browser
let context: BrowserContext
const findings = new Map<string, ReadonlyArray<Finding>>()

/**
 * SC 2.1.1 Keyboard, for the scrollable regions a code block and a wide table create.
 *
 * A region that scrolls with a pointer and cannot be reached with a keyboard hides the part of itself
 * that is off-screen. A region is reachable when it is focusable itself or contains something
 * focusable, which is what axe's own rule checks. This reads the geometry rather than trusting the
 * rule, because the rule was measured to be unreliable and the geometry was measured to be stable.
 */
const scrollableRegions = (focusable: string): Finding[] => {
  const found: Finding[] = []
  for (const element of document.querySelectorAll(".sl-markdown-content, .sl-markdown-content *")) {
    const style = getComputedStyle(element)
    const scrolls = (overflow: string, over: boolean) =>
      (overflow === "auto" || overflow === "scroll") && over
    const wide = scrolls(style.overflowX, element.scrollWidth > element.clientWidth + 1)
    const tall = scrolls(style.overflowY, element.scrollHeight > element.clientHeight + 1)
    if (!wide && !tall) continue
    if (element.matches('[tabindex]:not([tabindex^="-"])')) continue
    if (element.querySelector(focusable) !== null) continue
    const by = wide
      ? `${element.scrollWidth - element.clientWidth}px horizontally`
      : `${element.scrollHeight - element.clientHeight}px vertically`
    found.push({
      rule: "scrollable-region-focusable",
      evidence: `${element.outerHTML.slice(0, 140)}\nscrolls ${by} and takes no focus`,
      report: `<${element.tagName.toLowerCase()}> scrolls ${by} and takes no focus`
    })
  }
  return found
}

/**
 * SC 2.4.11 Focus Not Obscured (Minimum), new in WCAG 2.2 and the criterion this layout is most
 * exposed to: it has a sticky header, and a focused anchor scrolled to just beneath it is conforming
 * to look at and non-conforming to use. axe ships no rule for it, because the criterion needs a
 * focused element and a hit test, which only a browser can supply.
 */
const obscuredFocus = (focusable: string): Finding[] => {
  const found: Finding[] = []
  // Bounded at 80: enough to reach the skip link, the whole header, the sidebar and the first screens
  // of body links, and short of turning a gate into a crawl of every anchor on a 6000px page.
  for (const element of [...document.querySelectorAll<HTMLElement>(focusable)].slice(0, 80)) {
    element.focus()
    const box = element.getBoundingClientRect()
    if (box.width < 1 || box.height < 1) continue
    if (box.top < 0 || box.bottom > window.innerHeight) continue
    // The top edge is where a sticky header cuts in, so that is the point to test.
    const x = Math.min(box.left + Math.min(box.width / 2, 20), window.innerWidth - 1)
    const hit = document.elementFromPoint(x, box.top + 1)
    if (hit === null || element.contains(hit) || hit.contains(element)) continue
    const position = getComputedStyle(hit).position
    if (position !== "fixed" && position !== "sticky") continue
    const what = `${element.tagName.toLowerCase()} "${(element.textContent ?? "").trim().slice(0, 40)}"`
    const cover = `${hit.tagName.toLowerCase()}.${hit.className.toString().split(" ")[0]}`
    found.push({
      rule: "focus-not-obscured",
      evidence: `${element.outerHTML.slice(0, 140)}\ncovered by ${cover}`,
      report: `${what} is covered by ${position} ${cover} when focused`
    })
  }
  return found
}

/**
 * Inline SVG, which axe's `svg-img-alt` rule reaches only when the element already declares
 * `role="img"`. An inline `<svg>` with no role at all is invisible to it, and inline SVG with no role
 * is how Starlight draws every icon and how the build-time Mermaid plugin draws every diagram.
 *
 * A real DOM is required because the answer usually lives on an ancestor. Three ways to be decided:
 * `aria-hidden` on the element or above it (Starlight wraps the heading-anchor icon in
 * `<span aria-hidden="true">`); an `aria-label`/`aria-labelledby` ancestor, which supplies the
 * accessible name so the SVG's own content is never consulted (the scroll-to-top button); or
 * `role="img"` with a name of its own that is not a placeholder.
 */
const undecidedSvg = (): Finding[] => {
  const PLACEHOLDER =
    /^(image|img|picture|photo|screenshot|diagram|figure|chart|graph|illustration|graphic|icon)s?$/i
  const found: Finding[] = []
  for (const svg of document.querySelectorAll("svg")) {
    if (svg.closest('[aria-hidden="true"]') !== null) continue
    if (svg.closest("[aria-label], [aria-labelledby]") !== null) continue
    const name =
      svg.getAttribute("aria-label") ?? svg.querySelector("title")?.textContent?.trim() ?? ""
    const why =
      svg.getAttribute("role") !== "img"
        ? "carries no role and is not hidden, so assistive technology meets bare shapes"
        : name === ""
          ? 'declares role="img" and has no accessible name'
          : PLACEHOLDER.test(name)
            ? `is named "${name}", which describes nothing`
            : ""
    if (why === "") continue
    found.push({
      rule: "inline-svg-undecided",
      evidence: `${svg.outerHTML.slice(0, 140)}\n${why}`,
      report: `an inline <svg> ${why}`
    })
  }
  return found
}

/**
 * SC 1.4.12 Text Spacing, which axe does not test at all. It cannot: the criterion is about what
 * happens after the reader overrides the spacing, and axe reads the page as authored. The overrides
 * are the ones the criterion names verbatim.
 *
 * The assertion is on CLIPPING rather than on overflow, which is the difference between a real failure
 * and a false one: a code block scrolls horizontally by design (`overflow-x: auto`) and loses nothing,
 * while an element with `overflow: hidden` that grew past its box has thrown text away.
 *
 * Two further narrowings. The scope is the BLOCK containers the criterion is written about: a `<span>`
 * inside a code line clipping while its scrollable parent still holds the text is not a loss of
 * content. And a visually-hidden element is skipped: Starlight labels every heading anchor with a 1x1
 * `.sr-only` span whose whole purpose is to overflow a one-pixel box.
 */
const TEXT_SPACING_CSS = `.sl-markdown-content, .sl-markdown-content * {
  line-height: 1.5 !important;
  letter-spacing: 0.12em !important;
  word-spacing: 0.16em !important;
}
.sl-markdown-content p { margin-block-end: 2em !important; }`

const clippedText = (): Finding[] => {
  const BLOCKS = "p li dt dd td th blockquote figcaption h1 h2 h3 h4 h5 h6".split(" ")
  const found: Finding[] = []
  const selector = BLOCKS.map((tag) => `.sl-markdown-content ${tag}`).join(", ")
  for (const element of document.querySelectorAll(selector)) {
    const box = element.getBoundingClientRect()
    if (box.width <= 1 || box.height <= 1) continue
    const style = getComputedStyle(element)
    const hides = (value: string) => value === "hidden" || value === "clip"
    const over =
      hides(style.overflowY) && element.scrollHeight > element.clientHeight + 1
        ? `${element.scrollHeight - element.clientHeight}px of height`
        : hides(style.overflowX) && element.scrollWidth > element.clientWidth + 1
          ? `${element.scrollWidth - element.clientWidth}px of width`
          : ""
    if (over === "") continue
    const what = `${element.tagName.toLowerCase()} "${(element.textContent ?? "").trim().slice(0, 40)}"`
    found.push({
      rule: "text-spacing-clips",
      evidence: `${element.outerHTML.slice(0, 140)}\nclips ${over}`,
      report: `${what} clips ${over} under the WCAG spacing overrides`
    })
  }
  return found
}

beforeAll(async () => {
  site = await serveStatic(dist, BASE)
  try {
    browser = await chromium.launch()
  } catch (cause) {
    throw new Error(
      "could not launch Chromium. The browser is acquired by `mise run docs:browsers`, which is a " +
        "one-time download per machine; run it if this is a fresh clone.",
      { cause }
    )
  }
  // An EXPLICIT context, not `browser.newPage()`. axe finishes a run by opening a second, blank page
  // in the page's own context and reducing the per-frame partials there; against the implicit context
  // `browser.newPage()` creates that second page fails, and axe reports only its own advice to call
  // `browser.newContext()`.
  context = await browser.newContext({ viewport: VIEWPORT })

  for (const path of AUDITED_PAGES) {
    const page = await context.newPage()
    const response = await page.goto(`${site.origin}${path}`, { waitUntil: "load" })
    // A 404 renders as a page and would be audited as one, reporting a clean result for a page that
    // is not the page under test. The message names the path, because during a tiered build the
    // likeliest cause is a page another producer has not emitted yet.
    expect(
      response?.status(),
      `${path} is not a page of the built site (HTTP ${response?.status()}). Every AUDITED_PAGES ` +
        "entry in src/gates.ts must exist in dist/; nothing here is skipped."
    ).toBe(200)
    /*
     * Wait for the scripts to finish, not just for `load`. The scroll-to-top button and Starlight's
     * theme selector are appended by client JS, so auditing at `load` audits a DOM that is missing
     * elements the reader gets; memhtml-public measured the inline-SVG probe finding the button on
     * some runs and not others until this line existed.
     */
    await page.waitForLoadState("networkidle")

    const axe = await new AxeBuilder({ page }).options(AXE_OPTIONS).analyze()
    const collected: Finding[] = axe.violations.flatMap((violation) =>
      violation.nodes.map((node) => ({
        rule: violation.id,
        evidence: `${node.html}\n${node.failureSummary ?? ""}`,
        report:
          `${violation.id} (${violation.impact}) at ${node.target.join(" ")}: ` +
          `${(node.failureSummary ?? "").replace(/\s+/g, " ").slice(0, 200)}`
      }))
    )
    collected.push(...(await page.evaluate(scrollableRegions, FOCUSABLE)))
    collected.push(...(await page.evaluate(undecidedSvg)))
    // Moves focus, so it runs after everything that reads the page at rest.
    collected.push(...(await page.evaluate(obscuredFocus, FOCUSABLE)))
    // Restyles the page, so it runs last of all.
    await page.addStyleTag({ content: TEXT_SPACING_CSS })
    collected.push(...(await page.evaluate(clippedText)))

    findings.set(path, collected)
    await page.close()
  }
}, 300_000)

afterAll(async () => {
  await context?.close()
  await browser?.close()
  await site?.close()
})

/**
 * The rule ids this file reports under itself. `scrollable-region-focusable` is deliberately axe's
 * own id: the finding is the same one axe's rule describes, so the baseline entry reads the same
 * whichever side detects it.
 */
const PROBE_RULES = [
  "scrollable-region-focusable",
  "focus-not-obscured",
  "inline-svg-undecided",
  "text-spacing-clips"
]

const collectedFor = (path: string): ReadonlyArray<Finding> => {
  const collected = findings.get(path)
  if (collected === undefined) throw new Error(`nothing was collected for ${path}`)
  return collected
}

/** Findings under the given rules; passing no rule means every rule axe itself reported. */
const of = (path: string, rules?: ReadonlyArray<string>): ReadonlyArray<Finding> =>
  collectedFor(path).filter((finding) =>
    rules === undefined ? !PROBE_RULES.includes(finding.rule) : rules.includes(finding.rule)
  )

/** A finding is claimed when some baseline entry names its rule AND matches its evidence. */
const claimedBy = (finding: Finding) =>
  KNOWN_A11Y_FAILURES.find(
    (entry) => entry.rule === finding.rule && entry.signature.test(finding.evidence)
  )

/** Findings a declared baseline entry accounts for are set aside, node by node; the rest fail. */
const unexpected = (found: ReadonlyArray<Finding>): ReadonlyArray<string> =>
  found.filter((finding) => claimedBy(finding) === undefined).map((finding) => finding.report)

describe.each(AUDITED_PAGES)("%s", (path) => {
  it("has no WCAG 2.2 AA violation outside the declared baseline", () => {
    expect(unexpected(of(path))).toEqual([])
  })

  it("keeps every scrollable region reachable by keyboard (SC 2.1.1)", () => {
    expect(unexpected(of(path, ["scrollable-region-focusable"]))).toEqual([])
  })

  it("never hides a focused element behind the sticky header (SC 2.4.11)", () => {
    expect(unexpected(of(path, ["focus-not-obscured"]))).toEqual([])
  })

  it("loses no text when the reader overrides spacing (SC 1.4.12)", () => {
    expect(unexpected(of(path, ["text-spacing-clips"]))).toEqual([])
  })

  it("decides every inline SVG: hidden, named by an ancestor, or named itself", () => {
    expect(unexpected(of(path, ["inline-svg-undecided"]))).toEqual([])
  })
})

describe("the baseline is a ratchet", () => {
  /*
   * Every declared failure must still be reported somewhere. Without this a fix leaves its
   * suppression behind, and the next defect of the same shape lands inside a license nobody remembers
   * granting.
   */
  it("carries no entry that has already been fixed", () => {
    const everything = [...findings.values()].flat()
    const stale = KNOWN_A11Y_FAILURES.filter(
      (entry) => !everything.some((finding) => claimedBy(finding) === entry)
    )
    expect(
      stale.map((entry) => `${entry.rule} (${entry.owner})`),
      "these no longer fail; delete them from KNOWN_A11Y_FAILURES in src/gates.ts"
    ).toEqual([])
  })

  it("audits the declared pages and no others", () => {
    expect([...findings.keys()]).toEqual([...AUDITED_PAGES])
  })
})
