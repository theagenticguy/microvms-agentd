// SPDX-License-Identifier: Apache-2.0
/**
 * Markdown assembly for generated pages.
 *
 * Ported from memhtml-public's `apps/docs/src/loaders/markdown.ts`. Two hazards a hand-written page
 * never hits, because a human sees the result. First, the manifest quotes shell and path fragments
 * such as `<image-name>` and `KEY=VALUE`, and Markdown passes raw HTML through, so an unescaped angle
 * bracket would open an element rather than print one. Escaping happens outside code spans only:
 * inside them the renderer escapes for us, and an `&lt;` written into a code span renders as those five
 * characters. Second, a table cell ends at the first unescaped pipe, including one inside a code span.
 *
 * Headings carry their number in the TEXT, because this site serves raw Markdown to agents and a number
 * injected by CSS or by an AST plugin is absent there. A human citing a section and an agent reading the
 * Markdown would otherwise name different things.
 */

/** A code span, kept intact by `inlineText`. The capture keeps the span in the split output. */
const CODE_SPAN = /(`+[^`]*`+)/

/**
 * A long flag written in bare prose, promoted to a code span.
 *
 * Not cosmetic: this site's Markdown engine applies smart typography, which turns a bare `--stream`
 * into an en dash plus `stream`, a flag a reader cannot copy. A code span is exempt from that
 * transformation and is what a flag should look like anyway.
 */
const BARE_FLAG = /(?<![`\w-])--[a-z][a-z0-9-]*/g

/**
 * A brace outside a code span, escaped the one way both parsers agree on.
 *
 * `starlight-md-txt` runs every page's raw body through `remark-mdx`, where a bare `{` opens a JSX
 * expression and fails the whole raw route; `scripts/brace-gate.mjs` refuses the corpus first. A
 * backslash-escaped brace is literal text to MDX and a plain brace to CommonMark, which is why the
 * gate skips it and why it is used here for the JSON fragments a route summary quotes in prose.
 */
const BARE_BRACE = /(?<!\\)([{}])/g

/**
 * Prose safe to place in a Markdown document, with code spans left intact.
 *
 * @param {string} text
 * @returns {string}
 */
export const inlineText = (text) =>
  text
    .split(CODE_SPAN)
    .map((part, at) =>
      at % 2 === 1
        ? part
        : part
            .replaceAll("&", "&amp;")
            .replaceAll("<", "&lt;")
            .replace(BARE_BRACE, "\\$1")
            .replace(BARE_FLAG, (flag) => `\`${flag}\``)
    )
    .join("")

/**
 * Prose safe to place in one table cell: one line, pipes escaped.
 *
 * @param {string} text
 * @returns {string}
 */
export const cell = (text) => inlineText(text.replace(/\s*\n\s*/g, " ")).replaceAll("|", "\\|")

/**
 * A code span, for a value that must render verbatim. A value carrying a backtick gets a longer fence.
 *
 * @param {string} value
 * @returns {string}
 */
export const code = (value) => {
  const longest = Math.max(0, ...[...value.matchAll(/`+/g)].map((match) => match[0].length))
  const ticks = "`".repeat(longest + 1)
  return longest === 0 ? `${ticks}${value}${ticks}` : `${ticks} ${value} ${ticks}`
}

/**
 * A comma-separated list of code spans, or the given fallback when there are none.
 *
 * @param {ReadonlyArray<string>} values
 * @param {string} [empty]
 * @returns {string}
 */
export const codeList = (values, empty = "none") =>
  values.length === 0 ? empty : values.map(code).join(", ")

/**
 * A Markdown link. Root-relative targets are written with the base EXCLUDED: `starlight-base-path`
 * prefixes the rendered tree and `base-raw-links` prefixes the twins, each exactly once.
 *
 * @param {string} label already-escaped label text
 * @param {string} target
 * @returns {string}
 */
export const link = (label, target) => `[${label}](${target})`

/**
 * A GFM table. Rows are already-escaped cells.
 *
 * @param {ReadonlyArray<string>} headers
 * @param {ReadonlyArray<ReadonlyArray<string>>} rows
 * @returns {string}
 */
export const table = (headers, rows) =>
  [
    `| ${headers.join(" | ")} |`,
    `| ${headers.map(() => "---").join(" | ")} |`,
    ...rows.map((row) => `| ${row.join(" | ")} |`)
  ].join("\n")

const FENCE = "```"

/**
 * A fenced block. The info string is the language, so highlighting is not guessed.
 *
 * @param {string} language
 * @param {string} body
 * @returns {string}
 */
export const fence = (language, body) => [`${FENCE}${language}`, body, FENCE].join("\n")

/**
 * A bullet list.
 *
 * @param {ReadonlyArray<string>} items already-escaped items
 * @returns {string}
 */
export const bullets = (items) => items.map((item) => `- ${item}`).join("\n")

/**
 * A paragraph run, from prose that separates paragraphs with blank lines.
 *
 * A quoted line opening with `#` would become a heading inside a numbered section and take over the
 * table of contents, so it is escaped.
 *
 * @param {string} text
 * @returns {string}
 */
export const paragraphs = (text) =>
  text
    .split(/\n\s*\n/)
    .map((paragraph) => inlineText(paragraph.replace(/\s*\n\s*/g, " ").trim()))
    .filter((paragraph) => paragraph !== "")
    .map((paragraph) => paragraph.replace(/^#/, "\\#"))
    .join("\n\n")

/**
 * The anchor Starlight derives from a heading's text.
 *
 * Astro's `rehypeHeadingIds` and `starlight-links-validator` both run `github-slugger` over the
 * heading's text content, where a code span contributes its text without its backticks. The slugger
 * lowercases, drops everything that is not a letter, a number, a space, a hyphen or an underscore, and
 * turns each space into a hyphen. That is reproduced here for the headings this tier writes and links,
 * which are ASCII prose; the build's own validator is what proves the anchors resolve.
 *
 * @param {string} headingText the heading as written, backticks allowed
 * @returns {string}
 */
export const slug = (headingText) =>
  headingText
    .replaceAll("`", "")
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, "")
    .replaceAll(" ", "-")

/**
 * @typedef {object} Section
 * @property {string} title
 * @property {string} body already-assembled Markdown
 * @property {ReadonlyArray<Section>} [children]
 */

/**
 * @param {Section} section
 * @param {string} number e.g. `1.` or `1.2.`
 * @param {number} depth heading depth, 2 at the top level
 * @returns {string}
 */
const renderSection = (section, number, depth) => {
  const heading = `${"#".repeat(depth)} ${number} ${section.title}`
  const children = (section.children ?? []).map((child, at) =>
    renderSection(child, `${number}${at + 1}.`, depth + 1)
  )
  return [heading, section.body, ...children].filter((part) => part !== "").join("\n\n")
}

/**
 * A page body: RFC-numbered sections, `## 1. Title` at the top level and `### 1.1. Sub` below it.
 *
 * The number lives in the heading text and the anchor is derived from it, so a renumbering changes
 * anchors; `starlight-links-validator` catches the internal links that breaks. An explicit `{#anchor}`
 * would survive a renumbering and cannot be used here: `starlight-md-txt` parses every page's raw body
 * through `remark-mdx`, where a brace opens a JSX expression, and one brace fails the raw route.
 *
 * @param {ReadonlyArray<Section>} list
 * @returns {string}
 */
export const sections = (list) =>
  list.map((section, at) => renderSection(section, `${at + 1}.`, 2)).join("\n\n")

/** The heading text a section renders with, for computing its anchor. */
export const sectionHeading = (number, title) => `${number} ${title}`
