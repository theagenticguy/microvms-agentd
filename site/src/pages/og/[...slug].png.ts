// SPDX-License-Identifier: Apache-2.0
import { getCollection } from "astro:content"
import { createRequire } from "node:module"
import { OGImageRoute } from "astro-og-canvas"

import { ogCard, ogId } from "../../branding/og-card.ts"

/**
 * One social card per docs page, rendered to PNG at build time.
 *
 * The route covers every entry of the `docs` collection: the authored pages, the generated Reference
 * pages and the synced tree alike, because `sync-docs.mjs` writes all of them as real files under
 * `src/content/docs/` and the loader sees one flat collection.
 *
 * The `.png` belongs to the FILE NAME and not to the slug, which is forced by `trailingSlash:
 * "always"` plus `build.format: "directory"`: a route pattern of `/og/[...slug]` is a page path as far
 * as Astro is concerned, so it gets a trailing slash appended and the card is served from `/og/x.png/`,
 * a directory whose name ends in `.png`. Putting the extension in the pattern makes the route a file,
 * and `getSlug` returns the bare id so the two halves compose to exactly `ogSlug`'s answer.
 *
 * The two faces are resolved from `node_modules` rather than fetched. `astro-og-canvas` defaults to
 * downloading Noto Sans from a CDN on first render, which would make a card's typography depend on a
 * network round-trip during the build and put a face on the card that appears nowhere on the site.
 * Tinos is metric-compatible with Times New Roman to four decimal places, so the card is set in the
 * same face `--sl-font` asks for first. Ported from memhtml-public's apps/docs/src/pages/og route.
 */

const require = createRequire(import.meta.url)

const FONTS = [
  require.resolve("@expo-google-fonts/tinos/400Regular/Tinos_400Regular.ttf"),
  require.resolve("@expo-google-fonts/tinos/700Bold/Tinos_700Bold.ttf")
]

const entries = await getCollection("docs")

/**
 * The root page is keyed by `ogId`, which is the one place its two possible ids (`index` from the glob
 * loader, the empty string from older route data) are reconciled. `Head.astro` reads the same function
 * so the tag and the file agree.
 */
const pages = Object.fromEntries(entries.map((entry) => [ogId(entry.id), entry.data]))

export const { getStaticPaths, GET } = await OGImageRoute({
  pages,
  getSlug: (path) => path,
  getImageOptions: (path, page) => ({
    ...ogCard({ id: path, title: page.title, description: page.description }),
    fonts: FONTS
  })
})
