// SPDX-License-Identifier: Apache-2.0
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync
} from "node:fs"
import { tmpdir } from "node:os"
import { dirname, join } from "node:path"
import { fileURLToPath } from "node:url"
import { describe, expect, it } from "vitest"

import { braceOffenders } from "../scripts/brace-gate.mjs"
import { generate, OWNERSHIP_MANIFEST } from "../scripts/gen-reference.mjs"
import {
  loadManifest,
  loadSchema,
  REGENERATE,
  SOURCES,
  validateManifest,
  validateSchema
} from "../scripts/reference/manifest.mjs"
import { inlineText, slug } from "../scripts/reference/markdown.mjs"
import {
  ANNOTATED_PAGES,
  commandSlug,
  everyCommandSupportsJson,
  PLATFORM_PAGE,
  referencePages,
  responseTypes,
  TIER
} from "../scripts/reference/pages.mjs"
import { rawMarkdownUrl } from "../src/lib/agent-surface.js"

/**
 * Census and shape probes over the generated Reference tier, after memhtml-public's
 * `tests/census.test.ts`.
 *
 * Every expectation is a total DERIVED from the manifest or the schema and compared against the page
 * body. A probe asserting `24` would pass forever after the twenty-fifth command silently stopped being
 * documented. The rendered side is counted off the Markdown rather than off the data that built it, so
 * a page that assembles a row and then drops it fails here.
 */

const root = dirname(dirname(fileURLToPath(import.meta.url)))
const repoRoot = dirname(root)
const dist = join(root, "dist")

const manifest = loadManifest(join(repoRoot, SOURCES.manifest))
const schema = loadSchema(join(repoRoot, SOURCES.schema))
const pages = referencePages(manifest, schema)

/** The ccu pages the sync writes beside this tier. Named here as the set this tier must never claim. */
const CCU_PAGES = ["cli", "public-api", "rpc-tools"].map((stem) => `${TIER}/${stem}.md`)

const page = (id: string) => {
  const found = pages.find((candidate) => candidate.id === id)
  if (!found) throw new Error(`no page \`${id}\`; generated: ${pages.map((p) => p.id).join(", ")}`)
  return found
}

/** Every DATA row of every Markdown table on a page: header and separator lines drop out. */
const tableRows = (body: string): ReadonlyArray<string> => {
  const lines = body.split("\n")
  const isSeparator = (line: string | undefined): boolean =>
    line !== undefined && /^\|(\s*---\s*\|)+$/.test(line)
  return lines.filter(
    (line, at) => line.startsWith("|") && !isSeparator(line) && !isSeparator(lines[at + 1])
  )
}

/** Rows whose FIRST cell is exactly the given code span. */
const rowsLeadingWith = (body: string, value: string): number =>
  tableRows(body).filter((row) => row.startsWith(`| \`${value}\` |`)).length

const headings = (body: string, marker: string): ReadonlyArray<string> =>
  body.split("\n").filter((line) => line.startsWith(marker))

/* =================================================================================================
 * Census: every member of every registry reaches exactly one place.
 * ================================================================================================= */

