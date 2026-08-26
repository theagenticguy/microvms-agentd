// SPDX-License-Identifier: Apache-2.0
/**
 * The URLs behind the page-action controls and the `<head>` discovery block.
 *
 * This module is pure and takes the site's origin and base segment as arguments rather than reading
 * `import.meta.env`, so every URL it emits is asserted in a unit test rather than only in a browser.
 *
 * Every URL here is built with `new URL()`. A base segment joined to a path by string concatenation
 * produces `//microvms-agentd/index.md` — a protocol-relative URL naming a *host* called
 * `microvms-agentd` — and the failure is silent: the href parses, resolves to nothing, and looks
 * right in the source.
 */

/** The corpus name a prompt uses to tell an assistant what it is reading. */
export const DOCS_NAME = "microvms-agentd"

/**
 * The character ceiling on a shipped deep-link href.
 *
 * Failures above roughly 8 KB are silent: the HTTP/2 `:path` pseudo-header shares its HPACK budget
 * with cookies, so an oversized prompt URL is dropped or truncated with no error anywhere the author
 * can see it. The ceiling is on the whole href rather than on the prompt, because percent-encoding
 * roughly doubles Markdown and it is the encoded path that the budget applies to.
 */
export const DEEP_LINK_BUDGET = 7_500

/** A target that accepts a prefilled prompt in a URL. */
export interface DeepLinkTarget {
  readonly id: string
  /**
   * "Open in …", never "Ask …". Every one of these links is prefill-only by deliberate design —
   * ChatGPT gates auto-submission on `sec-fetch-site` — so a label promising an answer describes
   * behavior the link does not have.
   */
  readonly label: string
  /** The origin and path, with no query. `new URL()` is applied to this, never `+`. */
  readonly endpoint: string
  /** The query key that carries the prompt. */
  readonly parameter: string
  /** Whether the vendor documents this parameter, or it is verified working but undocumented. */
  readonly warrant: "vendor-documented" | "verified-undocumented"
  /** The vendor's own stated ceiling, where one is published. */
  readonly vendorLimit: number | undefined
}

/**
 * The shipped targets, verified 2026-08-12.
 *
 * Codex has **no web prompt parameter** — confirmed absent, not merely undocumented — so no Codex
 * control is shipped. `codex://new?prompt=` exists but is a desktop scheme: it does nothing on a
 * machine without the app installed, and a control that silently does nothing is the failure this
 * table exists to prevent.
 *
 * `claude.ai/code` also accepts `prompt_url=` for payloads too long to inline. It is unused because
 * the ceiling below already replaces an oversized payload with the page's own raw-Markdown URL, which
 * is the same remedy without a second URL shape to keep verified.
 */
export const DEEP_LINK_TARGETS: ReadonlyArray<DeepLinkTarget> = [
  {
    id: "chatgpt",
    label: "Open in ChatGPT",
    endpoint: "https://chatgpt.com/",
    parameter: "q",
    warrant: "verified-undocumented",
    vendorLimit: undefined
  },
  {
    id: "claude",
    label: "Open in Claude",
    endpoint: "https://claude.ai/new",
    parameter: "q",
    warrant: "verified-undocumented",
    vendorLimit: undefined
  },
  {
    id: "claude-code",
    label: "Open in Claude Code",
    endpoint: "https://claude.ai/code",
    parameter: "prompt",
    warrant: "vendor-documented",
    vendorLimit: undefined
  },
  {
    id: "cursor",
    label: "Open in Cursor",
    endpoint: "https://cursor.com/link/prompt",
    parameter: "text",
    warrant: "vendor-documented",
    vendorLimit: 8_000
  }
]

/**
 * Where the site is published.
 *
 * `site` is the origin with the base segment EXCLUDED, which is what `Astro.site` holds; `base`
 * includes it, which is what `import.meta.env.BASE_URL` holds. Confusing the two is the whole reason
 * both are named here instead of one being derived from the other.
 */
export interface SiteContext {
  readonly site: URL
  readonly base: string
}

const withTrailingSlash = (value: string): string => (value.endsWith("/") ? value : `${value}/`)

/** An absolute URL for a path under the site's base segment. */
export const siteUrl = (relative: string, context: SiteContext): URL =>
  new URL(relative.replace(/^\/+/, ""), new URL(withTrailingSlash(context.base), context.site))

/**
 * The raw-Markdown route for a docs entry.
 *
 * `starlight-md-txt` injects `/[...slug].md` and maps the root entry — whose id is the empty string
 * or `index` — to an undefined slug, so the site root's raw route is `<base>/.md`. Reproducing that
 * mapping here rather than guessing is what keeps this control off a 404.
 */
