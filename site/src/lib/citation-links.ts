// SPDX-License-Identifier: Apache-2.0
import { execFileSync } from "node:child_process"
import { join } from "node:path"
import { fileURLToPath } from "node:url"

import { defineMdastPlugin, type MdastPluginDefinition } from "satteri"

/**
 * ccu citations as links: an mdast `inlineCode` visitor that turns every `` `path:LOC` `` in the
 * generated docs tree into either an intra-site link or a repo permalink, at build time.
 *
 * `inlineCode` is a declared key on `MdastPluginInstance`, and so are `heading`, `tableRow`, and the
 * per-document `before` hook this needs for its reset points.
 *
 * ## Wrap, never replace
 *
 * The citation stays visible exactly as authored: `ctx.wrapNode` puts a `link` around the
 * `inlineCode` node, so `` `agentd/src/routes.rs:412` `` renders as
 * `<a href="…"><code>agentd/src/routes.rs:412</code></a>`. Rewriting the text would take the
 * grep-able path away from the reader who wants to run `rg` on it, which is most of them.
 *
 * ## Where the transform lands, and where it deliberately does not
 *
 * mdast runs before anything that reads the tree, so the link reaches the rendered page, and it
 * reaches the llms bundles too because `starlight-llms-txt` renders the page and flattens the HTML
 * back to Markdown. It does NOT reach the raw `.md` twin, which `starlight-md-txt` builds from the
 * entry's source body and never sends through this processor. That absence is the point: an agent
 * fetching the twin wants the repo-relative `path:LOC` it can pass to `rg`, not a URL it has to
 * parse back into one. `agent-note.ts` reasons the same way in the other direction — its label is
 * authored into the body precisely because a plugin-injected one would reach the HTML and the bundle
 * and miss the twin.
 *
 * ## Every visitor here is synchronous, and that is load-bearing
 *
 * Sätteri walks a document synchronously when no visitor returns a promise. The antecedent lives on
 * this plugin's closure, so one document's walk has to finish before the next one starts; an async
 * visitor would let two compiles interleave and one document would resolve a shorthand against the
 * other's antecedent. The presence check is therefore `execFileSync`, run once and memoized, rather
 * than an await.
 */

/** Where the citations point, and what they are pinned to. */
export interface CitationLinksOptions {
  /**
   * The commit the permalinks anchor to, resolved to a full SHA. NEVER a branch name.
   *
   * A line anchor against a moving ref is silently wrong the day after it ships: the file keeps
   * resolving, the line number keeps rendering, and it points at whatever moved into that position.
   * In CI this is the checked-out SHA the runner exports, which on a merge-commit checkout is the
   * commit under review rather than what `git rev-parse HEAD` reports.
   */
  readonly commit: string
  /** Repository web root, no trailing slash: `https://github.com/owner/repo`. */
  readonly repoUrl: string
  /** Absolute path to the repository root. Every citation path is relative to it. */
  readonly repoRoot: string
  /** Absolute path to the ccu tree, whose own pages become intra-site links. */
  readonly treeRoot: string
  /**
   * The route prefix the tree is published under, no trailing slash, and WITHOUT the site's base.
   *
   * The base is deliberately excluded, and getting this wrong is not a subtle failure: this plugin runs
   * at mdast, inside the pipeline `starlight-base-path` also augments, so the base segment is prefixed
   * onto every root-relative link this plugin emits AFTER it emits one. Including the base here produces
   * `/microvms-agentd/microvms-agentd/platform/` — measured, and caught by the link validator rather than
   * by anything in this file.
   *
   * Empty when the tree is published at the content root, which is the case here.
   */
  readonly siteBase: string
  /**
   * The tree-relative paths the site actually publishes as pages, from the sync manifest.
   *
   * Read from the manifest rather than from a walk of the tree, because the two disagree: the tree
   * holds pages the site deliberately does not publish, and a citation into one of those would
   * otherwise become an intra-site link to a route that was never built. The manifest is written by
   * the same script that decides what is published, so it cannot drift from that decision.
   */
  readonly published: ReadonlySet<string>
  /** Whether a repo-relative path exists at `commit`. Defaults to one `git ls-tree` at that commit. */
  readonly existsAtCommit?: (repoRelativePath: string) => boolean
  /** The permalink for a path and line span. Defaults to GitHub's `#L10` / `#L10-L20` fragment. */
  readonly permalink?: (target: CitationTarget) => string
  /** The intra-site href for a tree page. Defaults to `${siteBase}/${slug}/`. */
  readonly intraSiteHref?: (target: CitationTarget) => string
}

