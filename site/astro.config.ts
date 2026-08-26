// SPDX-License-Identifier: Apache-2.0
import { readFileSync } from "node:fs"
import { fileURLToPath } from "node:url"
import { resolve } from "node:path"

import { satteri } from "@astrojs/markdown-satteri"
import starlight from "@astrojs/starlight"
import { defineConfig } from "astro/config"
import { starlightBasePath } from "starlight-base-path"
import starlightLinksValidator from "starlight-links-validator"
import starlightLlmsTxt from "starlight-llms-txt"
import starlightMdTxt from "starlight-md-txt"

import { agentNotePlugin } from "./src/lib/agent-note.js"
import { baseRawLinks } from "./src/lib/base-raw-links.js"
import { citationLinks } from "./src/lib/citation-links.js"
import mermaid from "./src/lib/mermaid.js"
import { robotsPolicyFile } from "./src/lib/robots.js"
import { rootTwin } from "./src/lib/root-twin.js"

/**
 * Where this site is published.
 *
 * Both are environment-overridable because the base decides two things a hardcoded value would freeze:
 * every URL the machine surfaces emit, and whether a robots.txt can exist at all (RFC 9309 §2.3 puts it
 * at the origin root, so a path-prefixed site cannot own one). Moving to a custom domain is therefore
 * two variables in the deploy workflow rather than an edit here.
 *
 * `site` EXCLUDES the base segment, matching what `Astro.site` holds. `base` includes it, matching
 * `import.meta.env.BASE_URL`. Nothing derives one from the other, because confusing the two produces a
 * protocol-relative URL that parses, resolves to nothing, and looks correct in source.
 */
const SITE = new URL(process.env.DOCS_SITE ?? "https://theagenticguy.github.io")

/**
 * Exactly one leading and one trailing slash, whatever the caller wrote.
 *
 * The deploy workflow derives the base from the repository name, and a user or organization Pages site
 * derives `/` — so `${name}/` would produce `//`, a base that reads as a protocol-relative URL naming a
 * host. Normalizing here means the env var may be `/x`, `/x/`, `x`, or `/` and the rest of this file has
 * one shape to reason about.
 */
const normalizeBase = (value: string): string => `/${value.replace(/^\/+|\/+$/g, "")}/`.replace(/^\/{2,}/, "/")

const BASE = normalizeBase(process.env.DOCS_BASE ?? "/microvms-agentd/")

const SITE_TITLE = "microvms-agentd"
const SITE_DESCRIPTION =
  "A verified client stack and in-VM daemon for AWS Lambda MicroVMs, in Rust: the wire protocol, " +
  "the trust boundary, and measured platform behavior."

const REPO_URL = "https://github.com/theagenticguy/microvms-agentd"

const here = fileURLToPath(new URL(".", import.meta.url))
const REPO_ROOT = resolve(here, "..")
const TREE_ROOT = resolve(REPO_ROOT, "docs")
const MANIFEST = "src/content/docs/.sync-manifest.json"

/**
 * What `scripts/sync-docs.mjs` published, read back rather than restated.
 *
 * Two consumers need it and both would otherwise drift from the sync's own decisions. The citation
 * rewriter needs the set of pages that exist as routes, so a citation into a tree page the site does not
 * publish becomes a repo permalink instead of a link to a route nobody built. The sidebar needs the
 * hand-authored tier's labels, so a document added to `docs/` reaches the navigation rail on the run
 * that publishes it — a config listing them by hand is how a published page becomes an orphan.
 *
 * The commit comes from here too, so every permalink on the site — the ones the sync wrote into link
 * targets and the ones this config's plugin writes into citations — pins the same SHA.
 */
interface SyncManifest {
  readonly commit: string
  readonly publishedTreePaths: ReadonlyArray<string>
  readonly sidebar: ReadonlyArray<{ readonly label: string; readonly link: string }>
}

const manifest: SyncManifest = (() => {
  try {
    return JSON.parse(readFileSync(resolve(here, MANIFEST), "utf8")) as SyncManifest
  } catch (cause) {
    throw new Error(
      `${MANIFEST} is absent or unreadable. The content directory is generated: run ` +
        "`pnpm run sync` (which `pnpm run build` and `pnpm run dev` do for you) before loading " +
        "this config.",
      { cause }
    )
  }
})()

/**
 * The `llms.txt` prose block, and the only place an entry can be listed FIRST.
 *
 * `starlight-llms-txt` assembles `llms.txt` as an ordered array of segments fixed in code: title,
 * description, this block verbatim, then `## Documentation Sets`, `## Notes`, and `## Optional`.
 * `optionalLinks` renders at the very end and `customSets` renders inside Documentation Sets *after*
 * both bundle links, so neither can place anything ahead of them. llmstxt.org sanctions exactly this
 * content in this slot — "markdown sections of any type except headings" — so a link list here is
 * in-spec rather than a workaround.
 *
 * Placement is the deliverable: an agent does not wander, and guidance outside the loaded context did
 * not happen.
 */
