// SPDX-License-Identifier: Apache-2.0
import type { OGImageOptions } from "astro-og-canvas"

import { ICON_PNG_SIZE, PALETTE } from "./mark.ts"

/**
 * The social card, as a pure function of a page.
 *
 * The card is a specification excerpt and not a banner: warm paper, the mark at the head, one ink
 * title, a gray line of abstract, a running foot, and the accent rule bleeding off the bottom edge.
 * Flat fill, no gradient, no photograph and no wordmark; each of those would be the first thing on
 * this site that existed to decorate.
 *
 * `astro-og-canvas` draws exactly two text registers, a title and a description, so the hierarchy has
 * to come from size, weight and color. That is what an RFC's first page does anyway. Ported from
 * memhtml-public's apps/docs/src/branding/og-card.ts.
 */

/** The slug the root page's card is written under. */
export const OG_SLUG_ROOT = "index"

/** The card's rendered size, fixed by `astro-og-canvas`. Stated so the head tags can declare it. */
export const OG_WIDTH = 1200
export const OG_HEIGHT = 630

/**
 * The layout, as measured against the renderer rather than chosen for the look.
 *
 * `astro-og-canvas` (dist/generateOpenGraphImage.js) lays the text out as one paragraph: the title,
 * then two newlines at `padding / 3`, then the description. It places that paragraph no higher than
 * `padding + logo + padding` from the top and does not shrink it, so everything past
 * `OG_HEIGHT - padding` is clipped by the bottom edge. The band the text may fill is therefore
 * `OG_HEIGHT - 3 * padding - logo`, and the worst case the budgets below allow has to fit inside it.
 *
 * The first version of this card carried memhtml's 62px title and 30px abstract at 76px padding. On
 * this site's titles, which run to 76 characters, a three-line title plus a three-line abstract put
 * the running foot under the rule on two of sixty-five cards. The values here were re-derived from
 * the renderer's arithmetic and confirmed by rendering the two longest pages through the library.
 */
export const OG_PADDING = 60
export const TITLE_FONT = { size: 52, lineHeight: 1.1 } as const
export const DESCRIPTION_FONT = { size: 27, lineHeight: 1.3 } as const

/**
 * The most lines each register may take at the budgets below, measured on Tinos at these sizes over
 * the renderer's column of `OG_WIDTH - 3 * OG_PADDING` pixels (1020): bold Tinos advances about
 * 0.43em per character, so 52px fits 45 characters a line and an 84-character title wraps to two or
 * three; regular Tinos advances about 0.40em, so 27px fits 93 a line and a 140-character abstract
 * wraps to two even after losing a twenty-character word to each break.
 */
export const TITLE_LINES = 3
export const DESCRIPTION_LINES = 2
/** The running foot: the tier line and the path line. */
export const FOOT_LINES = 2

/** The vertical band the renderer gives the text block, in pixels. */
export const OG_TEXT_BAND = OG_HEIGHT - 3 * OG_PADDING - ICON_PNG_SIZE

/**
 * The tallest text block the budgets permit: the title lines, the renderer's own two-newline
 * separator at `padding / 3`, the abstract, the blank line before the foot, and the foot.
 */
export const worstCaseTextHeight = (): number =>
  TITLE_LINES * TITLE_FONT.size * TITLE_FONT.lineHeight +
  2 * (OG_PADDING / 3) +
  (DESCRIPTION_LINES + 1 + FOOT_LINES) * DESCRIPTION_FONT.size * DESCRIPTION_FONT.lineHeight

const rgb = (hex: string): [number, number, number] => [
  Number.parseInt(hex.slice(1, 3), 16),
  Number.parseInt(hex.slice(3, 5), 16),
  Number.parseInt(hex.slice(5, 7), 16)
]

/** 5.36:1 on paper: the secondary ink `rfc.css` measured, kept rather than invented for the card. */
export const INK_SECONDARY = "#6B6862"

/** The face family name CanvasKit reads out of the Tinos TTFs. */
const FAMILY = "Tinos"

/** The site's name, as the running foot and the brand line state it. */
const SITE_NAME = "microvms-agentd"

/** The brand line, on the root card only: the three verbs the daemon answers. */
export const BRAND_LINE = `${SITE_NAME} · EXEC · FILES · HEALTH`

/** The three tiers, keyed by the path segment a page id starts with. */
const TIERS: ReadonlyArray<readonly [string, string]> = [
  ["learn/", "Learn"],
  ["reference/", "Reference"],
  ["internals/", "Internals"]
]