/** One resolved citation. */
export interface CitationTarget {
  /** Repository-relative path, `./` stripped. */
  readonly path: string
  readonly start: number
  /** The end of a range, or `undefined` for a single line. */
  readonly end: number | undefined
  /** The path relative to the ccu tree root, or `undefined` when it lies outside the tree. */
  readonly treePath: string | undefined
  /** The route slug, when the site publishes this path as a page. */
  readonly slug: string | undefined
}

/**
 * The extensions a citation path may carry, verbatim from ccu's own validator
 * (`docs/.packets/crosslink.py`).
 *
 * A closed list rather than `\.\w+` because a code span like `` `process.env` `` is prose, not a
 * citation, and linking it produces a permalink to a file that was never named.
 */
const EXT =
  "rs|py|pyi|toml|json|jsonc|json5|md|mdx|yml|yaml|mjs|cjs|js|jsx|ts|tsx|tf|tfvars|sh|bash|" +
  "zsh|lock|txt|typed|go|java|kt|kts|rb|php|cs|c|h|cc|cpp|hpp|swift|scala|sql|proto|graphql|" +
  "gradle|cfg|ini|mk|css|scss|html|vue|svelte|ipynb|xml|csv|snap"

/**
 * Four shapes matter, and every part is optional so one anchored pattern covers all four:
 *
 *   agentd/src/routes.rs:412            a full citation, which also becomes the antecedent
 *   agentd/src/routes.rs:412-430        a range
 *   agentd/src/routes.rs                a bare path: not a citation, but it names the antecedent
 *   :551                                a shorthand, resolved against the antecedent
 *
 * A trailing label inside the same span — `` `routes.rs:371 surface_docs()` `` — is kept, because
 * rejecting the span drops its line number AND never records the antecedent, which then reports
 * every shorthand after it as an orphan though a reader resolves them fine.
 *
 * ANCHORED, unlike ccu's validator, which scans whole lines. A wrap replaces the whole span with a
 * link, so a pattern that matched a fragment would link text that is not a citation: `.gitignore:29`
 * would emit a link to line 29 of the antecedent file with `.gitignore:29` as its label.
 */
const CITATION = new RegExp(
  "^(?:(?<repo>[A-Za-z0-9_.\\-]+):)??" +
    `(?<path>[A-Za-z0-9_./\\-]+\\.(?:${EXT}))?` +
    "(?::(?<start>\\d+)(?:-(?<end>\\d+))?)?" +
    "(?<label> .*)?$",
  "s"
)

/**
 * Directories under the tree that hold build scratch rather than pages: `.packets/` the per-agent
 * task packets, `.repomix/` the flattened pack. Both are gitignored, so a citation into one resolves
 * on the author's disk and nowhere else — which is what the presence gate catches.
 */
const isSkippedDirectory = (name: string): boolean => name.startsWith(".")

/** GitHub's line fragment. GitLab writes `#L10-20` and Bitbucket `#lines-10:20`; override for those. */
const githubPermalink = (repoUrl: string, commit: string, target: CitationTarget): string => {
  const fragment = target.end === undefined ? `L${target.start}` : `L${target.start}-L${target.end}`
  return `${repoUrl}/blob/${commit}/${target.path}#${fragment}`
}

/**
 * One path segment, slugged the way Astro's glob loader slugs an authored file.
 *
 * On `[A-Za-z0-9_-]` lowercasing is exactly what `github-slugger` does, which is what turns
 * `PLATFORM.md` into the route `/platform/`. Off that set the two diverge — `foo.bar` slugs to
 * `foobar` — so a segment outside the set yields no slug and the citation takes the permalink branch
 * instead of shipping a link to a route that does not exist.
 */
