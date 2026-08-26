// SPDX-License-Identifier: Apache-2.0
/**
 * The JSON-LD graph emitted from the `Head` override: one `TechArticle` per page and one `WebSite` node
 * for the site, joined in a single `@graph`.
 *
 * This module is pure and takes the site's origin and base segment as arguments rather than reading
 * `import.meta.env`, so every URL and every node it emits is asserted in a unit test rather than only
 * in a validator run against a deployed page.
 *
 * Every URL is built through `siteUrl`, which resolves with `new URL()`. A base segment joined to a
 * path by string concatenation produces `//microvms-agentd/index.md` — a protocol-relative URL naming
 * a *host* — and the failure is silent: the value parses, resolves to nothing, and looks right in the
 * source and in the rendered `<script>` block alike. A structured-data consumer that cannot resolve
 * `@id` merges nothing, which is the whole graph lost with no error anywhere.
 *
 * **Why one `@graph` and not two `<script>` blocks.** Two blocks are two disconnected graphs, so the
 * article's `isPartOf` cannot be resolved against the site node and the relationship is asserted
 * without being expressible. One `@graph` with `@id`-addressed nodes is what makes the reference join.
 *
 * **Why `TechArticle` and not `Article`.** `TechArticle` is schema.org's subtype for technical
 * documentation and it carries the two properties a docs page can honestly fill — `proficiencyLevel`
 * and `dependencies`. `Article` is shaped for editorial and news, where the expected properties are
 * `author`, `datePublished` and `publisher`; a docs page usually has no single author and no
 * publication date distinct from its last edit, so filling those to satisfy a validator invents facts.
 * An empty optional beats a fabricated value: a consumer treats an absent property as unknown and a
 * present one as asserted.
 */

import { siteUrl, type SiteContext } from "./agent-surface.js"

/**
 * The site node, described once.
 *
 * `searchEndpoint` is the URL template for a `SearchAction`, and it is optional because the honest
 * default is to omit it. A `SearchAction` is a promise that a GET against the template returns
 * results, and this site's search runs entirely in the browser through Pagefind, so no such endpoint
 * exists. Declaring one anyway ships a machine-readable claim that 404s on the first consumer that
 * follows it.
 */
export interface SiteDescription {
  readonly name: string
  readonly description: string
  /** A BCP 47 tag. Written out rather than defaulted, because a wrong language tag is worse than none. */
  readonly inLanguage: string
  /** The organization or person the site belongs to, omitted when there is no honest answer. */
  readonly publisher?: { readonly name: string; readonly url?: string } | undefined
  /**
   * A URL template containing the literal `SEARCH_TERM_STRING`, for example
   * `https://example.com/search/?q=SEARCH_TERM_STRING`. Omit unless that URL really returns results.
   */
  readonly searchEndpoint?: string | undefined
}

/** One page, as the graph describes it. */
export interface PageDescription {
  /** The content-collection entry id, the same value the raw-Markdown route is derived from. */
  readonly entryId: string
  readonly title: string
  readonly description: string
  /**
   * ISO 8601. Omitted when the build does not know it. A docs page's `dateModified` is honest when it
   * comes from git; a `datePublished` invented to fill a required-looking field is not.
   */
  readonly datePublished?: string | undefined
  readonly dateModified?: string | undefined
  /** `TechArticle.proficiencyLevel`: schema.org's enumerated values are `Beginner` and `Expert`. */
  readonly proficiencyLevel?: "Beginner" | "Expert" | undefined
  /** `TechArticle.dependencies`: prose naming what a reader needs before this page is useful. */
  readonly dependencies?: string | undefined
}

/** A node with an `@id` other nodes reference. */
interface Referenced {
  readonly "@id": string
}

/** The trailing-slash-normalized page URL, which is what `trailingSlash: "always"` serves. */
const pageUrl = (entryId: string, context: SiteContext): URL =>
  siteUrl(entryId === "index" || entryId === "" ? "" : `${entryId}/`, context)

