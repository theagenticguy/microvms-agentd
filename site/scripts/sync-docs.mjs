#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
/**
 * Publish `docs/` as Starlight content: copy every Markdown page into a generated content directory,
 * stamp the frontmatter Starlight requires, and write the manifest the build reads back.
 *
 * ## The tree is input. This script never writes into it.
 *
 * Two facts leave no other arrangement:
 *
 * - Starlight's `docsSchema()` declares `title: z.string()` — required, no default, and no code path
 *   reading the body's first heading. A page without `title` fails the build.
 * - `docs/` is a comprehensive-codebase-understanding tree, and ccu forbids frontmatter on its outputs:
 *   its cross-link pass strips any YAML frontmatter it finds there. Frontmatter written into the tree
 *   self-reverts on the next ccu run, and the site then fails its build for a reason living in another
 *   tool's output directory.
 *
 * So the title is derived here, written onto the copy, and the tree stays read-only. This script opens
 * no file under the source for writing and refuses to run at all when the target is inside the source.
 *
 * ## The target is generated, and gitignored
 *
 * Commit the target and a reader sees ordinary editable pages, edits one, and the next sync overwrites
 * the edit with a fresh copy — a silent discard whose git history reads as a deliberate revert. The
 * authored pages therefore live in `authored/` and are copied in from there, so every file under the
 * target has exactly one producer.
 *
 * Usage, from `site/`:
 *
 *   node scripts/sync-docs.mjs
 *   node scripts/sync-docs.mjs --dry-run
 */

import { execFileSync } from "node:child_process"
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync
} from "node:fs"
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path"

const SOURCE = "../docs"
const TARGET = "src/content/docs"
const AUTHORED = "authored"
const REPO_ROOT = ".."
const REPO_URL = "https://github.com/theagenticguy/microvms-agentd"

/** The manifest of files this tool owns in the target, so a prune can never reach a foreign page. */
const MANIFEST = ".sync-manifest.json"

/**
 * ccu's reading order for its six category directories, offset past the root pages.
 *
 * Starlight computes a sidebar group's sort weight as the MINIMUM `sidebar.order` of the routes it
 * contains, so one rank stamped on every page in a category is what orders the six GROUPS. Pages inside
 * a category share that rank, tie, and fall back to Starlight's collator — alphabetical within a
 * category, ccu's order across them. Without this the groups sort alphabetically and `analysis` opens
 * the book.
 */
const CATEGORY_ORDER = new Map([
  ["architecture", 10],
  ["reference", 11],
  ["behavior", 12],
  ["analysis", 13],
  ["diagrams", 14],
  ["insights", 15]
])

/**
 * The hand-authored documents at the tree root, each named rather than globbed.
 *
 * These predate the generated tree and win any disagreement with it, so they sort ahead of the six
 * categories. Their H1s are full sentences — good page titles, too long for a sidebar rail — so each
 * one carries a short `label` and keeps its H1 as the title.
 *
 * Naming them individually is the contract: a new uppercase document appearing in the tree is still
 * published, with its H1 as both title and label, and is reported at the end of the run so the omission
 * is visible rather than silently formatted.
 */
const AUTHORITATIVE = new Map([
  ["PLATFORM.md", { label: "Platform", order: 2 }],
  ["PROTOCOL.md", { label: "Protocol", order: 3 }],
  ["TRUST.md", { label: "Trust", order: 4 }],
  ["EMBEDDING.md", { label: "Embedding", order: 5 }],
  ["STRATEGY.md", { label: "Strategy", order: 6 }],
  ["HARNESS-CAPABILITIES.md", { label: "Harness capabilities", order: 7 }],
  ["CLI-COVERAGE-PLAN.md", { label: "CLI coverage plan", order: 8 }]
])

/** Anything else at the tree root: published, ranked after the named documents, before the categories. */
const ROOT_ORDER = 9

/**
 * Pages in the tree that the site deliberately does not publish, each with the reason.
 *
 * `README.md` is the tree's own table of contents. A sidebar is that, generated, so publishing it ships
 * two navigation surfaces that disagree the moment one is regenerated. Its standing claim — that the
 * hand-written documents win over the generated ones — is authored into `authored/index.md`, where it is
 * the landing page's first section rather than a link list.
 */