const slugSegment = (segment: string): string | undefined =>
  /^[A-Za-z0-9_-]+$/.test(segment) ? segment.toLowerCase() : undefined

const slugFor = (treePath: string): string | undefined => {
  if (!treePath.endsWith(".md")) return undefined
  const segments = treePath.slice(0, -".md".length).split("/")
  if (segments.some((segment) => isSkippedDirectory(segment))) return undefined
  const slugged = segments.map(slugSegment)
  if (slugged.some((segment) => segment === undefined)) return undefined
  return (slugged as string[]).join("/").replace(/\/index$/, "")
}

/*
 * The node and context types are derived from the visitor's own signature rather than imported from
 * `mdast`, which is transitive here and so does not resolve under pnpm's isolated layout.
 */
type InlineCodeVisitor = NonNullable<MdastPluginDefinition["inlineCode"]>
type InlineCodeNode = Parameters<InlineCodeVisitor>[0]
type VisitorContext = Parameters<InlineCodeVisitor>[1]

/**
 * Rewrites every ccu citation in a document into a link.
 *
 * ## The antecedent rules
 *
 * A shorthand `` `:LOC` `` inherits the last full path a reader has seen, and every one of these
 * rules exists because breaking it mis-attributes a correct citation to the wrong file:
 *
 * - **Carried across lines within a section.** "router `src/router.ts:422`; handlers `:551`, `:648`"
 *   resolves all three against `src/router.ts`.
 * - **RESET at a heading.** A new section is where a reader stops holding the previous file in their
 *   head, so a shorthand under a fresh heading with no path of its own is an error rather than a
 *   link into whatever the last section discussed.
 * - **RESET at the start of every table row.** A row is self-contained: its first cell names the
 *   file its later cells abbreviate. Carrying an antecedent in from the row above points a correct
 *   citation at the wrong file and invents a range error out of it.
 * - **A bare backticked path SETS the antecedent and asserts no line.** That is how a table row
 *   whose first cell is `` `agentd/src/exec.rs` `` licenses `` `:30` `` in a later cell.
 * - **A shorthand with no antecedent is an error.** No reader can resolve it either.
 *
 * The reset points arrive in document order because Sätteri dispatches in a pre-order walk: the
 * `heading` visitor fires before the spans that follow the heading, and `tableRow` before the spans
 * in its own cells.
 *
 * State lives on this closure and is reset in `before`, which runs once per document. At module
 * scope one document's last antecedent would resolve the next document's first shorthand, and the
 * link would be plausible and wrong.
 */