export const rawMarkdownUrl = (entryId: string, context: SiteContext): URL =>
  siteUrl(`${entryId === "index" ? "" : entryId}.md`, context)

/** One page, as a machine reader is told about it. */
export interface PageReference {
  readonly title: string
  readonly pageUrl: string
  readonly markdownUrl: string
}

/**
 * The prompt that names the page without carrying it.
 *
 * This is what ships when the page's own Markdown will not fit the budget, and it is not a
 * degradation so much as a redirection: every target here can fetch a URL.
 */
export const referencePrompt = (page: PageReference): string =>
  [
    `Read this page of the ${DOCS_NAME} documentation, then answer questions about it.`,
    "Fetch the raw Markdown rather than scraping the HTML.",
    "",
    `Title: ${page.title}`,
    `Page: ${page.pageUrl}`,
    `Raw Markdown: ${page.markdownUrl}`
  ].join("\n")

/** The prompt that carries the page's Markdown inline, for a page small enough to fit. */
export const contentPrompt = (page: PageReference, body: string): string =>
  [referencePrompt(page), "", "---", "", body].join("\n")

/** A control that is actually rendered, with the href it will carry. */
export interface DeepLink {
  readonly target: DeepLinkTarget
  readonly href: string
  /** Whether the payload is the page's Markdown, or only its URL. */
  readonly carriesContent: boolean
}

const promptUrl = (target: DeepLinkTarget, prompt: string): string => {
  const url = new URL(target.endpoint)
  url.searchParams.set(target.parameter, prompt)
  return url.href
}

/**
 * One target's href, carrying the page's Markdown when it fits and the page's URL when it does not.
 *
 * An overflowing reference prompt throws rather than shipping: a truncated prompt URL is the dead
 * button this whole module exists to prevent, and it cannot be seen by looking at the page.
 */
export const deepLink = (target: DeepLinkTarget, page: PageReference, body: string): DeepLink => {
  const ceiling = Math.min(DEEP_LINK_BUDGET, target.vendorLimit ?? DEEP_LINK_BUDGET)
  const trimmed = body.trim()
  if (trimmed !== "") {
    const href = promptUrl(target, contentPrompt(page, trimmed))
    if (href.length <= ceiling) return { target, href, carriesContent: true }
  }
  const href = promptUrl(target, referencePrompt(page))
  if (href.length > ceiling) {
    throw new Error(
      `${target.id}: the reference prompt alone is ${href.length} characters, over the ${ceiling} ceiling`
    )
  }
  return { target, href, carriesContent: false }
}

/** Every shipped control for one page. */
export const deepLinks = (page: PageReference, body: string): ReadonlyArray<DeepLink> =>
  DEEP_LINK_TARGETS.map((target) => deepLink(target, page, body))

/** One `<link>` in the `<head>` discovery block. */
export interface DiscoveryLink {
  readonly rel: string
  readonly type: string
  readonly href: string
  readonly title: string | undefined
  /** Whether the relation type has external warrant, or is this site's own invention. */
  readonly warrant: "convention" | "invention"
}

/**
 * What a machine reader is pointed at from every page.
 *
 * Starlight already emits base-correct `canonical` and `sitemap`, so this is only the delta. The
 * `warrant` field is carried in the data rather than in a comment because the honesty is the point:
 * two of these relations are what several vendors ship, and the third is this site's own.
 */
export const discoveryLinks = (
  entryId: string,
  context: SiteContext
): ReadonlyArray<DiscoveryLink> => [
  {
    // Convention: shipped by Anthropic, Bun, Turso, Neon, Vercel, Cloudflare, Hono and GitHub Docs.
    // `text/markdown` is registered by RFC 7763.
    rel: "alternate",
    type: "text/markdown",
    href: rawMarkdownUrl(entryId, context).href,
    title: "Markdown source of this page",
    warrant: "convention"
  },
  {
    // Convention: `index` is a registered link relation ("Refers to an index") and is GitHub Docs'
    // own spelling for llms.txt. Preferred over `llms-txt`, which is unregistered and appears only
    // in HTTP `Link:` headers — a channel a static host gives no control over.
    rel: "index",
    type: "text/markdown",
    href: siteUrl("llms.txt", context).href,
    title: "llmstxt.org index",
    warrant: "convention"
  },
  {
    // THIS SITE'S OWN. `llms-full-txt` has no registration and no other adopter; it is named here
    // so that a later reader does not mistake it for a standard they failed to find.
    rel: "llms-full-txt",
    type: "text/markdown",
    href: siteUrl("llms-full.txt", context).href,
    title: undefined,
    warrant: "invention"
  }
]