const UNPUBLISHED = new Map([["README.md", "the sidebar is this file, generated"]])

/** Non-Markdown files copied to `public/` so a machine reader can fetch them at a stable URL. */
const ASSETS = new Map([["schema.json", "public/schema.json"]])

/**
 * ccu writes its H1 as `identifier · Title`. The separator is U+00B7 MIDDLE DOT surrounded by single
 * spaces, spelled as an escape so no homoglyph can pass for it in this file.
 */
const H1_SEPARATOR = " · "

/**
 * Directories holding build scratch rather than pages: `.packets/` carries the per-agent task packets,
 * `.repomix/` the flattened pack. Both are gitignored, so a page built from one would cite files a
 * reader cannot open. Every dot-directory is skipped, which also keeps a `.git` out of the copy.
 */
const isSkippedDirectory = (name) => name.startsWith(".")

const fail = (message) => {
  process.stderr.write(`sync-docs: ${message}\n`)
  process.exit(2)
}

/**
 * A resolved path with every symlink on its existing prefix collapsed.
 *
 * The target need not exist yet, so the walk stops at the deepest ancestor that does and re-appends the
 * rest. Comparing unresolved paths lets a symlinked target sit inside the source undetected, which is
 * the one arrangement that writes into the tree.
 */
const realish = (path) => {
  let head = resolve(path)
  const tail = []
  while (!existsSync(head)) {
    const parent = dirname(head)
    if (parent === head) return resolve(path)
    tail.unshift(basename(head))
    head = parent
  }
  return join(realpathSync(head), ...tail)
}

/** Whether `child` is `parent` or sits beneath it. */
const contains = (parent, child) => {
  const rel = relative(parent, child)
  return rel === "" || (rel !== ".." && !rel.startsWith(`..${sep}`) && !isAbsolute(rel))
}

/** Every `.md` file under `root`, as tree-relative slash-separated paths, in path order. */
const markdownUnder = (root) => {
  const walk = (dir) =>
    readdirSync(join(root, dir), { withFileTypes: true }).flatMap((entry) => {
      const path = dir === "" ? entry.name : `${dir}/${entry.name}`
      if (entry.isDirectory()) return isSkippedDirectory(entry.name) ? [] : walk(path)
      return entry.isFile() && entry.name.endsWith(".md") ? [path] : []
    })
  return walk("").sort()
}