export const citationLinks = (options: CitationLinksOptions): MdastPluginDefinition => {
  const repoUrl = options.repoUrl.replace(/\/$/, "")
  const siteBase = options.siteBase.replace(/\/$/, "")
  const permalink = options.permalink ?? ((t) => githubPermalink(repoUrl, options.commit, t))
  const intraSiteHref = options.intraSiteHref ?? ((t) => `${siteBase}/${t.slug}/`)

  /** Paths present at `commit`, from one `git ls-tree`. Absence is what the loud failure reports. */
  let tracked: Set<string> | undefined
  const existsAtCommit =
    options.existsAtCommit ??
    ((path: string): boolean => {
      tracked ??= new Set(
        execFileSync("git", ["ls-tree", "-r", "--name-only", options.commit], {
          cwd: options.repoRoot,
          encoding: "utf8",
          maxBuffer: 1 << 26
        })
          .split("\n")
          .filter((line) => line !== "")
      )
      return tracked.has(path)
    })

  // Per-document, reset in `before`. Never module scope.
  let antecedent: string | undefined

  const where = (node: InlineCodeNode, ctx: VisitorContext): string => {
    const document = ctx.fileURL === undefined ? "<unknown document>" : fileURLToPath(ctx.fileURL)
    const line = node.position?.start.line
    return line === undefined ? document : `${document}:${line}`
  }

  return defineMdastPlugin({
    name: "ccu-citation-links",
    // Positions are read for the error messages: a build failure naming the citation but not the
    // line it sits on sends the reader grepping a 900-line document for a string that occurs twice.
    options: { position: true },

    before() {
      antecedent = undefined
    },

    heading() {
      antecedent = undefined
    },

    tableRow() {
      antecedent = undefined
    },

    inlineCode(node, ctx) {
      // A citation already inside a link stays as it is. Nesting an `a` inside an `a` is invalid
      // HTML, and the author's own link is the more specific intent.
      if (ctx.parent(node)?.type === "link") return

      const groups = CITATION.exec(node.value)?.groups
      if (groups === undefined) return
      const { repo, path: cited, start: rawStart, end: rawEnd } = groups
      if (cited === undefined && rawStart === undefined) return

      if (repo !== undefined) {
        throw new Error(
          `ccu-citation-links: ${where(node, ctx)}: \`${node.value}\` is a cross-repo citation, ` +
            "and this plugin resolves one repository. Pass a repo map, or write the path " +
            "relative to this repository's root."
        )
      }

      const normalized = cited === undefined ? undefined : cited.replace(/^\.\//, "")
      const escapes =
        normalized !== undefined &&
        (normalized.startsWith("/") || normalized.split("/").includes(".."))

      if (rawStart === undefined) {
        // A bare path names the antecedent and asserts no line. One that names nothing inside the
        // repository asserts nothing either, so it is left alone rather than failing a build over a
        // path mentioned in prose.
        if (!escapes) antecedent = normalized
        return
      }

      if (normalized !== undefined && escapes) {
        throw new Error(
          `ccu-citation-links: ${where(node, ctx)}: \`${node.value}\` cites a line in ` +
            `'${normalized}', which names no path inside ${options.repoRoot}. ` +
            "Write the citation relative to the repository root."
        )
      }

      if (normalized !== undefined) antecedent = normalized
      else if (antecedent === undefined) {
        throw new Error(
          `ccu-citation-links: ${where(node, ctx)}: \`${node.value}\` is a shorthand with no ` +
            "antecedent. The last full path resets at every heading and at the start of every " +
            "table row, so write the full path here."
        )
      }

      const path = normalized ?? (antecedent as string)
      const treePath = treeRelative(options.repoRoot, options.treeRoot, path)
      const target: CitationTarget = {
        path,
        start: Number(rawStart),
        end: rawEnd === undefined ? undefined : Number(rawEnd),
        treePath,
        slug:
          treePath === undefined || !options.published.has(treePath) ? undefined : slugFor(treePath)
      }

      // Two destinations, each checked against what its own link resolves against: a published page
      // against the sync manifest, everything else against the commit.
      if (target.slug !== undefined) {
        // The line number stays in the visible text and leaves the href: a rendered page carries no
        // line anchors, so a fragment here would be a promise the HTML cannot keep.
        ctx.wrapNode(node, { type: "link", url: intraSiteHref(target), children: [] })
        return
      }

      if (!existsAtCommit(path)) {
        throw new Error(
          `ccu-citation-links: ${where(node, ctx)}: \`${node.value}\` cites ` +
            `${path}:${rawStart}, which does not exist at ${options.commit}. ` +
            (treePath === undefined
              ? "A permalink to an absent path is a 404 carrying a line anchor. Commit the file, " +
                "or re-run ccu's cross-link pass to find every stale citation at once."
              : "A path under the docs tree that the site does not publish and git does not track " +
                "is build scratch — ccu's own checklist forbids citing one, because a reader " +
                "cannot open it.")
        )
      }
      ctx.wrapNode(node, { type: "link", url: permalink(target), children: [] })
    }
  })
}

/** A repo-relative path as a tree-relative one, or `undefined` when it lies outside the tree. */
const treeRelative = (
  repoRoot: string,
  treeRoot: string,
  repoRelativePath: string
): string | undefined => {
  const prefix = `${treeRoot.replace(/\/$/, "")}/`
  const absolute = join(repoRoot, repoRelativePath)
  return absolute.startsWith(prefix) ? absolute.slice(prefix.length) : undefined
}
