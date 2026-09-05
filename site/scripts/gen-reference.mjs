#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
/**
 * Write the generated Reference tier into the site's content directory.
 *
 * `scripts/sync-docs.mjs` publishes `docs/` and `authored/`; this script publishes the pages that
 * `scripts/reference/pages.mjs` derives from `docs/manifest.json` and `docs/schema.json`. The two share
 * `src/content/docs/reference/`, so ownership has to be explicit:
 *
 * - This script keeps its own manifest, `.reference-manifest.json`, listing every file it wrote. On the
 *   next run it deletes only the files listed there that it no longer generates.
 * - It refuses to overwrite a file it does not own: one that exists and is not in its manifest, or one
 *   that `sync-docs.mjs`'s own manifest claims. A ccu page arriving in the tree at a path this tier
 *   uses is therefore a hard failure that names both producers, rather than a page that flips between
 *   two contents on alternate runs.
 * - It never touches the three ccu pages `reference/cli.md`, `reference/public-api.md` and
 *   `reference/rpc-tools.md`; they belong to the sync.
 *
 * Why real files rather than a content-layer loader, which is how memhtml-public does it: see the
 * comment in `src/content.config.ts`. A loader-injected entry has no source file for the links
 * validator and no `fileURL` for the mdast visitors here.
 *
 * Usage, from `site/`:
 *
 *   node scripts/gen-reference.mjs
 *   node scripts/gen-reference.mjs --dry-run
 */

import { execFileSync } from "node:child_process"
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs"
import { dirname, join, relative, resolve } from "node:path"
import { fileURLToPath } from "node:url"

import { loadManifest, loadSchema, SOURCES } from "./reference/manifest.mjs"
import { referencePages, TIER } from "./reference/pages.mjs"

const SITE_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const REPO_ROOT = resolve(SITE_ROOT, "..")
const CONTENT_DIR = join(SITE_ROOT, "src", "content", "docs")

/** This tool's ownership record, inside the tier's own directory. */
export const OWNERSHIP_MANIFEST = `${TIER}/.reference-manifest.json`

/** The sync's ownership record, read so a path it claims is never written here. */
const SYNC_MANIFEST = ".sync-manifest.json"

/**
 * A YAML double-quoted scalar. Quoted unconditionally so a title with a colon stays one scalar.
 *
 * @param {string} value
 */
const yamlString = (value) => {
  for (const character of value) {
    const point = character.codePointAt(0)
    if (point !== undefined && (point < 0x20 || point === 0x7f)) {
      throw new Error(`value carries a control character: ${JSON.stringify(value)}`)
    }
  }
  return `"${value.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`
}

/**
 * The frontmatter for one page. `editUrl: false` because the file is generated; the page's own
 * Provenance section says where a change belongs.
 *
 * @param {import("./reference/pages.mjs").ReferencePage} page
 * @param {string | undefined} lastUpdated ISO 8601, unquoted so YAML reads it as a timestamp
 */
const frontmatter = (page, lastUpdated) =>
  [
    "---",
    `title: ${yamlString(page.title)}`,
    `description: ${yamlString(page.description)}`,
    ...(lastUpdated === undefined ? [] : [`lastUpdated: ${lastUpdated}`]),
    "editUrl: false",
    "sidebar:",
    `  label: ${yamlString(page.sidebarLabel)}`,
    `  order: ${page.sidebarOrder}`,
    "---",
    ""
  ].join("\n")

/**
 * When the source file last changed, as an ISO 8601 committer date, or nothing when git cannot say.
 * An untracked source (the manifest before its first commit) yields nothing, and absent beats wrong.
 *
 * @param {string} repoRoot
 * @param {string} repoRelativePath
 */
const lastUpdatedAt = (repoRoot, repoRelativePath) => {
  try {
    const out = execFileSync("git", ["log", "-1", "--format=%cI", "--", repoRelativePath], {
      cwd: repoRoot,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"]
    }).trim()
    return out === "" ? undefined : out
  } catch {
    return undefined
  }
}

/**
 * The `owned` list of a manifest file, or none when it is absent or unreadable.
 *
 * @param {string} path
 * @returns {ReadonlyArray<string>}
 */
const ownedIn = (path) => {
  try {
    const parsed = JSON.parse(readFileSync(path, "utf8"))
    return Array.isArray(parsed?.owned) ? parsed.owned.filter((f) => typeof f === "string") : []
  } catch {
    return []
  }
}