/** A fenced block, opener through the matching closer of the same run. */
const FENCED = /^ {0,3}(`{3,}|~{3,})[^\n]*\n[\s\S]*?^ {0,3}\1[^\n]*$/gm

/**
 * The document's H1, and the body with that H1 removed.
 *
 * The H1 is removed because it becomes `title`, and Starlight renders `title` as the page's `<h1>`.
 * Leaving it in the body ships two `<h1>` elements per page saying nearly the same thing — one from the
 * frontmatter and one from the prose — which is a heading structure no reader benefits from and which no
 * build step complains about.
 *
 * Fences are blanked before the scan because a `# ` on the first line of a shell block is a comment, not
 * a heading, and a file opening with such a fence would otherwise take its title from it.
 */
const splitHeading = (body) => {
  const masked = body.replace(FENCED, (match) => match.replace(/[^\n]/g, " "))
  const lines = body.split("\n")
  const at = masked.split("\n").findIndex((line) => /^#[ \t]+\S/.test(line))
  if (at === -1) return { heading: undefined, body }
  const heading = /^#[ \t]+(\S.*?)[ \t]*$/.exec(lines[at] ?? "")?.[1]
  const rest = lines.slice(at + 1)
  while (rest.length > 0 && (rest[0] ?? "").trim() === "") rest.shift()
  return { heading, body: rest.join("\n") }
}

/** How long a derived description may be before it is cut at a word boundary. */
const DESCRIPTION_LIMIT = 180

/**
 * The page's own first sentence, as a meta description.
 *
 * Taken from the body rather than written by hand, because a hand-written description per page is one
 * more claim nobody re-reads. Inline code markers and link syntax are removed: the value lands in a
 * `<meta>` attribute and in the JSON-LD graph, where backticks and bracket pairs are noise a consumer
 * cannot use.
 *
 * Returns `undefined` when the first block is not prose — a table, a list, a fence, a directive.
 * `description` is optional in Starlight's schema, and an absent one is read as unknown while a wrong
 * one is read as asserted.
 */
const descriptionOf = (body) => {
  const block = body
    .replace(FENCED, "")
    .split(/\n[ \t]*\n/)
    .map((paragraph) => paragraph.trim())
    .find((paragraph) => paragraph !== "")
  if (block === undefined) return undefined
  if (/^(?:[#>|*+-]|\d+\.|:::)/.test(block)) return undefined
  const flat = block
    .replace(/\s*\n\s*/g, " ")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/`+/g, "")
    .replace(/\s+/g, " ")
    .trim()
  if (flat === "") return undefined
  if (flat.length <= DESCRIPTION_LIMIT) return flat
  const cut = flat.slice(0, DESCRIPTION_LIMIT)
  const boundary = cut.lastIndexOf(" ")
  return `${(boundary === -1 ? cut : cut.slice(0, boundary)).replace(/[,;:.—-]$/, "")}…`
}

/**
 * The page title, and the reason it fell back when it did.
 *
 * Two sources and never a third: the Title segment of a ccu-shaped H1, or the whole H1 for a
 * hand-authored document that carries no separator. A missing H1 is reported rather than dressed up —
 * an invented title is a claim about the document that nothing checks, and the fix (write the H1 ccu's
 * way) belongs to whoever owns the tree.
 */
const titleOf = (heading, treePath) => {
  const stem = basename(treePath, ".md")
  if (heading === undefined) return { title: stem, fallback: "no H1" }
  const at = heading.lastIndexOf(H1_SEPARATOR)
  if (at === -1) return { title: heading, fallback: undefined }
  const title = heading.slice(at + H1_SEPARATOR.length).trim()
  if (title === "") return { title: stem, fallback: `H1 has an empty title: ${heading}` }
  return { title, fallback: undefined }
}

/** The `sidebar.order` and label override for a tree-relative path. */
const placementOf = (treePath) => {
  const segments = treePath.split("/")
  if (segments.length > 1) return { order: CATEGORY_ORDER.get(segments[0]) }
  const named = AUTHORITATIVE.get(treePath)
  return named === undefined ? { order: ROOT_ORDER } : { order: named.order, label: named.label }
}

/**
 * A YAML double-quoted scalar.
 *
 * Quoted unconditionally, so a title carrying a colon stays one scalar instead of reparsing as a nested
 * mapping: `AWS Lambda MicroVMs: measured platform behavior` is one of this tree's own H1s. Compared by
 * code point rather than through a character class, because a control character written into a regex is
 * itself the kind of thing a linter rejects.
 */
const yamlString = (value) => {
  for (const character of value) {
    const code = character.codePointAt(0)
    if (code !== undefined && (code < 0x20 || code === 0x7f)) {
      throw new Error(`value carries a control character: ${JSON.stringify(value)}`)
    }
  }
  return `"${value.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`
}

/**
 * The frontmatter block for one page.
 *
 * `editUrl: false` because the file at this path is a copy. An edit link pointing at it invites the edit
 * the next sync discards; the tree is where a change belongs.
 */
const frontmatter = ({ title, description, order, label, lastUpdated }) =>
  [
    "---",
    `title: ${yamlString(title)}`,
    ...(description === undefined ? [] : [`description: ${yamlString(description)}`]),
    /*
     * Unquoted on purpose. YAML parses a bare ISO 8601 timestamp as a timestamp, which is what
     * Starlight's `lastUpdated: z.date()` accepts; quoting it makes a string that fails the schema.
     */
    ...(lastUpdated === undefined ? [] : [`lastUpdated: ${lastUpdated}`]),
    "editUrl: false",
    ...(order === undefined && label === undefined
      ? []
      : [
          "sidebar:",
          ...(label === undefined ? [] : [`  label: ${yamlString(label)}`]),
          ...(order === undefined ? [] : [`  order: ${order}`])
        ]),
    "---",
    ""
  ].join("\n")

/** Every path tracked at `commit`, from one `git ls-tree`. */
const trackedAt = (repoRoot, commit) =>
  new Set(
    execFileSync("git", ["ls-tree", "-r", "--name-only", commit], {
      cwd: repoRoot,
      encoding: "utf8",
      maxBuffer: 1 << 26
    })
      .split("\n")
      .filter((line) => line !== "")
  )

/**
 * When the SOURCE file last changed, as an ISO 8601 committer date.
 *
 * Starlight reads `lastUpdated` from git by default, and here that would report nothing: every file in
 * the content directory is generated and untracked, so git has no history for the path Starlight asks
 * about. Stamping the source file's date is the only honest value available — it describes the document
 * a reader would edit rather than the copy the build made.
 *
 * Empty on a shallow clone that does not reach the commit that touched the file, which is why the deploy
 * workflow checks out with `fetch-depth: 0`. Absent beats wrong: a date every page shares is a claim
 * that every page changed together.
 */
const lastUpdatedAt = (repoRoot, repoRelativePath) => {
  const out = execFileSync("git", ["log", "-1", "--format=%cI", "--", repoRelativePath], {
    cwd: repoRoot,
    encoding: "utf8"
  }).trim()
  return out === "" ? undefined : out
}

/** `a/b/../c` -> `a/c`, on slash-separated paths, with no filesystem access. */
const normalizeSlashes = (path) => {
  const out = []
  for (const segment of path.split("/")) {
    if (segment === "" || segment === ".") continue
    if (segment === "..") out.pop()
    else out.push(segment)
  }
  return out.join("/")
}

/**
 * Rewrites a relative link that leaves the published corpus into a commit-pinned repository permalink.
 *
 * A relative `.md` link between two published pages is left exactly as written, and that is the whole
 * reason this function is small. Astro resolves such a link against the entry's own path, so it is
 * correct on the rendered page; and it is correct on the raw Markdown twin too, because a relative
 * target resolves against the twin's own directory, which already carries the base segment. Relative is
 * the one link form that needs no rewriting on either surface.
 *
 * Everything else is a 404 waiting to happen: a link to a repository file the site does not serve, or to
 * a page it does not publish. Those become permalinks, pinned to a SHA for the same reason the citations
 * are — a blob link against a moving branch rots the day the file moves.
 */
const rewriteEscapingLinks = (body, treePath, { published, commit, tracked, treePrefix, report }) => {
  const dir = dirname(treePath) === "." ? "" : dirname(treePath)
  return body.replace(/\]\(([^)\s]+)\)/g, (match, target) => {
    if (/^[a-z][a-z0-9+.-]*:/i.test(target) || target.startsWith("/") || target.startsWith("#")) {
      return match
    }
    const [path, fragment] = target.split("#", 2)
    const treeTarget = normalizeSlashes(dir === "" ? path : `${dir}/${path}`)
    if (published.has(treeTarget)) return match
    const repoTarget = normalizeSlashes(`${treePrefix}/${dir === "" ? path : `${dir}/${path}`}`)
    if (!tracked.has(repoTarget)) {
      throw new Error(
        `${treePath}: the link \`${target}\` resolves to ${repoTarget}, which the site does not ` +
          `publish and git does not track at ${commit}. Fix the link in the tree, or publish the page.`
      )
    }
    report(`${treePath} -> ${repoTarget}`)
    const anchor = fragment === undefined ? "" : `#${fragment}`
    return `](${REPO_URL}/blob/${commit}/${repoTarget}${anchor})`
  })
}

/** The paths a previous run wrote, or none when the target holds no manifest. */
const readManifest = (target) => {
  try {
    const parsed = JSON.parse(readFileSync(join(target, MANIFEST), "utf8"))
    return Array.isArray(parsed?.owned) ? parsed.owned.filter((f) => typeof f === "string") : []
  } catch {
    return []
  }
}

/** Where the read-next table is substituted into the authored agent page. */
const READ_NEXT_MARKER = "<!--READ-NEXT-->"

/**
 * The read-next table for the agent page: one row per published page, that page's URL beside its twin's.
 *
 * Built from the same list the sidebar is built from, so a page added to the tree appears here without an
 * edit and a removed page cannot leave a row behind. The links are root-relative because both surfaces
 * prefix them: `starlight-base-path` rewrites the rendered tree, `base-raw-links` rewrites the twins.
 */
const readNextTable = (pages) =>
  [
    "| Read this | Raw Markdown |",
    "| --------- | ------------ |",
    ...pages.map(({ id, title }) => `| [${title}](/${id}/) | [\`${id}.md\`](/${id}.md) |`)
  ].join("\n")

const main = () => {
  const argv = process.argv.slice(2)
  const dryRun = argv.includes("--dry-run")
  for (const arg of argv) {
    if (arg !== "--dry-run") fail(`unknown flag ${arg}\n  usage: sync-docs.mjs [--dry-run]`)
  }

  const source = realish(SOURCE)
  const target = realish(TARGET)
  const authored = realish(AUTHORED)
  const repoRoot = realish(REPO_ROOT)
  if (!existsSync(source)) fail(`no such source directory: ${source}`)
  if (!existsSync(authored)) fail(`no such authored directory: ${authored}`)

  // Refused in both directions. A target inside the source writes pages into the tree, which the next
  // ccu run then documents as if they were source. A source inside the target puts the tree in the
  // prune's reach.
  if (contains(source, target)) {
    fail(`refusing to write into the source tree: ${target} is inside ${source}`)
  }
  if (contains(target, source)) {
    fail(`refusing to run: ${source} is inside ${target}, where the prune reaches it`)
  }

  /*
   * The SHA every permalink is pinned to. `DOCS_COMMIT` is what CI passes, because the runner's
   * `github.sha` on a pull-request checkout is the commit under review while `git rev-parse HEAD` in that
   * workspace is a synthetic merge commit that exists on no branch — a permalink to it 404s once the
   * check is garbage-collected.
   */
  const commit =
    process.env.DOCS_COMMIT ??
    execFileSync("git", ["rev-parse", "HEAD"], { cwd: repoRoot, encoding: "utf8" }).trim()
  if (!/^[0-9a-f]{40}$/.test(commit)) {
    fail(`DOCS_COMMIT is not a full 40-character SHA: ${JSON.stringify(commit)}`)
  }
  const tracked = trackedAt(repoRoot, commit)
  /** The tree's own path inside the repository, so nothing below spells `docs/` as a literal. */
  const treePrefix = relative(repoRoot, source).split(sep).join("/")

  const treePages = markdownUnder(source)
  if (treePages.length === 0) fail(`no .md files under ${source}`)
  const published = new Set(treePages.filter((treePath) => !UNPUBLISHED.has(treePath)))

  const owned = new Set(readManifest(target))
  const generated = new Set()
  const written = []
  const unchanged = []
  const fallbacks = []
  const unnamed = []
  const rewrittenLinks = []
  const pages = []

  const emit = (contentPath, content) => {
    generated.add(contentPath)
    const destination = join(target, contentPath)
    const current = existsSync(destination) ? readFileSync(destination, "utf8") : undefined
    if (current === content) {
      unchanged.push(contentPath)
      return
    }
    written.push(contentPath)
    if (dryRun) return
    mkdirSync(dirname(destination), { recursive: true })
    writeFileSync(destination, content)
  }

  for (const treePath of treePages) {
    if (UNPUBLISHED.has(treePath)) continue
    const raw = readFileSync(join(source, treePath), "utf8")
    // A source file carrying frontmatter is an upstream contract break, not something to paper over:
    // ccu forbids frontmatter and strips it, so prepending here stacks two blocks on this run and
    // produces a different file on the run after the strip. Name it and stop.
    if (/^---\r?\n/.test(raw)) {
      fail(
        `${treePath} opens with YAML frontmatter, which a ccu output never carries.\n` +
          "  Stamping a second block would leave the page with two. Remove it from the tree and re-run."
      )
    }

    const { heading, body } = splitHeading(raw)
    const { title, fallback } = titleOf(heading, treePath)
    if (fallback !== undefined) fallbacks.push({ treePath, title, fallback })
    const placement = placementOf(treePath)
    if (!treePath.includes("/") && !AUTHORITATIVE.has(treePath)) unnamed.push(treePath)

    const linked = rewriteEscapingLinks(body, treePath, {
      published,
      commit,
      tracked,
      treePrefix,
      report: (entry) => rewrittenLinks.push(entry)
    })

    pages.push({
      treePath,
      id: treePath.slice(0, -".md".length).toLowerCase(),
      title,
      order: placement.order
    })
    emit(
      treePath,
      `${frontmatter({
        ...placement,
        title,
        description: descriptionOf(linked),
        lastUpdated: lastUpdatedAt(repoRoot, `${treePrefix}/${treePath}`)
      })}${linked}`
    )
  }

  /*
   * The authored pages pass through verbatim. They carry their own frontmatter — they are authored, so
   * nothing strips it — and the agent page additionally gets the derived read-next table substituted for
   * its marker, which is why it is generated into the target rather than living there.
   */
  const readNextRows = pages
    .slice()
    .sort((a, b) => (a.order ?? 0) - (b.order ?? 0) || a.id.localeCompare(b.id))
  for (const name of readdirSync(authored).sort()) {
    if (!name.endsWith(".md")) continue
    const body = readFileSync(join(authored, name), "utf8")
    if (name === "agents.md" && !body.includes(READ_NEXT_MARKER)) {
      fail(`authored/agents.md carries no ${READ_NEXT_MARKER}: the read-next table has nowhere to go`)
    }
    emit(name, body.replace(READ_NEXT_MARKER, readNextTable(readNextRows)))
  }

  for (const [treeAsset, destination] of ASSETS) {
    const from = join(source, treeAsset)
    if (!existsSync(from)) fail(`no such asset in the tree: ${treeAsset}`)
    if (dryRun) continue
    mkdirSync(dirname(destination), { recursive: true })
    copyFileSync(from, destination)
  }

  /*
   * The sidebar's own group for the hand-authored tier, derived rather than restated in `astro.config.ts`.
   * A new root page therefore reaches the rail on the run that publishes it: listing it in the config by
   * hand is how a published page becomes an orphan nothing links to.
   */
  const sidebar = [...AUTHORITATIVE]
    .filter(([treePath]) => published.has(treePath))
    .sort(([, a], [, b]) => a.order - b.order)
    .map(([treePath, named]) => ({
      label: named.label,
      link: `/${treePath.slice(0, -".md".length).toLowerCase()}/`
    }))
    .concat(
      unnamed.slice().sort().map((treePath) => {
        const id = treePath.slice(0, -".md".length).toLowerCase()
        return { label: pages.find((page) => page.id === id)?.title ?? id, link: `/${id}/` }
      })
    )

  // Only a path this tool wrote on a previous run is a prune candidate, so a hand-authored page sharing
  // the target survives and a first run against a populated directory removes nothing.
  const stale = [...owned].filter((path) => !generated.has(path)).sort()

  if (!dryRun) {
    for (const path of stale) rmSync(join(target, path), { force: true })
    mkdirSync(target, { recursive: true })
    writeFileSync(
      join(target, MANIFEST),
      `${JSON.stringify(
        {
          source: relative(process.cwd(), source),
          commit,
          owned: [...generated].sort(),
          publishedTreePaths: [...published].sort(),
          sidebar
        },
        undefined,
        2
      )}\n`
    )
  }

  const out = process.stdout
  const verb = dryRun ? "would write" : "wrote"
  for (const path of written) out.write(`${verb}  ${join(target, path)}\n`)
  for (const path of stale) {
    out.write(`${dryRun ? "would remove" : "removed"}  ${join(target, path)}\n`)
  }
  for (const entry of rewrittenLinks) out.write(`  link -> permalink  ${entry}\n`)
  for (const { treePath, title, fallback } of fallbacks) {
    out.write(`  fallback title  ${treePath} -> ${JSON.stringify(title)}  (${fallback})\n`)
  }
  for (const treePath of unnamed) {
    out.write(`  unnamed root page  ${treePath}  (name it in AUTHORITATIVE for a short label)\n`)
  }
  out.write(
    `${dryRun ? "dry run: " : ""}${generated.size} pages at ${commit.slice(0, 12)}, ` +
      `${written.length} ${verb}, ${unchanged.length} unchanged, ${stale.length} stale, ` +
      `${UNPUBLISHED.size} unpublished, ${rewrittenLinks.length} links pinned, ` +
      `${fallbacks.length} fallback titles\n`
  )
  if (fallbacks.length > 0) {
    out.write(
      "  A fallback title is the filename stem. Give the page an H1 shaped " +
        `\`# identifier${H1_SEPARATOR}Title\` in the tree to replace it.\n`
    )
  }
}

main()
