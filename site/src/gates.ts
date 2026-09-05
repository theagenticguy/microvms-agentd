// SPDX-License-Identifier: Apache-2.0
/**
 * What the built site is allowed to contain, and which pages the browser gates audit.
 *
 * Both lists are declared here and nowhere else. `tests/built-site.test.ts` reads them against
 * `dist/`, `tests/a11y.test.ts` and `tests/layout-stability.test.ts` drive a browser over them, and a
 * case in the first asserts that `lighthouserc.json` (JSON, so it cannot import this file) names the
 * same pages under the same base.
 *
 * Ported from memhtml-public's `apps/docs/src/gates.ts`; the lists are this repository's own.
 */

/**
 * Tokens that must never reach a public page.
 *
 * This repository is public. Its documentation tree is generated from the source by a pass that reads
 * everything a checkout holds, and its hand-written pages quote measured API envelopes verbatim, so the
 * failure mode is a private name surviving an edit that looked cosmetic: a session ledger, a task id
 * from a private tracker, a real account id inside a copied ARN.
 *
 * `pattern` is matched against the raw bytes of every built HTML page, raw `.md` twin and llms bundle.
 * Adding a term is one entry; `why` is printed with the failure so whoever hits it knows what leaked
 * rather than only that something did.
 */
export const DENYLIST: ReadonlyArray<{ readonly pattern: RegExp; readonly why: string }> = [
  {
    pattern: /\bT-AC-\d+(?:-\d+)*\b/i,
    why: "an internal task id, meaningless to a reader and a pointer into a private tracker"
  },
  {
    pattern: /\.sarif\b/i,
    why: "scanner output; the findings are private even when the scanner is not"
  },
  {
    pattern: /\b392583147479\b/,
    why:
      "an AWS account id. Not a secret on its own, and still a pointer at a real account that a " +
      "public page has no reason to name: redact it to 123456789012 in the quoted envelope"
  },
  {
    pattern: /\b741448939267\b/,
    why:
      "an AWS account id from an earlier smoke stack. Same rule as the other: redact it to " +
      "123456789012 wherever an ARN is quoted"
  },
  {
    pattern: /docs\/\.(?:packets|repomix)\b/i,
    why:
      "gitignored build scratch under docs/: a page citing it points a reader at a file the " +
      "repository does not hold"
  }
]

/**
 * Exactly one leading and one trailing slash, whatever the caller wrote.
 *
 * The same normalization `astro.config.ts` applies, restated here because that file cannot be imported
 * by a test without loading the whole Astro configuration. The deploy workflow derives the base from
 * the repository name and a user or organization Pages site derives `/`, so `${name}/` would produce
 * `//`, a base that reads as a protocol-relative URL naming a host.
 */
const normalizeBase = (value: string): string =>
  `/${value.replace(/^\/+|\/+$/g, "")}/`.replace(/^\/{2,}/, "/")

/**
 * The base the site is built under, with a guaranteed trailing slash: `/microvms-agentd/` here, `/`
 * on a user or organization Pages site. It is what `import.meta.env.BASE_URL` holds inside the build.
 *
 * Every asset URL in the output is prefixed with it, so anything serving `dist/` has to mount it here
 * rather than elsewhere. A site served one segment too high loads its HTML and none of its CSS, which
 * reads as a catastrophic regression in every gate at once rather than as a misconfigured harness.
 */
export const BASE = normalizeBase(process.env.DOCS_BASE ?? "/microvms-agentd/")

/**
 * `BASE` as a prefix: `/microvms-agentd` under a path, and the empty string at the root.
 *
 * Exported because every consumer that joins the base onto a root-relative path needs this form.
 * Joining or slicing with the trailing-slashed `BASE` is correct under `/microvms-agentd/` and wrong
 * at the root, where `${BASE}/learn/` is `//learn/`, a URL naming a host.
 */
export const BASE_SEGMENT = BASE.replace(/\/$/, "")

/**
 * The pages the browser gates audit, as site-absolute paths including the base.
 *
 * Five, on purpose. The corpus grows with every command the CLI gains and every page the generated tree
 * emits, so a gate that walked it would get slower for the rest of the project's life and be deleted
 * the first time it cost someone ten minutes. These five are the distinct templates; every other page
 * is one of them with different prose:
 *
 * - `/` is the cover page, the only one authored as a landing page.
 * - the tutorial page is authored Markdown in the Learn tier: prose, an aside, code fences.
 * - the command page is a generated Reference page, assembled from `docs/manifest.json` at sync time.
 * - the platform page is the heaviest hand-written body on the site: long tables and measured
 *   envelopes in fences, which is where a contrast or focus regression in the code theme shows up.
 * - the system-overview page is the generated tree's shape: a ccu page carrying a Mermaid figure
 *   rendered to inline SVG at build time. Measured 2026-09-05: no page among the first four carries
 *   one, and the census in `tests/built-site.test.ts` refused the four-page sample for exactly that
 *   reason, so the diagram template is audited here rather than exempted from the census.
 *
 * `tests/built-site.test.ts` proves the sample is representative for the one property where a small
 * sample could silently under-report: every distinct inline `<svg>` shape the whole site emits
 * appears on one of these pages.
 */
export const AUDITED_PAGES: ReadonlyArray<string> = [
  `${BASE}`,
  `${BASE}learn/tutorial/first-run/`,
  `${BASE}reference/commands/run/`,
  `${BASE}internals/platform/`,
  `${BASE}internals/architecture/system-overview/`
]

