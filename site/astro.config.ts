// SPDX-License-Identifier: Apache-2.0
import { readFileSync } from "node:fs"
import { resolve } from "node:path"
import { fileURLToPath } from "node:url"

import { satteri } from "@astrojs/markdown-satteri"
import starlight from "@astrojs/starlight"
import { pluginCollapsibleSections } from "@expressive-code/plugin-collapsible-sections"
import { pluginLineNumbers } from "@expressive-code/plugin-line-numbers"
import { defineConfig, passthroughImageService } from "astro/config"
import { starlightBasePath } from "starlight-base-path"
import starlightLinksValidator from "starlight-links-validator"
import starlightLlmsTxt from "starlight-llms-txt"
import starlightMdTxt from "starlight-md-txt"
import starlightScrollToTop from "starlight-scroll-to-top"

import { agentNotePlugin } from "./src/lib/agent-note.js"
import { baseRawLinks } from "./src/lib/base-raw-links.js"
import { citationLinks } from "./src/lib/citation-links.js"
import { focusableScrollers } from "./src/lib/focusable-scrollers.js"
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
const normalizeBase = (value: string): string =>
  `/${value.replace(/^\/+|\/+$/g, "")}/`.replace(/^\/{2,}/, "/")

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
 *
 * `routes` and `redirects` arrived with the tiers. A tree page is no longer served at its lowercased
 * tree path — `PLATFORM.md` is `/internals/platform/` — so the citation rewriter reads the route off
 * the same map the sync wrote the page with, and every old route is handed to Astro as a redirect
 * rather than listed here by hand.
 */
interface SyncManifest {
  readonly commit: string
  readonly publishedTreePaths: ReadonlyArray<string>
  readonly routes: Readonly<Record<string, string>>
  readonly redirects: Readonly<Record<string, string>>
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
  "The site has three tiers. Learn (`/learn/`) is task-shaped: the tutorials in order, then how-tos.",
  "Reference (`/reference/`) is generated from `microvm manifest`, the binary's own statement of every",
  "command, exit code, and response type, so a page there is the contract the binary ships. Internals",
  "(`/internals/`) is the reasoning, in two kinds of document that are not equally reliable. The",
  "hand-written documents — Platform, Protocol, Trust, Embedding, Strategy, Harness capabilities —",
  "carry measured findings and design rationale, and they win any disagreement. The generated",
  "categories — Architecture, Behavior, Analysis, Diagrams, Insights — were produced per-file from the",
  "source tree, and every factual claim there carries a `path:line` citation pinned to the commit the",
  "site was built from. Those citations anchor to line numbers, so they rot when the cited code moves",
  "while still reading as authoritative.",
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

  /*
   * Every route a tree page had before the tiers, answering with a redirect to where the page is now.
   * Derived by the sync from the same rule that placed the pages, so a category that moves later
   * carries its redirects with it. Astro emits each as a static page holding a meta refresh, which is
   * what a static host can serve.
   *
   * The destination carries the base and the source does not, and that asymmetry is measured rather
   * than assumed. Astro prefixes the base onto the SOURCE pattern itself, and when the destination
   * names a route it knows, it regenerates the location from that route's segments — which hold no
   * base — so `dist/platform/index.html` refreshed to `/internals/platform/`, one segment short of the
   * page, on a host that serves the site under `/microvms-agentd/`. A destination the route map does
   * not recognise is emitted verbatim, so spelling it with the base here is what reaches the page.
   */
  redirects: Object.fromEntries(
    Object.entries(manifest.redirects).map(([from, to]) => [
      from,
      `${BASE}${to.replace(/^\//, "")}`
    ])
  ),

  /*
   * pnpm's isolated node_modules puts sharp out of reach of a resolve from this app's root, so Astro's
   * default image service exits 1 with MissingSharp on the first raster image — a latent failure that a
   * build with no images does not reveal. This site optimises no images; the social cards are drawn
   * by their own route. Ported from memhtml-public's config.
   */
  image: { service: passthroughImageService() },