const tierLabel = (id: string): string | undefined =>
  TIERS.find(([prefix]) => id === prefix.replace(/\/$/, "") || id.startsWith(prefix))?.[1]

/**
 * The root page's id, normalized.
 *
 * Astro's glob loader ids `src/content/docs/index.md` as `index` (its `/index$` strip needs a leading
 * slash, so the root file keeps its stem), while Starlight's route data has reported it as the empty
 * string in other versions. Both spellings map to `OG_SLUG_ROOT`, so the card file and the `og:image`
 * tag agree whichever the running version hands out. `agent-surface.ts` accepts the same two for the
 * raw Markdown twin, for the same reason.
 */
export const ogId = (id: string): string => (id === "" || id === OG_SLUG_ROOT ? OG_SLUG_ROOT : id)

/**
 * The running foot: the two lines a specification carries at the bottom of every page, what the
 * document is and where in it you are.
 *
 * The brand line appears on the root card only. It is one line and it stops meaning anything if it is
 * stamped onto every card, which is the same reason the cover page states it once.
 */
const runningFoot = (id: string): string => {
  if (id === OG_SLUG_ROOT) return BRAND_LINE
  const tier = tierLabel(id)
  return [tier === undefined ? SITE_NAME : `${SITE_NAME} · ${tier}`, `/${id}/`].join("\n")
}

/**
 * Trim to a word boundary at or under `budget` characters.
 *
 * The renderer does not truncate: text longer than the card runs off the bottom edge and is clipped
 * mid-word, so the budget is enforced here where it can end on a word and say that it did. The two
 * numbers below are what `TITLE_LINES` and `DESCRIPTION_LINES` were measured against.
 */
const clamp = (text: string, budget: number): string => {
  if (text.length <= budget) return text
  const cut = text.slice(0, budget)
  const lastSpace = cut.lastIndexOf(" ")
  return `${(lastSpace > budget * 0.6 ? cut.slice(0, lastSpace) : cut).replace(/[,;:.\s]+$/, "")}…`
}

export const TITLE_BUDGET = 84
export const DESCRIPTION_BUDGET = 140

/** Where the card for a page id is served from, relative to the site base. */
export const ogSlug = (id: string): string => `${ogId(id)}.png`

/** What a reader of the card is told it shows, for `og:image:alt`. */
export const ogAlt = (title: string): string =>
  `A specification cover set on warm paper: “${title}” over a line of abstract, closed by a teal rule.`

export interface OgPage {
  readonly id: string
  readonly title: string
  readonly description?: string | undefined
}

/**
 * Where the mark is read from, relative to the working directory.
 *
 * `astro-og-canvas` reads a logo with `fs.readFile`, so the path is resolved against the process's
 * cwd, which every documented way of building this package sets to the package root. A cwd that fails
 * this assumption fails the build on a missing file rather than shipping a card without the mark, so
 * the assumption is loud.
 */
export const OG_LOGO_PATH = `./public/icon-${ICON_PNG_SIZE}.png`

export const ogCard = ({ id, title, description }: OgPage): OGImageOptions => ({
  title: clamp(title, TITLE_BUDGET),
  description: [
    ...(description === undefined || description === ""
      ? []
      : [clamp(description, DESCRIPTION_BUDGET)]),
    runningFoot(ogId(id))
  ].join("\n\n"),
  bgGradient: [rgb(PALETTE.paper)],
  /*
   * The mark, at its natural size, top-left.
   *
   * It also fixes the card's vertical composition, which is the non-obvious part: with no logo the
   * renderer pins the text block to the top padding edge and leaves half the card as paper, which
   * reads as a card that failed to finish rendering. A logo moves the text's permitted band down by
   * its own height plus one padding, so 48px of mark buys a text block that sits against the accent
   * rule instead of floating above nothing.
   */
  logo: { path: OG_LOGO_PATH, size: [ICON_PNG_SIZE, ICON_PNG_SIZE] },
  // `block-end` strokes the bottom edge and half the width is clipped by it, so 6 renders 6px of
  // teal across the foot: the mark's port, at card scale.
  border: { color: rgb(PALETTE.accent), width: 6, side: "block-end" },
  padding: OG_PADDING,
  font: {
    title: {
      color: rgb(PALETTE.ink),
      size: TITLE_FONT.size,
      lineHeight: TITLE_FONT.lineHeight,
      weight: "Bold",
      families: [FAMILY]
    },
    description: {
      color: rgb(INK_SECONDARY),
      size: DESCRIPTION_FONT.size,
      lineHeight: DESCRIPTION_FONT.lineHeight,
      weight: "Normal",
      families: [FAMILY]
    }
  }
})