/**
 * Generate the tier into `contentDir`.
 *
 * Exported so a test can drive it against a temporary directory with a planted foreign file, and so
 * the refusal is proven rather than described.
 *
 * @param {object} options
 * @param {string} options.contentDir the `src/content/docs` directory to write under
 * @param {import("./reference/manifest.mjs").Manifest} options.manifest
 * @param {import("./reference/manifest.mjs").Schema} options.schema
 * @param {boolean} [options.dryRun]
 * @param {string} [options.repoRoot] where `git log` runs for `lastUpdated`; omit to stamp nothing
 * @returns {{ written: string[], unchanged: string[], removed: string[], pages: ReadonlyArray<import("./reference/pages.mjs").ReferencePage> }}
 */
export const generate = ({ contentDir, manifest, schema, dryRun = false, repoRoot }) => {
  const pages = referencePages(manifest, schema)
  const owned = new Set(ownedIn(join(contentDir, OWNERSHIP_MANIFEST)))
  const foreign = new Set(ownedIn(join(contentDir, SYNC_MANIFEST)))

  const paths = new Set()
  for (const page of pages) {
    if (paths.has(page.path)) throw new Error(`two reference pages share the path ${page.path}`)
    paths.add(page.path)
    if (!page.path.startsWith(`${TIER}/`)) {
      throw new Error(`refusing to write outside ${TIER}/: ${page.path}`)
    }
  }

  const lastUpdated = {
    [SOURCES.manifest]:
      repoRoot === undefined ? undefined : lastUpdatedAt(repoRoot, SOURCES.manifest),
    [SOURCES.schema]: repoRoot === undefined ? undefined : lastUpdatedAt(repoRoot, SOURCES.schema)
  }

  const written = []
  const unchanged = []
  for (const page of pages) {
    const destination = join(contentDir, page.path)
    if (foreign.has(page.path)) {
      throw new Error(
        `refusing to write ${page.path}: scripts/sync-docs.mjs lists it in ${SYNC_MANIFEST}, so a page ` +
          "in docs/ publishes at the same path. Rename one of the two producers' pages."
      )
    }
    if (existsSync(destination) && !owned.has(page.path)) {
      throw new Error(
        `refusing to overwrite ${page.path}: it exists and ${OWNERSHIP_MANIFEST} does not list it, so ` +
          "another producer wrote it. Remove it by hand if it is stale, or rename this tier's page."
      )
    }
    const content = `${frontmatter(page, lastUpdated[page.source])}${page.body}\n`
    const current = existsSync(destination) ? readFileSync(destination, "utf8") : undefined
    if (current === content) {
      unchanged.push(page.path)
      continue
    }
    written.push(page.path)
    if (dryRun) continue
    mkdirSync(dirname(destination), { recursive: true })
    writeFileSync(destination, content)
  }

  // Only a path this tool wrote on a previous run is a prune candidate, and never one the sync claims.
  const removed = [...owned].filter((path) => !paths.has(path) && !foreign.has(path)).sort()
  if (!dryRun) {
    for (const path of removed) rmSync(join(contentDir, path), { force: true })
    mkdirSync(join(contentDir, TIER), { recursive: true })
    writeFileSync(
      join(contentDir, OWNERSHIP_MANIFEST),
      `${JSON.stringify(
        {
          sources: Object.values(SOURCES),
          cli: manifest.data.cli,
          version: manifest.data.version,
          owned: [...paths].sort()
        },
        undefined,
        2
      )}\n`
    )
  }
  return { written, unchanged, removed, pages }
}

const main = () => {
  const argv = process.argv.slice(2)
  const dryRun = argv.includes("--dry-run")
  for (const arg of argv) {
    if (arg !== "--dry-run") {
      process.stderr.write(
        `gen-reference: unknown flag ${arg}\n  usage: gen-reference.mjs [--dry-run]\n`
      )
      process.exit(2)
    }
  }
  const manifest = loadManifest(join(REPO_ROOT, SOURCES.manifest))
  const schema = loadSchema(join(REPO_ROOT, SOURCES.schema))
  const { written, unchanged, removed, pages } = generate({
    contentDir: CONTENT_DIR,
    manifest,
    schema,
    dryRun,
    repoRoot: REPO_ROOT
  })
  const out = process.stdout
  const label = (path) => relative(process.cwd(), join(CONTENT_DIR, path))
  const verb = dryRun ? "would write" : "wrote"
  for (const path of written) out.write(`${verb}  ${label(path)}\n`)
  for (const path of removed) out.write(`${dryRun ? "would remove" : "removed"}  ${label(path)}\n`)
  out.write(
    `${dryRun ? "dry run: " : ""}${pages.length} reference pages from ${manifest.data.cli} ` +
      `${manifest.data.version}, ${written.length} ${verb}, ${unchanged.length} unchanged, ` +
      `${removed.length} removed\n`
  )
}

/*
 * Run only when this file IS the entry point, so a test can import `generate` without writing into
 * the real content directory.
 */
if (
  process.argv[1] !== undefined &&
  import.meta.url === new URL(`file://${process.argv[1]}`).href
) {
  main()
}
