// SPDX-License-Identifier: Apache-2.0
import { mkdirSync, writeFileSync } from "node:fs"
import { dirname, join } from "node:path"
import { fileURLToPath } from "node:url"

import { markArtifacts } from "./mark.ts"

/**
 * Writes every icon under `public/`, from the one declaration of the mark in `mark.ts`.
 *
 * The artifacts are committed rather than generated during the build because `public/` is copied
 * verbatim and a build step that writes into it would race the copy. `pnpm run gen:icons` regenerates
 * them and `tests/branding.test.ts` fails if what is committed no longer draws the current mark, so
 * the binaries cannot drift away from the geometry silently.
 *
 * Run by node directly (Node 22.13 and later strip types without a flag), which is why every import
 * under `branding/` carries its `.ts` extension.
 */

const publicDir = join(dirname(dirname(dirname(fileURLToPath(import.meta.url)))), "public")
mkdirSync(publicDir, { recursive: true })

for (const { file, bytes } of markArtifacts()) {
  writeFileSync(join(publicDir, file), bytes)
  process.stdout.write(`${file} ${bytes.length} bytes\n`)
}