/**
 * The raw-Markdown twin, addressed as a `MediaObject`.
 *
 * This is the highest-value node in the graph and the reason the graph is worth emitting at all: it is
 * the machine-readable statement that a Markdown representation of this page exists at a specific URL.
 * `encoding` on a `CreativeWork` expects a `MediaObject`, and `encodingFormat` carries the media type —
 * so a consumer reads "same work, other bytes, `text/markdown`, here" without knowing this site's
 * `.md` convention. The `<head>` `rel="alternate"` link says the same thing to a different reader;
 * neither makes the other redundant, because they are read by different consumers.
 */
const markdownEncoding = (entryId: string, context: SiteContext) => {
  const href = siteUrl(`${entryId === "index" ? "" : entryId}.md`, context).href
  return {
    "@type": "MediaObject" as const,
    "@id": href,
    contentUrl: href,
    encodingFormat: "text/markdown"
  }
}

const websiteId = (context: SiteContext): string => `${siteUrl("", context).href}#website`

/** The site node. Emitted once per page, with a stable `@id` so consumers merge rather than duplicate. */
export const websiteNode = (site: SiteDescription, context: SiteContext) => {
  const root = siteUrl("", context).href
  return {
    "@type": "WebSite" as const,
    "@id": websiteId(context),
    url: root,
    name: site.name,
    description: site.description,
    inLanguage: site.inLanguage,
    ...(site.publisher === undefined
      ? {}
      : {
          publisher: {
            "@type": "Organization" as const,
            name: site.publisher.name,
            ...(site.publisher.url === undefined ? {} : { url: site.publisher.url })
          }
        }),
    ...(site.searchEndpoint === undefined
      ? {}
      : {
          potentialAction: {
            "@type": "SearchAction" as const,
            target: {
              "@type": "EntryPoint" as const,
              urlTemplate: site.searchEndpoint
            },
            "query-input": "required name=search_term_string"
          }
        })
  }
}

/** One page's article node. */
export const articleNode = (
  page: PageDescription,
  site: SiteDescription,
  context: SiteContext
) => {
  const url = pageUrl(page.entryId, context).href
  const isPartOf: Referenced = { "@id": websiteId(context) }
  return {
    "@type": "TechArticle" as const,
    "@id": `${url}#article`,
    url,
    /*
     * `headline` and `name` carry the same string on purpose. A consumer looking for an article reads
     * `headline`; one walking the graph generically reads `name`. Emitting one leaves the other
     * unanswerable, and the value is not derived differently for the two.
     */
    headline: page.title,
    name: page.title,
    description: page.description,
    inLanguage: site.inLanguage,
    isPartOf,
    mainEntityOfPage: { "@type": "WebPage" as const, "@id": url },
    encoding: markdownEncoding(page.entryId, context),
    ...(page.datePublished === undefined ? {} : { datePublished: page.datePublished }),
    ...(page.dateModified === undefined ? {} : { dateModified: page.dateModified }),
    ...(page.proficiencyLevel === undefined ? {} : { proficiencyLevel: page.proficiencyLevel }),
    ...(page.dependencies === undefined ? {} : { dependencies: page.dependencies })
  }
}

/** The whole graph for one page: its article, and the site it belongs to. */
export const jsonLdGraph = (page: PageDescription, site: SiteDescription, context: SiteContext) => ({
  "@context": "https://schema.org",
  "@graph": [articleNode(page, site, context), websiteNode(site, context)]
})

/**
 * The graph as the text of a `<script type="application/ld+json">` block.
 *
 * `<`, `>` and `&` are escaped to their JSON `\u` forms. The reason is not tidiness: a `</script>`
 * sequence anywhere inside a string value — a page description quoting HTML, a code sample in a title —
 * closes the block early, and the remainder of the graph is then parsed as HTML. `<` is a valid
 * JSON escape, so every consumer reads the identical string and the sequence cannot occur in the output.
 *
 * Emit with `set:html` and never with `{...}` interpolation, which would HTML-escape the JSON into
 * `&quot;` entities that no `application/ld+json` parser accepts.
 */
export const jsonLdText = (graph: unknown): string =>
  JSON.stringify(graph)
    .replace(/</g, "\\u003c")
    .replace(/>/g, "\\u003e")
    .replace(/&/g, "\\u0026")