describe("every manifest member reaches a page exactly once", () => {
  it("gives each command its own page, and no command two", () => {
    const commandPages = pages.filter((one) => one.id.startsWith(`${TIER}/commands/`))
    expect(commandPages).toHaveLength(manifest.data.commands.length)
    expect(commandPages.map((one) => one.id).sort()).toEqual(
      manifest.data.commands
        .map((command) => `${TIER}/commands/${commandSlug(command.name)}`)
        .sort()
    )
  })

  it("links every command from the overview's command table once, in manifest order", () => {
    const body = page(TIER).body
    // Every row linking into a command page. The census table links the SECTION, so it is not counted.
    const rows = tableRows(body).filter((row) => row.includes(`/${TIER}/commands/`))
    expect(rows).toHaveLength(manifest.data.commands.length)
    expect(rows.map((row) => /\/commands\/([a-z0-9-]+)\//.exec(row)?.[1])).toEqual(
      manifest.data.commands.map((command) => commandSlug(command.name))
    )
  })

  it("lists every exit code once, with its status, and a finding link where the manifest names one", () => {
    const body = page(`${TIER}/exit-codes`).body
    const rows = tableRows(body).filter((row) => /^\| `\d+` \|/.test(row))
    expect(rows).toHaveLength(manifest.data.exitCodes.length)
    for (const row of manifest.data.exitCodes) {
      expect(rowsLeadingWith(body, String(row.exit))).toBe(1)
      if (row.code !== null) expect(body.split(`\`${row.code}\``)).toHaveLength(2)
      if (row.finding !== "") {
        expect(body).toContain(`](${PLATFORM_PAGE}#${slug(row.finding)})`)
        expect(body).toContain(`[${inlineText(row.finding)}](`)
      }
    }
  })

  it("lists every distinct response type once, naming each emitter and the alternate", () => {
    const body = page(`${TIER}/response-types`).body
    // Recomputed independently, in order of first appearance: a command's own type, then its alternate.
    const distinct = new Set(
      manifest.data.commands.flatMap((command) => [
        command.responseType,
        ...(command.alternateResponse === null ? [] : [command.alternateResponse.responseType])
      ])
    )
    expect(responseTypes(manifest).map((entry) => entry.type)).toEqual([...distinct])
    const rows = tableRows(body).filter((row) => row.startsWith("| `microvm."))
    expect(rows).toHaveLength(distinct.size)
    for (const type of distinct) expect(rowsLeadingWith(body, type)).toBe(1)
    for (const command of manifest.data.commands) {
      const row = rows.find((candidate) => candidate.startsWith(`| \`${command.responseType}\` |`))
      expect(row, command.name).toContain(`/${TIER}/commands/${commandSlug(command.name)}/`)
      if (command.alternateResponse !== null) {
        const alternate = rows.find((candidate) =>
          candidate.startsWith(`| \`${command.alternateResponse?.responseType}\` |`)
        )
        expect(alternate).toContain(`with \`${command.alternateResponse.when}\``)
      }
    }
  })

  it("lists every envelope field and every convention once", () => {
    const body = page(`${TIER}/envelope`).body
    const fields = [
      ...Object.keys(manifest.data.envelope.ok),
      ...Object.keys(manifest.data.envelope.error)
    ]
    const rows = tableRows(body).filter((row) => !row.startsWith("| Field"))
    expect(rows).toHaveLength(fields.length)
    const bullets = body.split("\n").filter((line) => line.startsWith("- "))
    expect(bullets).toHaveLength(manifest.data.conventions.length)
  })

  it("gives every schema definition exactly one subsection", () => {
    const body = page(`${TIER}/wire-schema`).body
    const names = Object.keys(schema.$defs)
    const subsections = headings(body, "### ")
    expect(subsections).toHaveLength(names.length)
    expect(subsections.map((line) => line.replace(/^### \d+\.\d+\. /, ""))).toEqual(names)
    for (const name of names) {
      const definition = schema.$defs[name]
      if (definition?.properties === undefined) continue
      for (const field of Object.keys(definition.properties)) {
        expect(body, `${name}.${field}`).toContain(`| \`${field}\` |`)
      }
    }
  })

  it("lists every route on the wire-schema page", () => {
    const body = page(`${TIER}/wire-schema`).body
    for (const route of schema.routes)
      expect(body).toContain(`| \`${route.method}\` | \`${route.path}\` |`)
  })

  it("links each annotated ccu page from the overview, and claims none of them", () => {
    const body = page(TIER).body
    for (const annotated of ANNOTATED_PAGES) expect(body).toContain(`](/${annotated.id}/)`)
    expect(pages.map((one) => one.path).filter((path) => CCU_PAGES.includes(path))).toEqual([])
  })
})

/* =================================================================================================
 * Shape: what every page must look like.
 * ================================================================================================= */

describe("the pages themselves", () => {
  it("keeps ids, routes and paths unique and inside the tier", () => {
    expect(new Set(pages.map((one) => one.id)).size).toBe(pages.length)
    expect(new Set(pages.map((one) => one.path)).size).toBe(pages.length)
    for (const one of pages) {
      expect(one.path.startsWith(`${TIER}/`)).toBe(true)
      expect(one.path.endsWith(".md")).toBe(true)
      expect(one.route).toBe(`/${one.id}/`)
      expect(one.path).toBe(one.path.toLowerCase())
    }
  })

  it("gives every page a title, a plain description, a body, a label and an order", () => {
    for (const one of pages) {
      expect(one.title.length).toBeGreaterThan(0)
      expect(one.description.length).toBeGreaterThan(0)
      expect(one.description).not.toContain("`")
      expect(one.description).not.toContain("\n")
      expect(one.body.length).toBeGreaterThan(0)
      expect(one.sidebarLabel.length).toBeGreaterThan(0)
      expect(Number.isInteger(one.sidebarOrder)).toBe(true)
    }
  })

  it("sorts the overview first and the commands in manifest order", () => {
    const overview = page(TIER)
    for (const one of pages) {
      if (one !== overview) expect(one.sidebarOrder).toBeGreaterThan(overview.sidebarOrder)
    }
    const orders = manifest.data.commands.map(
      (command) => page(`${TIER}/commands/${commandSlug(command.name)}`).sidebarOrder
    )
    expect([...orders].sort((a, b) => a - b)).toEqual(orders)
    for (const command of manifest.data.commands) {
      expect(page(`${TIER}/commands/${commandSlug(command.name)}`).sidebarLabel).toBe(command.name)
    }
  })

  it("numbers top-level sections from one, contiguously, in the heading text", () => {
    for (const one of pages) {
      const top = headings(one.body, "## ")
      expect(top.length, one.id).toBeGreaterThan(1)
      expect(top.map((heading) => heading.split(" ")[1])).toEqual(top.map((_, at) => `${at + 1}.`))
    }
  })

  it("numbers subsections under their parent, contiguously", () => {
    for (const one of pages) {
      let parent = 0
      let child = 0
      for (const line of one.body.split("\n")) {
        if (line.startsWith("## ")) {
          parent += 1
          child = 0
        } else if (line.startsWith("### ")) {
          child += 1
          expect(line.split(" ")[1], `${one.id}: ${line}`).toBe(`${parent}.${child}.`)
        }
      }
    }
  })

  it("writes no bare brace outside a code span or fence, which would break the raw route", () => {
    // The same scanner the build gate runs, so the probe and the gate cannot disagree.
    for (const one of pages) {
      expect(braceOffenders(one.body), one.id).toEqual([])
    }
  })

  it("mounts no raw HTML element from a quoted placeholder", () => {
    for (const one of pages) {
      const outsideCode = one.body.replace(/`+[^`]*`+/g, "")
      expect(outsideCode, one.id).not.toMatch(/<[a-z][a-z0-9-]*[\s>]/)
    }
  })

  it("closes every page with the source file and the regenerate command", () => {
    for (const one of pages) {
      const last = headings(one.body, "## ").at(-1) ?? ""
      expect(last).toContain("Provenance")
      expect(one.body).toContain(`\`${one.source}\``)
      const which = one.source === SOURCES.schema ? "schema" : "manifest"
      expect(one.body).toContain(`\`${REGENERATE[which]}\``)
    }
  })

  it("links every command page to the exit codes and the envelope", () => {
    for (const command of manifest.data.commands) {
      const body = page(`${TIER}/commands/${commandSlug(command.name)}`).body
      expect(body).toContain(`](/${TIER}/exit-codes/)`)
      expect(body).toContain(`](/${TIER}/envelope/)`)
      expect(body).toContain(`\`\`\`sh\n${manifest.data.cli} ${command.name}`)
      for (const parameter of command.parameters) {
        const shown = parameter.positional ? parameter.name : `--${parameter.name}`
        expect(rowsLeadingWith(body, shown), `${command.name} ${shown}`).toBe(1)
      }
      if (command.alternateResponse !== null) {
        expect(body).toContain(`### 3.1. With \`${command.alternateResponse.when}\``)
      }
    }
  })

  it("states the global flag once, on the overview, when every command supports JSON", () => {
    /*
     * The three flags the CLI reads off raw argv never appear in any command's parameter list, so
     * there is nothing to factor out of the tables. What the manifest DOES state is `supportsJson`,
     * and this is the derived sentence about it.
     */
    for (const command of manifest.data.commands) {
      for (const parameter of command.parameters) {
        expect(["json", "dense", "quiet"]).not.toContain(parameter.name)
      }
    }
    const overview = page(TIER).body
    expect(overview).toContain("## 3. Global flags")
    if (everyCommandSupportsJson(manifest)) {
      expect(overview).toContain(`all ${manifest.data.commands.length} set it`)
      for (const command of manifest.data.commands) {
        expect(page(`${TIER}/commands/${commandSlug(command.name)}`).body).toContain(
          `](/${TIER}/#3-global-flags)`
        )
      }
    }
  })

  it("writes no literal count into prose that the registries could contradict", () => {
    /*
     * Every number on the overview must be a `.length`. Rather than parse the source, this checks the
     * consequence: the numbers the overview prints agree with the registries it was built from.
     */
    const body = page(TIER).body
    expect(body).toContain(`accepts ${manifest.data.commands.length} commands`)
    expect(body).toContain(`| ${manifest.data.exitCodes.length} |`)
    expect(body).toContain(`| ${Object.keys(schema.$defs).length} |`)
  })
})

/* =================================================================================================
 * The mutation lock: the tier is a pure function of its inputs.
 * ================================================================================================= */

describe("the mutation lock", () => {
  const synthetic = {
    alternateResponse: null,
    name: "zz-synthetic",
    parameters: [
      {
        choices: null,
        default: null,
        help: "A synthetic positional",
        name: "thing",
        positional: true,
        required: true,
        type: "string"
      }
    ],
    responseKeys: ["thing"],
    responseType: "microvm.zz-synthetic",
    summary: "A command that exists only in this test",
    supportsJson: true
  }

  it("moves the page count by exactly one when a command is appended to a copy of the manifest", () => {
    const copy = structuredClone(manifest)
    copy.data.commands = [...copy.data.commands, synthetic]
    const mutated = referencePages(validateManifest(copy, "a copy"), schema)
    expect(mutated).toHaveLength(pages.length + 1)
    const added = mutated.find((one) => one.id === `${TIER}/commands/zz-synthetic`)
    expect(added).toBeDefined()
    expect(added?.body).toContain("```sh\nmicrovm zz-synthetic <thing>\n```")
    const overview = mutated.find((one) => one.id === TIER)?.body ?? ""
    expect(overview).toContain(`accepts ${copy.data.commands.length} commands`)
    expect(overview).toContain(`/${TIER}/commands/zz-synthetic/`)
    // The new type is a new row, not a missing one.
    const types = mutated.find((one) => one.id === `${TIER}/response-types`)?.body ?? ""
    expect(rowsLeadingWith(types, "microvm.zz-synthetic")).toBe(1)
  })

  it("adds a row when an exit code is appended, and a subsection when a schema type is", () => {
    const withExit = structuredClone(manifest)
    withExit.data.exitCodes = [
      ...withExit.data.exitCodes,
      { code: "ERR_SYNTHETIC", exit: 99, finding: "", meaning: "a synthetic failure" }
    ]
    const exitBody = referencePages(validateManifest(withExit, "a copy"), schema).find(
      (one) => one.id === `${TIER}/exit-codes`
    )?.body
    expect(tableRows(exitBody ?? "").filter((row) => /^\| `\d+` \|/.test(row))).toHaveLength(
      manifest.data.exitCodes.length + 1
    )

    const withType = structuredClone(schema)
    withType.$defs = {
      ...withType.$defs,
      ZzSynthetic: {
        description: "A type that exists only in this test.",
        properties: { thing: { type: "string" } },
        required: ["thing"],
        type: "object"
      }
    }
    const schemaBody = referencePages(manifest, validateSchema(withType, "a copy")).find(
      (one) => one.id === `${TIER}/wire-schema`
    )?.body
    expect(headings(schemaBody ?? "", "### ")).toHaveLength(Object.keys(schema.$defs).length + 1)
  })

  it("refuses a manifest whose shape drifted, naming the field", () => {
    const noCommands = structuredClone(manifest)
    noCommands.data.commands = []
    expect(() => validateManifest(noCommands, "poisoned")).toThrow(
      /poisoned.*`data\.commands` should be a non-empty array/
    )

    const renamed = structuredClone(manifest) as unknown as {
      data: { commands: Array<Record<string, unknown>> }
    }
    const first = renamed.data.commands[0]
    if (first === undefined) throw new Error("the manifest has no commands")
    first.params = first.parameters
    delete first.parameters
    expect(() => validateManifest(renamed, "poisoned")).toThrow(
      /`data\.commands\[0\]\.parameters` should be an array/
    )

    const wrongType = structuredClone(manifest)
    wrongType.type = "microvm.doctor"
    expect(() => validateManifest(wrongType, "poisoned")).toThrow(
      /`type` should be "microvm\.manifest"/
    )

    const danglingRequired = structuredClone(schema)
    const health = danglingRequired.$defs.Health
    if (health === undefined) throw new Error("the schema has no Health type")
    danglingRequired.$defs = {
      ...danglingRequired.$defs,
      Health: { ...health, required: [...(health.required ?? []), "nonexistent"] }
    }
    expect(() => validateSchema(danglingRequired, "poisoned")).toThrow(/\$defs\.Health\.required/)
  })

  it("accepts the committed files, so the refusals above are not vacuous", () => {
    expect(() => validateManifest(structuredClone(manifest))).not.toThrow()
    expect(() => validateSchema(structuredClone(schema))).not.toThrow()
  })
})

/* =================================================================================================
 * Ownership: the generator writes only what it owns and deletes only what it wrote.
 * ================================================================================================= */

describe("the generator's ownership rules", () => {
  const scratch = () => mkdtempSync(join(tmpdir(), "gen-reference-"))

  it("writes every page, records them, and leaves a second run with nothing to do", () => {
    const contentDir = scratch()
    try {
      const first = generate({ contentDir, manifest, schema })
      expect(first.written.sort()).toEqual(pages.map((one) => one.path).sort())
      expect(first.removed).toEqual([])
      const recorded = JSON.parse(readFileSync(join(contentDir, OWNERSHIP_MANIFEST), "utf8"))
      expect(recorded.owned).toEqual(pages.map((one) => one.path).sort())
      for (const one of pages) {
        const text = readFileSync(join(contentDir, one.path), "utf8")
        expect(text.startsWith("---\ntitle: ")).toBe(true)
        expect(text).toContain("\neditUrl: false\n")
        expect(text).toContain(`\ndescription: `)
        expect(text).toContain(`\n  label: `)
        expect(text).toContain(`\n  order: ${one.sidebarOrder}\n`)
      }
      const second = generate({ contentDir, manifest, schema })
      expect(second.written).toEqual([])
      expect(second.unchanged).toHaveLength(pages.length)
    } finally {
      rmSync(contentDir, { recursive: true, force: true })
    }
  })

  it("removes only a page it wrote before and no longer generates", () => {
    const contentDir = scratch()
    try {
      generate({ contentDir, manifest, schema })
      const smaller = structuredClone(manifest)
      const dropped = smaller.data.commands.at(-1)
      if (dropped === undefined) throw new Error("the manifest has no commands")
      smaller.data.commands = smaller.data.commands.slice(0, -1)
      // A stranger in the directory, owned by nobody this generator knows about.
      writeFileSync(join(contentDir, TIER, "stranger.md"), "---\ntitle: x\n---\n")
      const run = generate({ contentDir, manifest: validateManifest(smaller, "a copy"), schema })
      expect(run.removed).toEqual([`${TIER}/commands/${commandSlug(dropped.name)}.md`])
      expect(
        existsSync(join(contentDir, TIER, "commands", `${commandSlug(dropped.name)}.md`))
      ).toBe(false)
      expect(existsSync(join(contentDir, TIER, "stranger.md"))).toBe(true)
    } finally {
      rmSync(contentDir, { recursive: true, force: true })
    }
  })

  it("refuses to overwrite a file it does not own", () => {
    const contentDir = scratch()
    try {
      mkdirSync(join(contentDir, TIER), { recursive: true })
      writeFileSync(join(contentDir, TIER, "envelope.md"), "---\ntitle: someone else's\n---\n")
      expect(() => generate({ contentDir, manifest, schema })).toThrow(
        /refusing to overwrite reference\/envelope\.md/
      )
      // Nothing was written past the refusal point that a reader could mistake for a complete tier.
      expect(existsSync(join(contentDir, OWNERSHIP_MANIFEST))).toBe(false)
    } finally {
      rmSync(contentDir, { recursive: true, force: true })
    }
  })

  it("refuses a path the sync's own manifest claims, even before the file exists", () => {
    const contentDir = scratch()
    try {
      writeFileSync(
        join(contentDir, ".sync-manifest.json"),
        JSON.stringify({ owned: [`${TIER}/exit-codes.md`] })
      )
      expect(() => generate({ contentDir, manifest, schema })).toThrow(
        /scripts\/sync-docs\.mjs lists it/
      )
    } finally {
      rmSync(contentDir, { recursive: true, force: true })
    }
  })

  it("never claims a path the sync writes", () => {
    expect(pages.map((one) => one.path).filter((path) => CCU_PAGES.includes(path))).toEqual([])
    const synced = join(root, "src", "content", "docs", ".sync-manifest.json")
    if (!existsSync(synced)) return
    const owned: ReadonlyArray<string> = JSON.parse(readFileSync(synced, "utf8")).owned ?? []
    expect(pages.map((one) => one.path).filter((path) => owned.includes(path))).toEqual([])
  })
})

/* =================================================================================================
 * The build: every page reached dist, twice.
 * ================================================================================================= */

describe("every generated page reaches the build", () => {
  const CONFIG = {
    origin: (process.env.DOCS_SITE ?? "https://theagenticguy.github.io").replace(/\/+$/, ""),
    base: process.env.DOCS_BASE ?? "/microvms-agentd/"
  }
  const segment = CONFIG.base.endsWith("/") ? CONFIG.base : `${CONFIG.base}/`
  const context = { site: new URL(CONFIG.origin), base: CONFIG.base }
  const served = (path: string): string =>
    path.startsWith(segment) ? path.slice(segment.length) : path.replace(/^\/+/, "")

  const requireDist = () => {
    if (!existsSync(dist)) {
      throw new Error("`dist/` is absent; run `pnpm run build` before this suite")
    }
  }

  it("has an index.html and a .md twin for every page", () => {
    requireDist()
    for (const one of pages) {
      const html = join(dist, one.id, "index.html")
      expect(existsSync(html), `${one.route} has no ${html}`).toBe(true)
      const twin = join(dist, served(rawMarkdownUrl(one.id, context).pathname))
      expect(existsSync(twin), `${one.route} has no twin at ${twin}`).toBe(true)
      // The twin is the page's own Markdown, so its numbered headings survive verbatim.
      const first = headings(one.body, "## ")[0] ?? ""
      expect(readFileSync(twin, "utf8"), twin).toContain(first)
    }
  })

  it("renders every command page with its own synopsis fence", () => {
    requireDist()
    for (const command of manifest.data.commands) {
      const html = readFileSync(
        join(dist, TIER, "commands", commandSlug(command.name), "index.html"),
        "utf8"
      )
      expect(html).toContain(`microvm ${command.name}`)
      expect(html).toContain("<h2")
    }
  })

  it("leaves no stale command page in dist", () => {
    requireDist()
    const built = readdirSync(join(dist, TIER, "commands"), { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .map((entry) => entry.name)
      .sort()
    expect(built).toEqual(manifest.data.commands.map((command) => commandSlug(command.name)).sort())
  })
})
