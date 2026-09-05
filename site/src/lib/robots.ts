// SPDX-License-Identifier: Apache-2.0
import { writeFile } from "node:fs/promises"
import { join } from "node:path"
import type { AstroIntegration } from "astro"

/**
 * The AI-crawler policy, and the constraint that decides whether it can ship at all.
 *
 * RFC 9309 §2.3 puts robots.txt at `/robots.txt` in the top-level path, with the URI
 * `scheme:[//authority]/robots.txt`. Authority is host plus port, so **robots.txt is per-origin and a
 * path-prefixed site cannot own one.** A site served from `https://theagenticguy.github.io/microvms-agentd/`
 * is governed by `https://theagenticguy.github.io/robots.txt`, which belongs to the account and not to
 * this repository; a file emitted at `/microvms-agentd/robots.txt` has no protocol meaning and is dead
 * weight that reads as a policy.
 *
 * So the integration below gates on the base and says so in the build log rather than shipping a file
 * nobody fetches. The policy is written and tested here so that the day this site moves to a root
 * origin — a custom domain, or a user/organization Pages site — it ships without anyone rediscovering
 * the decision.
 */

/**
 * A MISSING robots.txt IS PERMISSIVE, NOT NEUTRAL.
 *
 * RFC 9309 §2.3.1.3: when the status code reports robots.txt unavailable — 400-499 — "the crawler MAY
 * access any resources on the server." Shipping no file therefore chooses the most permissive policy
 * available by not choosing. An explicit `Allow` and a missing file produce identical traffic and are
 * not equivalent artifacts. One can be reviewed.
 *
 * Every vendor fact below was verified against that vendor's own crawler documentation on 2026-08-26.
 * Every hyphen is U+002D: Perplexity's documentation renders `Perplexity-User` in one table cell with
 * U+2011 NON-BREAKING HYPHEN, and a directive copy-pasted from that cell parses as a token no crawler
 * answers to, with nothing anywhere reporting the mismatch.
 *
 * @param sitemap the absolute URL of the sitemap index. Must be absolute: a root-relative path is
 *   ignored, silently.
 */