/**
 * WCAG 2.2 AA violations this site ships today, each one owned somewhere this gate cannot reach.
 *
 * A baseline is a ratchet, not an exemption, and `tests/a11y.test.ts` enforces it in both directions:
 * a violation outside this list fails, and an entry here that no longer fires fails too, so a fix
 * cannot leave a suppression behind to hide the next defect.
 *
 * `signature` is what keeps an entry from widening into a license for its whole rule. It is matched
 * against each violating node's markup followed by the failure summary, and every node has to be
 * claimed by some entry, so the same rule failing on a different element is a failure even though the
 * rule id is listed. A rule may therefore appear more than once: two independent defects with two
 * owners are two entries, and the ratchet holds each of them separately.
 *
 * Every entry here was MEASURED against `dist/` before it was written down; none is a guess about
 * what Starlight probably does. Measured 2026-09-05 against Starlight 0.41.10 over the five audited
 * pages at 1280x800, light color scheme.
 */
export const KNOWN_A11Y_FAILURES: ReadonlyArray<{
  readonly rule: string
  readonly criterion: string
  readonly signature: RegExp
  readonly owner: string
  readonly why: string
}> = [
  {
    rule: "label-content-name-mismatch",
    criterion: "SC 2.5.3 Label in Name",
    signature: /data-open-modal/,
    owner: "@astrojs/starlight",
    why:
      "Starlight's search button renders `Search` beside a `Ctrl` `K` shortcut hint but names " +
      "itself `Search`, so its accessible name does not contain its visible text. The markup is " +
      "the theme's own component, so fixing it means shadowing that component rather than editing " +
      "content or a token. Fires once on every audited page."
  },
  {
    rule: "inline-svg-undecided",
    criterion: "SC 1.1.1 Non-text Content",
    signature:
      /^<svg xmlns="http:\/\/www\.w3\.org\/2000\/svg" viewBox="[^"]*" width="[^"]*" height="[^"]*" style="--bg:/,
    owner: "site/src/lib/mermaid.ts",
    why:
      'Every build-time Mermaid figure is emitted as `<figure class="docs-mermaid" tabindex="0">` ' +
      "around a bare `<svg>` with no role, no `<title>` and no `aria-label`, so assistive technology " +
      "meets shapes and edges with no name. The signature is the renderer's exact opening tag " +
      "(beautiful-mermaid's `--bg`/`--fg` theme variables are what make it unmistakable), so a second " +
      "kind of undecided SVG still fails. The fix belongs to the plugin, not to any page: give the " +
      'figure `role="img"` and a name derived from the fence\'s title or the nearest heading, and this ' +
      "entry then fails as stale and is deleted. Measured on /internals/architecture/system-overview/."
  }
]

/** Where `astro build` writes, relative to the package root. */
export const DIST_DIR = "dist"

/**
 * The Cumulative Layout Shift a page may not reach, in CLS units (unitless, per page, 0 is perfect).
 *
 * 0.1 is the Core Web Vitals "good" boundary rather than a number chosen to fit this site: every
 * audited page measures 0 today (`tests/layout-stability.test.ts`), so the headroom is not budget
 * anyone is spending. It lives here, beside the page list, because the probe that enforces it and the
 * case in `tests/built-site.test.ts` that keeps `lighthouserc.json` honest both need the same number,
 * and Lighthouse's own CLS reading is the one this repo does NOT gate on; see that probe's header for
 * the measurement that decided it.
 */
export const LAYOUT_SHIFT_CEILING = 0.1

/**
 * Layout shifts this site ships today, declared so every OTHER shift still fails.
 *
 * Empty as measured 2026-09-05: five audited pages at 1350x940, zero `layout-shift` entries each,
 * observer installed before the first byte and left running 1.5 s past network idle and fonts ready.
 *
 * An entry here is a bound, not a license: `node` has to match the shift's own source element and
 * `most` caps how far it may move, so the same element shifting further fails, and any other element
 * shifting at all fails. What an entry deliberately does NOT do is assert that the shift still fires.
 * That half of the `KNOWN_A11Y_FAILURES` ratchet cannot hold here, because whether a paint race is
 * lost depends on the host: memhtml-public measured a Starlight right-sidebar shift on a 4-vCPU CI
 * runner that never fired on a laptop. Deleting the entry is how one retires.
 */
export const KNOWN_LAYOUT_SHIFTS: ReadonlyArray<{
  readonly node: string
  readonly most: number
  readonly why: string
}> = []

/**
 * The most bytes one audited page may transfer, in bytes, as Lighthouse's `total-byte-weight`
 * counts them (every resource the page load requested, compressed as served).
 *
 * `lighthouserc.json` carries the same number, because lhci reads only that file, and
 * `tests/built-site.test.ts` asserts the two agree so the budget is edited in one place with its
 * measurement beside it. JSON has no comments; this is where the comment lives.
 *
 * MEASURED 2026-09-05, `lhci collect` over `tests/static-server.ts`, desktop preset, three runs per
 * page, every run of a page identical to the byte:
 *
 *   /                                          283269
 *   /learn/tutorial/first-run/                 288810
 *   /reference/commands/run/                   282293
 *   /internals/platform/                       474253   (the heaviest: the platform document's body)
 *   /internals/architecture/system-overview/   266284
 *
 * The budget is the heaviest page plus roughly 20 percent (474253 x 1.2 = 569104, rounded up), so a
 * dependency that ships a new client bundle fails the gate rather than growing quietly inside slack,
 * while a long measured table added to Platform does not. Re-measure with `mise run docs:budget` and
 * move both numbers together when the site legitimately grows.
 */
export const TOTAL_BYTE_WEIGHT_BUDGET = 570000