  /*
   * `canvaskit-wasm` stays out of the SSR bundle, and it is a DIRECT dependency of this package for the
   * same reason. It ships as UMD and reads `__dirname`, which is not defined in the ESM chunk Vite would
   * otherwise inline it into — the social-card route then fails at render with a `ReferenceError` that
   * names neither the package nor the cause. Left external it is required at run time as CommonJS,
   * where `__dirname` exists, and its own `createRequire` locates the wasm binary beside it. Ported
   * from memhtml-public's config.
   */
  vite: { ssr: { external: ["canvaskit-wasm"] } },

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
          published: new Set(manifest.publishedTreePaths),
          /*
           * The route comes from the sync manifest rather than from the plugin's default, which lowercases
           * the tree path. That default was the route until the tiers arrived; now `PLATFORM.md` is served
           * at `/internals/platform/` and `reference/cli.md` stays at `/reference/cli/`, and only the sync
           * knows which rule placed which page. The fallback is the default, for a tree path the manifest
           * does not name — which `published` above already keeps from reaching here.
           */
          intraSiteHref: (target) =>
            (target.treePath === undefined ? undefined : manifest.routes[target.treePath]) ??
            `/${target.slug}/`
        })
      ],
      /*
       * Starlight PUSHES its own hast plugins after these, so this runs first and its `tabindex`
       * survives whatever Starlight adds afterwards. See the plugin for the SC 2.1.1 finding it closes.
       */
      hastPlugins: [focusableScrollers()]
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

      /*
       * `rfc.css` is the specification register — serif body, warm paper, one accent — and it is where
       * `--docs-rule` is defined. `docs.css` carries the page-action controls, the agent note, and the
       * diagram styles, and it stays independent of the register so a register change cannot break a
       * control's target size.
       */
      customCss: ["./src/styles/docs.css", "./src/styles/rfc.css"],

      /*
       * A code block is the one place on this site where a second typeface appears, so it is framed
       * rather than tinted: square corners and a hairline rule in the same value the tables use, which
       * is what keeps it reading as a figure inside a specification instead of a widget dropped onto
       * the page. Expressive Code's defaults are a 0.3rem radius and a drop shadow, and both are wrong
       * against a serif body. `codeFontSize` is 0.85em rather than 1em because the mono stack's
       * x-height runs ahead of the serif's at equal size; the two only look like one document at this
       * ratio. Ported from memhtml-public's config.
       */
      expressiveCode: {
        plugins: [pluginLineNumbers(), pluginCollapsibleSections()],
        /*
         * A ```mermaid fence is a figure source, not a code sample: the `mermaid()` integration above
         * claims it at mdast and replaces the block with an SVG. Expressive Code would still see the
         * fence first if it ever reached hast, find no highlighter for `mermaid`, and warn once per
         * figure. Aliasing it to plain text says what is true: nothing here needs highlighting.
         */
        shiki: { langAlias: { mermaid: "txt" } },
        styleOverrides: {
          borderRadius: "0",
          borderColor: "var(--docs-rule)",
          borderWidth: "1px",
          // The same tint an inline code span already sits on, so a block and a span are one material.
          // The bundled themes ship a violet-cast ground that belongs to no value in this palette.
          codeBackground: "var(--sl-color-bg-inline-code)",
          codeFontFamily: "var(--sl-font-mono)",
          codeFontSize: "0.85em",
          /*
           * The gutter's own foreground, per theme. Expressive Code's light default measured 2.34:1
           * (#90a7b2 on this site's #f2efe6 code ground) in `tests/a11y.test.ts`, twelve nodes on the
           * Platform page alone; a line number is body-size text, so SC 1.4.3 wants 4.5:1. Measured:
           * #4a5560 on #f2efe6 is 6.62:1; the same value on the dark ground #1e1e1c is 2.19:1, so the
           * dark theme takes rfc.css's dark secondary ink #b9b6ae, 8.24:1 there.
           */
          lineNumbers: {
            foreground: ({ theme }) => (theme.type === "dark" ? "#b9b6ae" : "#4a5560")
          },
          frames: {
            shadowColor: "transparent",
            editorTabBorderRadius: "0",
            frameBoxShadowCssValue: "none"
          }
        }
      },

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

      /*
       * Diátaxis, three tiers: Learn is task-shaped, Reference is derived from `microvm manifest`, and
       * Internals is the reasoning. An `autogenerate` entry has to sit inside `items` — a group carrying
       * it directly is rejected — and `collapsed` does not cascade, so each group states its own.
       */
      sidebar: [
        // First, above everything: an agent arriving at this site should not have to find this page.
        { label: "For agents", link: "/agents/" },
        {
          /*
           * Explicit rather than one `autogenerate` over `learn`: Starlight would label the two
           * subgroups from their directory names, lowercase, and sort them alphabetically, which puts
           * the how-tos above the tutorials they assume. The order here is the reading order.
           */
          label: "Learn",
          collapsed: false,
          items: [
            { label: "Overview", link: "/learn/" },
            { label: "Tutorials", items: [{ autogenerate: { directory: "learn/tutorial" } }] },
            { label: "How-to", items: [{ autogenerate: { directory: "learn/operations" } }] }
          ]
        },
        {
          label: "Reference",
          collapsed: true,
          items: [
            { label: "Overview", link: "/reference/" },
            // The four contract pages first: they are what a caller branches on, and they are short.
            { label: "Envelope", link: "/reference/envelope/" },
            { label: "Exit codes", link: "/reference/exit-codes/" },
            { label: "Response types", link: "/reference/response-types/" },
            { label: "Wire schema", link: "/reference/wire-schema/" },
            {
              /*
               * One page per `microvm` subcommand, written by `scripts/gen-reference.mjs` from the CLI's
               * own manifest, so the rail lists a command on the run that publishes its page. Collapsed,
               * because the list is as long as the command surface.
               */
              label: "Commands",
              collapsed: true,
              items: [{ autogenerate: { directory: "reference/commands" } }]
            },
            {
              // ccu's three reference pages: generated too, but from the source tree rather than the
              // manifest, and carrying `path:line` citations the manifest cannot.
              label: "Annotated",
              items: [
                { label: "CLI", link: "/reference/cli/" },
                { label: "Public API", link: "/reference/public-api/" },
                { label: "RPC tools", link: "/reference/rpc-tools/" }
              ]
            }
          ]
        },
        {
          label: "Internals",
          collapsed: true,
          items: [
            { label: "Overview", link: "/internals/" },
            {
              /*
               * Derived from the sync manifest, because these documents predate the generated tree and
               * win any disagreement with it — so the rail has to list all of them, and a hand-written
               * list here would silently omit the next one.
               */
              label: "Authoritative",
              items: [...manifest.sidebar]
            },
            // ccu's own reading order across its categories, rather than alphabetical.
            {
              label: "Architecture",
              items: [{ autogenerate: { directory: "internals/architecture" } }]
            },
            { label: "Behavior", items: [{ autogenerate: { directory: "internals/behavior" } }] },
            { label: "Analysis", items: [{ autogenerate: { directory: "internals/analysis" } }] },
            {
              /*
               * Each subgroup is named explicitly. ccu nests diagrams one level deeper than the other
               * categories, so autogenerating `diagrams` as one directory produces a subgroup labelled
               * "architecture" beside the Architecture group — two rail entries with one name.
               */
              label: "Diagrams",
              items: [
                {
                  label: "Components",
                  items: [{ autogenerate: { directory: "internals/diagrams/architecture" } }]
                },
                {
                  label: "Structure",
                  items: [{ autogenerate: { directory: "internals/diagrams/structural" } }]
                },
                {
                  label: "Sequences",
                  items: [{ autogenerate: { directory: "internals/diagrams/behavioral" } }]
                }
              ]
            },
            { label: "Insights", items: [{ autogenerate: { directory: "internals/insights" } }] }
          ]
        },
        { label: "Glossary", link: "/glossary/" }
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
          details: llmsDetails,
          /*
           * A separate lever on a separate file: `promote` orders page bodies inside `llms-full.txt`,
           * where the default is `['index*']` alone. Listing `agents` after it keeps the cover page
           * first and puts the agent page immediately behind it.
           */
          promote: ["index*", "agents"]
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
        }),

        // A return-to-top control on long pages; Platform alone runs past a thousand lines of source.
        starlightScrollToTop()
      ]
    })
  ]
})
