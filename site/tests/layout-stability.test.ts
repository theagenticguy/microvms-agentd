// SPDX-License-Identifier: Apache-2.0
import { join } from "node:path"
import { fileURLToPath } from "node:url"

import { type Browser, chromium } from "playwright"
import { afterAll, beforeAll, describe, expect, it } from "vitest"

import {
  AUDITED_PAGES,
  BASE,
  DIST_DIR,
  KNOWN_LAYOUT_SHIFTS,
  LAYOUT_SHIFT_CEILING
} from "../src/gates.js"
import { type StaticSite, serveStatic } from "./static-server.js"

/**
 * Layout stability, measured where the viewport cannot change under the measurement. Ported from
 * memhtml-public's `apps/docs/tests/layout-stability.test.ts`.
 *
 * ## Why this exists beside the Lighthouse budget rather than inside it
 *
 * Cumulative Layout Shift is the one metric in the performance category that Lighthouse cannot
 * measure reliably on a loaded machine, because it competes with Lighthouse's OWN viewport
 * emulation. memhtml-public measured it 2026-08-14 on a Starlight page: three runs of one unchanged
 * page, identical `screenEmulation` (1350x940) and host `benchmarkIndex`, scored `1, 1, 0.81`, the
 * odd one out carrying `CLS 0.427` with `TBT 0 ms` and `LCP 324 ms`, so every metric that describes
 * the page was perfect. The shift it recorded was attributed to a container with a bounding box
 * 1335px wide ending at x=2370, which does not fit inside the 1350px viewport it claims to have been
 * measured in: the geometry belongs to the pre-emulation window, and the resize to the emulated
 * viewport was counted as a shift.
 *
 * So the composite score is asserted `optimistic` in `lighthouserc.json` (contention can only depress
 * a static page's score, never inflate it) and layout stability is asserted HERE instead, with the
 * viewport fixed before the first navigation so no emulation can race the paint. Same remedy the
 * `scrollable-region-focusable` flake got in `tests/a11y.test.ts`, and for the same reason: a
 * blocking gate cannot hold a measurement that flips on an unchanged page.
 *
 * The ceiling is not a suppression. `LAYOUT_SHIFT_CEILING` is the Core Web Vitals "good" threshold,
 * and every page here measures 0 today, so a real shift (an image without dimensions, a late
 * stylesheet, a web font that changes metrics) fails this gate on the first run rather than on the
 * one run in three where Lighthouse happens to notice.
 */

const dist = join(fileURLToPath(new URL("..", import.meta.url)), DIST_DIR)

/** The viewport Lighthouse's desktop preset emulates, set BEFORE any navigation. */
const VIEWPORT = { width: 1350, height: 940 }

/**
 * How long the page is watched after it goes quiet.
 *
 * A shift that arrives with a late resource is the defect this gate is for, so the observer has to
 * outlive `networkidle`; a probe that stopped there would measure only the shifts that beat the
 * network.
 */
const SETTLE_MS = 1_500

/** One recorded shift: how much it moved the page, when, and which nodes moved. */
type Shift = {
  readonly value: number
  readonly at: number
  readonly sources: ReadonlyArray<{
    readonly node: string
    readonly from: string
    readonly to: string
  }>
}

let site: StaticSite
let browser: Browser
const shifts = new Map<string, ReadonlyArray<Shift>>()

/** Every move that made up a page's total, so the failure message is the diagnosis. */
const report = (entries: ReadonlyArray<Shift>): string =>
  entries
    .map(
      (entry) =>
        `${entry.value.toFixed(4)} at ${entry.at}ms: ` +
        (entry.sources.map((s) => `${s.node} ${s.from} -> ${s.to}`).join("; ") || "no source node")
    )
    .join("\n      ")