export const robotsPolicy = (
  sitemap: string
): string => `# The AI-crawler policy for microvms-agentd's documentation.
#
# This corpus is public source documentation for a source-only project. Being read is the reason it
# exists, so every class below is allowed — and allowed EXPLICITLY, because the alternative is the same
# access with no record of the decision. RFC 9309 s2.3.1.3: a crawler that cannot fetch robots.txt "MAY
# access any resources on the server", so an absent file is the most permissive policy available.
#
# HOW A CRAWLER READS THIS, because two of these are near-universally misread:
#
#   * A crawler obeys exactly ONE group: the first whose product token matches it. "User-agent: *" is a
#     fallback used ONLY when no named group matches. A token named below does not also inherit the "*"
#     group's rules, so every rule that applies to it appears inside its own group.
#   * Product-token matching is CASE-INSENSITIVE (s2.2.1); path values are case-SENSITIVE. A token may
#     contain only a-zA-Z_- , which is why a non-ASCII hyphen breaks a directive outright rather than
#     loosening it.
#   * "Sitemap:" is NOT part of RFC 9309 (s2.2.4, "Other Records"). It must be an absolute URL, belongs
#     to no group, and MUST NOT terminate a group.
#
# Propagation after an edit, from each vendor's own documentation: OpenAI, Perplexity, Meta and Amazon
# roughly 24 hours; DuckDuckGo 72 hours; Amazon may act on a cached copy up to 30 days old.


# ===================================================================================================
# CLASS B - user-initiated fetchers. A human pasted this URL or asked a question, and the assistant is
# fetching this one page for them now. This is the exact use case the raw Markdown twins, the llms.txt
# bundles and the "Open in" controls exist for, so blocking it would be this site arguing with itself.
# ===================================================================================================
#
# What an Allow line is worth per vendor, from each vendor's own published statement:
#
#   Claude-User      binding. Anthropic states its bots honor robots.txt, with no carve-out.
#   DuckAssistBot    binding, with a 72-hour lag before a change takes effect.
#   ChatGPT-User     advisory. OpenAI: "Because these actions are initiated by a user, robots.txt rules
#                    may not apply."
#   Perplexity-User  advisory. Perplexity: "this fetcher generally ignores robots.txt rules."
#   Amzn-User        advisory. Amazon: "it may not follow all robots.txt directives."
#
# The advisory ones are listed anyway: a named group with Allow: / is the record that this access is
# intended, so a later reader does not read the fetch in an access log as a violation.
#
# Google's user-triggered fetchers are DELIBERATELY ABSENT. Google publishes them as HTTP user-agent
# strings and gives them no robots.txt token, so a group naming them would be a directive with no
# documented effect - indistinguishable, to a later reader, from one that works.

User-agent: Claude-User
User-agent: ChatGPT-User
User-agent: Perplexity-User
User-agent: DuckAssistBot
User-agent: Amzn-User
User-agent: meta-externalfetcher
Allow: /


# ===================================================================================================
# CLASS A - training, bulk corpus, and AI search indexes. ALLOWED, AS A RECORDED DECISION.
# ===================================================================================================
#
# A model that has read this documentation answers questions about the wire protocol correctly instead
# of inventing an API, which is worth more to this project than the corpus is worth withholding.
#
# Reverse it by replacing Allow: / with Disallow: / in group A1, and say why on the same line. Do that
# when the corpus stops being public documentation. Do NOT extend a block to Class B above: that breaks
# an agent a human explicitly asked to read the page.
#
# The three groups stay separated even though all three allow, because a later decision to permit search
# while declining training needs them already apart.

# A1 - corpus collection that feeds model training.
User-agent: GPTBot
User-agent: ClaudeBot
User-agent: CCBot
User-agent: meta-externalagent
User-agent: Applebot
User-agent: Amazonbot
Allow: /

# A2 - AI search and answer indexes. Each vendor states these do not feed model training; Perplexity
# states PerplexityBot is not used to crawl for foundation models, which is why it is not in A1.
User-agent: OAI-SearchBot
User-agent: Claude-SearchBot
User-agent: PerplexityBot
User-agent: meta-webindexer
User-agent: Amzn-SearchBot
User-agent: Google-CloudVertexBot
Allow: /

# A3 - control-only tokens. These crawlers never fetch: Google states Google-Extended "doesn't have a
# separate HTTP request user agent string", and the token governs whether content already collected by
# the operator's ordinary crawler may be used for AI training and grounding. Disallowing Google-Extended
# affects Gemini training and grounding and explicitly not crawling, Search inclusion, or ranking.
User-agent: Google-Extended
User-agent: Applebot-Extended
Allow: /


# ===================================================================================================
# EVERYTHING ELSE
# ===================================================================================================
#
# Deliberately empty of Disallow lines. Blocking /_astro/ is the classic self-inflicted wound: a crawler
# that renders a page to judge it needs the CSS and JS, and a blocked asset is scored as a broken page.
#
# Two tokens are NOT listed anywhere above, and their absence is the finding rather than an omission.
# "anthropic-ai" and "Claude-Web" appear nowhere in Anthropic's current crawler documentation, which
# lists exactly three tokens (ClaudeBot, Claude-User, Claude-SearchBot). Copy-pasted robots.txt files
# carry them; a rule naming them matches nothing in Anthropic's fleet.

User-agent: *
Allow: /


Sitemap: ${sitemap}

# The machine surfaces are NOT listed above, and cannot be: the sitemap covers page routes only, and
# robots.txt has no registered field for an LLM index. Inventing an "Llms:" field would put a directive
# no parser reads beside five that work. The surfaces are discoverable from every page's <head>
# (rel="alternate" for that page's Markdown twin, rel="index" for llms.txt) and from llms.txt itself.
`

/**
 * Writes the policy to the build output when — and only when — the site owns its origin.
 *
 * @param base the site's base segment, passed in because `astro:build:done` carries no base.
 * @param site the deployed origin, which the `Sitemap:` line is built from.
 */
export const robotsPolicyFile = (base: string, site: URL): AstroIntegration => ({
  name: "docs:robots-policy",
  hooks: {
    "astro:build:done": async ({ dir, logger }) => {
      const segment = base.endsWith("/") ? base : `${base}/`
      if (segment !== "/") {
        logger.warn(
          `no robots.txt emitted: this site is served from ${segment}, and RFC 9309 §2.3 puts ` +
            `robots.txt at the origin root. ${new URL("/robots.txt", site).href} governs this ` +
            "corpus and belongs to the account, not to this repository. The policy in " +
            "`src/lib/robots.ts` ships automatically at a root base."
        )
        return
      }
      const sitemap = new URL("sitemap-index.xml", new URL(segment, site)).href
      await writeFile(join(dir.pathname, "robots.txt"), robotsPolicy(sitemap))
      logger.info(`wrote robots.txt pointing at ${sitemap}`)
    }
  }
})
