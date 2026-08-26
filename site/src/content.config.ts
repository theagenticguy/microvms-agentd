// SPDX-License-Identifier: Apache-2.0
import { docsLoader } from "@astrojs/starlight/loaders"
import { docsSchema } from "@astrojs/starlight/schema"
import { defineCollection } from "astro:content"

/**
 * The one collection this site has.
 *
 * `docsLoader()` globs `src/content/docs/`, which is generated in full by `scripts/sync-docs.mjs` and
 * gitignored. The loader deletes every store key it did not touch on each run, so nothing may inject an
 * entry before it — which is also why the pages are real files here rather than loader-injected: a
 * synthesized entry has no source file for `starlight-links-validator`'s Markdown pass and no `fileURL`
 * for the Markdown visitors, so its diagrams silently render as code blocks and every link into it
 * reports invalid.
 */
export const collections = {
  docs: defineCollection({ loader: docsLoader(), schema: docsSchema() })
}