beforeAll(async () => {
  site = await serveStatic(dist, BASE)
  browser = await chromium.launch()
  const context = await browser.newContext({ viewport: VIEWPORT })
  for (const path of AUDITED_PAGES) {
    const page = await context.newPage()
    /**
     * The observer is installed as an init script, so it is running before the document's first
     * byte. Registering it after `goto` would miss every shift that happened during load, which is
     * all of the ones worth catching.
     *
     * `hadRecentInput` entries are dropped for the reason the metric drops them: a shift the user
     * asked for by clicking is not a layout defect. Nothing here clicks, so this only guards
     * against a future case that does.
     */
    await page.addInitScript(() => {
      const w = window as unknown as { __shifts: Array<unknown> }
      w.__shifts = []
      new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) {
          const shift = entry as PerformanceEntry & {
            value: number
            hadRecentInput: boolean
            sources?: ReadonlyArray<{
              node?: Element | null
              previousRect?: DOMRectReadOnly
              currentRect?: DOMRectReadOnly
            }>
          }
          if (shift.hadRecentInput) continue
          const box = (rect?: DOMRectReadOnly): string =>
            rect === undefined
              ? ""
              : `${Math.round(rect.x)},${Math.round(rect.y)} ${Math.round(rect.width)}x${Math.round(rect.height)}`
          // The node's identity, not its text: a tag plus its first classes is what a fix is written
          // against, where a snippet of prose would make the failure message unreadable.
          w.__shifts.push({
            value: shift.value,
            at: Math.round(shift.startTime),
            sources: (shift.sources ?? []).map((source) => {
              const node = source.node
              const classes = node?.className ? String(node.className).trim().split(/\s+/) : []
              return {
                node: node
                  ? `${node.tagName.toLowerCase()}.${classes.slice(0, 3).join(".")}`
                  : "detached",
                from: box(source.previousRect),
                to: box(source.currentRect)
              }
            })
          })
        }
      }).observe({ type: "layout-shift", buffered: true })
    })
    const response = await page.goto(`${site.origin}${path}`, { waitUntil: "networkidle" })
    // Same rule as the a11y tier: a 404 has a layout too, and a stable 404 is not a stable page.
    expect(response?.status(), `${path} is not a page of the built site`).toBe(200)
    await page.evaluate(() => document.fonts.ready)
    await page.waitForTimeout(SETTLE_MS)
    shifts.set(
      path,
      (await page.evaluate(
        () => (window as unknown as { __shifts: Array<unknown> }).__shifts
      )) as ReadonlyArray<Shift>
    )
    await page.close()
  }
  await context.close()
}, 300_000)

afterAll(async () => {
  await browser?.close()
  await site?.close()
})

describe("no audited page shifts its layout while it loads", () => {
  it.each([...AUDITED_PAGES])("holds %s under the Core Web Vitals ceiling", (path) => {
    const entries = shifts.get(path)
    expect(entries, `${path} was never measured`).toBeDefined()
    if (entries === undefined) return

    /**
     * A shift is claimed only if EVERY node it moved is declared and it stays inside that entry's
     * bound. One shift can carry several source nodes, so a single declared node does not excuse the
     * others travelling with it.
     */
    const claimed = (shift: Shift): boolean =>
      shift.sources.length > 0 &&
      shift.sources.every((source) =>
        KNOWN_LAYOUT_SHIFTS.some(
          (known) => source.node.startsWith(known.node) && shift.value <= known.most
        )
      )

    const unclaimed = entries.filter((entry) => !claimed(entry))
    const total = unclaimed.reduce((sum, entry) => sum + entry.value, 0)
    expect(
      total,
      `${path} shifted ${total.toFixed(4)} outside the declared baseline, across ` +
        `${unclaimed.length} of ${entries.length} shift(s):\n      ${report(unclaimed)}`
    ).toBeLessThan(LAYOUT_SHIFT_CEILING)
  })

  /**
   * Every page measured, and the probe proved capable of seeing a shift at all.
   *
   * A `PerformanceObserver` that silently failed to register (a renamed entry type, a browser
   * without the API) would report 0 for every page and pass forever. So the observer is exercised
   * against a page that shifts on purpose, and its verdict is asserted to exceed the ceiling the
   * real pages sit under.
   */
  it("registers a shift when one happens, so a zero means stability", async () => {
    const context = await browser.newContext({ viewport: VIEWPORT })
    const page = await context.newPage()
    /**
     * The observer ships INSIDE the fixture here, not through `addInitScript`.
     *
     * `setContent` lands on `about:blank`, which runs no init script, so a probe written the way the
     * page loop above is written would read `undefined` and this case would fail for a reason that
     * says nothing about layout shift. The loop's own use of `addInitScript` is proven by the page
     * cases above: an init script that never ran leaves `__shifts` undefined, and the `entries`
     * assertion fails on undefined rather than passing.
     *
     * The fixture is the shape of the defect this gate exists to catch: an element that gains height
     * after paint and pushes the content below it down.
     */
    await page.setContent(
      `<body style="margin:0">
         <script>
           window.__shift = 0
           new PerformanceObserver((list) => {
             for (const entry of list.getEntries()) window.__shift += entry.value
           }).observe({ type: "layout-shift", buffered: true })
         </script>
         <div id="pusher"></div>
         <p style="height:600px;background:#eee">content that gets pushed</p>
         <script>
           requestAnimationFrame(() => {
             setTimeout(() => {
               document.getElementById("pusher").style.height = "400px"
             }, 100)
           })
         </script>
       </body>`
    )
    await page.waitForTimeout(600)
    const measured = await page.evaluate(() => (window as unknown as { __shift: number }).__shift)
    await context.close()
    expect(measured).toBeGreaterThan(LAYOUT_SHIFT_CEILING)
  }, 60_000)
})