const llmsDetails = [
  "Read this first. It is the page written for a machine reader, and it names which surface answers",
  "which question:",
  "",
  `- [For agents](${new URL(`${BASE}agents.md`, SITE).href})`,
  "",
  "Two tiers of document live here and they are not equally reliable. The hand-written documents —",
  "Platform, Protocol, Trust, Embedding, Strategy, Harness capabilities — carry measured findings and",
  "design rationale, and they win any disagreement. Everything else was generated per-file from the",
  "source tree, and every factual claim in those pages carries a `path:line` citation that was",
  "machine-verified against the source at generation time. Those citations anchor to line numbers, so",
  "they rot when the cited code moves while still reading as authoritative.",
  "",
  "Any page is available as Markdown: append `.md` to its path."
].join("\n")

export default defineConfig({
  site: SITE.href,
  base: BASE,

  /*
   * `directory` plus a trailing slash, matched by `jsonld.ts`'s own page-URL builder and by the
   * `rel="canonical"` Starlight emits. GitHub Pages serves `/page/` from `/page/index.html`, so the
   * directory format is what makes a link without an extension resolve on that host.
   */
  build: { format: "directory" },
  trailingSlash: "always",

  markdown: {
    /*
     * The processor is named explicitly even though Sätteri is Astro 7's default, because naming it is
     * the only way to contribute `mdastPlugins`. Both plugins here claim at MDAST rather than HAST, and
     * that is not a style choice: Expressive Code pushes ITSELF as a hast plugin and returns a
     * replacement for the whole `pre` subtree, so a hast visitor that edits a code block is discarded
     * with no error. At mdast a fence is still a fence and nothing downstream has claimed it.
     *
     * `remarkPlugins` and `rehypePlugins` are inert under this processor — no error, no warning, no
     * effect — so a `remark-*` package is not a candidate for anything below.
     */
    processor: satteri({
      mdastPlugins: [
        agentNotePlugin(),
        citationLinks({
          commit: manifest.commit,
          repoUrl: REPO_URL,
          repoRoot: REPO_ROOT,
          treeRoot: TREE_ROOT,
          /*
           * Empty, and NOT `BASE`. The tree is published at the content root, and the base segment is
           * added by `starlightBasePath()` to every root-relative link produced in this pipeline — so
           * passing the base here prefixes it twice and every citation into a published page becomes
           * `/microvms-agentd/microvms-agentd/platform/`.
           */
          siteBase: "",
          published: new Set(manifest.publishedTreePaths)
        })
      ]
    })
  },

  integrations: [
    /*
     * FIRST, so every later pass sees one root twin at its final path rather than two spellings of it.
     * `starlight-md-txt` emits the landing page's twin as the dotfile `.md`, which GitHub Pages refuses
     * to serve — a 404 no gate over `dist/` can see, because the file is there and the host is the one
     * declining. This renames it to `index.md`.
     */
    rootTwin(),

    /*
     * Registered ahead of Starlight, and it closes a defect that is invisible from every angle a build
     * reports on: the raw `.md` twins are built from each page's Markdown SOURCE while the rendered tree
     * is built from the compiled page, so a root-relative link comes out correct on one surface and a
     * 404 on the other. `starlight-base-path` fixes the rendered half only.
     */
    baseRawLinks(BASE),

    /*
     * Emits the crawler policy when the site owns its origin, and logs the RFC 9309 constraint when it
     * does not. At `/microvms-agentd/` it writes nothing, because a robots.txt under a path segment has
     * no protocol meaning and would read as a policy while governing nothing.
     */
    robotsPolicyFile(BASE, SITE),

    /*
     * Diagrams render at BUILD time and land in the page as inline SVG. A client-rendered diagram is
     * absent from the raw twin, absent from all three llms bundles, and absent from any fetch that runs
     * no JavaScript — which ships the densest thing on the page to the audience that already has the
     * prose and withholds it from the audience that needs the structure.
     *
     * The colours are the page's own theme tokens rather than fixed values, so ONE build-time asset
     * tracks light and dark with no second render and no media query inside the SVG. That only works
     * because the SVG is inlined: a `var()` inside an `<img src="diagram.svg">` resolves in the image's
     * own document, where these properties do not exist, and the diagram renders with no colour at all.
     */
    mermaid({
      renderer: undefined,
      className: "docs-mermaid"
    }),

    starlight({
      title: SITE_TITLE,
      description: SITE_DESCRIPTION,

      /*
       * Both overrides DELEGATE to the default and add. Replacing `Head` drops the canonical link and
       * the sitemap reference, and replacing `PageTitle` drops the heading id that the skip link and the
       * table of contents both target.
       */
      components: {
        Head: "./src/components/Head.astro",
        PageTitle: "./src/components/PageTitle.astro"
      },

      customCss: ["./src/styles/docs.css"],

      /*
       * Read from git so the JSON-LD `dateModified` and the page footer describe the SOURCE file's last
       * change. The content directory is generated and untracked, so `sync-docs.mjs` stamps the value
       * per page instead; this option is what makes Starlight honour the stamp.
       */
      lastUpdated: true,

      social: [{ icon: "github", label: "GitHub", href: REPO_URL }],

      /*
       * No `editLink`. Every page under `src/content/docs/` is generated, so an edit link points at a
       * file whose next build overwrites the edit. Each page carries `editUrl: false` for the same
       * reason, and the change belongs in `docs/` or in `site/authored/`.
       */

      sidebar: [
        // First, above everything: an agent arriving at this site should not have to find this page.
        { label: "For agents", link: "/agents/" },
        {
          /*
           * Derived from the sync manifest, because these documents predate the generated tree and win
           * any disagreement with it — so the rail has to list all of them, and a hand-written list here
           * would silently omit the next one.
           */
          label: "Authoritative",
          collapsed: false,
          items: [...manifest.sidebar]
        },
        // ccu's own reading order across the six categories, rather than alphabetical.
        {
          label: "Architecture",
          collapsed: false,
          items: [{ autogenerate: { directory: "architecture" } }]
        },
        { label: "Reference", collapsed: true, items: [{ autogenerate: { directory: "reference" } }] },
        { label: "Behavior", collapsed: true, items: [{ autogenerate: { directory: "behavior" } }] },
        { label: "Analysis", collapsed: true, items: [{ autogenerate: { directory: "analysis" } }] },
        {
          /*
           * Each subgroup is named explicitly. ccu nests diagrams one level deeper than the other five
           * categories, so autogenerating `diagrams` as one directory produces a subgroup labelled
           * "architecture" beside the top-level Architecture group — two rail entries with one name.
           */
          label: "Diagrams",
          collapsed: true,
          items: [
            { label: "Components", items: [{ autogenerate: { directory: "diagrams/architecture" } }] },
            { label: "Structure", items: [{ autogenerate: { directory: "diagrams/structural" } }] },
            { label: "Sequences", items: [{ autogenerate: { directory: "diagrams/behavioral" } }] }
          ]
        },
        { label: "Insights", collapsed: true, items: [{ autogenerate: { directory: "insights" } }] }
      ],

      plugins: [
        /*
         * EXACTLY ONE dependency may own a route pattern, and two owners is not an error any build
         * reports: both register a producer, nothing names a winner, and the bytes served stop being
         * attributable to an implementation anyone chose. The inventory for this set:
         *
         *   /[...slug].md                                     starlight-md-txt
         *   /llms.txt, /llms-full.txt, /llms-small.txt         starlight-llms-txt
         *   /_llms-txt/[slug].txt                              starlight-llms-txt
         *   /sitemap-index.xml, /sitemap-0.xml                 Starlight's own sitemap
         *
         * `starlight-page-context-action` is the declined candidate: it emits its own `.md` and
         * `llms.txt` routes, which collide with both plugins below, and its MDX cleaner is regex-based.
         * `src/components/PageActions.astro` is what replaces it.
         */
        starlightMdTxt(),

        starlightLlmsTxt({
          projectName: SITE_TITLE,
          description: SITE_DESCRIPTION,
          details: llmsDetails
        }),

        /*
         * Lets content author `/agents/` and have it resolve under the base segment. It augments the
         * processor configured above IN PLACE rather than replacing it, so the mdast plugins registered
         * there survive. It reaches the rendered tree and the llms bundles and NOT the raw twins, which
         * is what `baseRawLinks` above is for.
         */
        starlightBasePath(),

        starlightLinksValidator({
          /*
           * The raw `.md` twins come from an injected dynamic route, so they are absent from the page
           * list this validator checks against and every link to one reports `InvalidLink` while
           * resolving perfectly in the browser. The exclusion is only allowed because two stricter gates
           * replace it: `tests/raw-route-links.test.ts` and `tests/agent-surface.test.ts` resolve every
           * link against the files actually present in `dist/`, which needs no side table at all.
           */
          exclude: ({ link }) => link.endsWith(".md"),

          /*
           * The generated tree cross-links with relative targets — `../insights/contract-map.md` and
           * siblings, hundreds of them — and that is the correct form here rather than something to
           * migrate. A relative target is the ONE link shape that resolves on both surfaces under a base
           * segment: Astro rewrites it to a route in the rendered page, and on the raw twin it resolves
           * against the twin's own directory, which already carries the base.
           */
          errorOnRelativeLinks: false
        })
      ]
    })
  ]
})
