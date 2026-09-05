// SPDX-License-Identifier: Apache-2.0
/**
 * The Reference tier: every page as a pure function of `docs/manifest.json` and `docs/schema.json`.
 *
 * Ported from memhtml-public's `apps/docs/src/loaders/pages.ts`, with one difference that the comment in
 * `src/content.config.ts` explains: the pages here become real files under `src/content/docs/`, written
 * by `scripts/gen-reference.mjs`, rather than loader-injected entries. A synthesized entry has no source
 * file for `starlight-links-validator` and no `fileURL` for the mdast visitors, so its links report
 * invalid and its diagrams render as code.
 *
 * Pure and total on purpose. Pure, so a test can append a synthetic command to a copy of the manifest
 * and assert the page count moves by one. Total, so every command, exit code, response type and schema
 * definition reaches a page: the generator writes exactly what this returns.
 *
 * No quantity is written as a literal. Where prose needs a count it reads `length`, which is why adding
 * a command cannot leave a sentence claiming the previous number of them.
 */

import { REGENERATE, SOURCES } from "./manifest.mjs"
import {
  bullets,
  cell,
  code,
  codeList,
  fence,
  inlineText,
  link,
  paragraphs,
  sectionHeading,
  sections,
  slug,
  table
} from "./markdown.mjs"

/** @typedef {import("./manifest.mjs").Manifest} Manifest */
/** @typedef {import("./manifest.mjs").Command} Command */
/** @typedef {import("./manifest.mjs").Parameter} Parameter */
/** @typedef {import("./manifest.mjs").Schema} Schema */
/** @typedef {import("./manifest.mjs").SchemaDefinition} SchemaDefinition */
/** @typedef {import("./manifest.mjs").SchemaProperty} SchemaProperty */
/** @typedef {import("./markdown.mjs").Section} Section */

/**
 * @typedef {object} ReferencePage
 * @property {string} id the content-collection entry id, which is also the route without slashes
 * @property {string} path where the file is written, relative to `src/content/docs/`
 * @property {string} route the root-relative URL, base excluded, trailing slash
 * @property {string} title
 * @property {string} description plain text for the `<meta>` description
 * @property {string} body the Markdown body, RFC-numbered
 * @property {number} sidebarOrder
 * @property {string} sidebarLabel
 * @property {string} source the repo-relative file the page is derived from
 */

/** Where in the site the tier lives. One constant, so a move is one edit. */
export const TIER = "reference"

/**
 * Where the hand-written platform findings are published. The exit-code table links each `finding`
 * into this page. The path is the brief's URL plan, base excluded, because `starlight-base-path`
 * prefixes the rendered tree once.
 */
export const PLATFORM_PAGE = "/internals/platform/"

/**
 * The three ccu-generated pages that share the `reference/` directory. They are written by
 * `scripts/sync-docs.mjs`, never by this tier, and the overview links to them so the tier has one
 * front door. Their labels are their path stems, because their titles live in the tree.
 */
export const ANNOTATED_PAGES = Object.freeze([
  { id: `${TIER}/cli`, label: "cli" },
  { id: `${TIER}/public-api`, label: "public-api" },
  { id: `${TIER}/rpc-tools`, label: "rpc-tools" }
])

/** Sidebar ranks. The overview sorts first, the cross-cutting pages next, then the commands. */
const ORDER = Object.freeze({
  overview: 0,
  envelope: 1,
  exitCodes: 2,
  responseTypes: 3,
  wireSchema: 4,
  /** The first command's rank. Commands take `COMMANDS + index`, so manifest order is sidebar order. */
  commands: 100
})

/** How long a derived description may be before it is cut at a word boundary. */
const DESCRIPTION_LIMIT = 180

/** A command's slug. Names carry no spaces today; the function is where that assumption lives. */
export const commandSlug = (name) => name.replaceAll(" ", "-")

/** @param {string} id */
export const routeOf = (id) => `/${id}/`

/** @param {string} id @param {string} label */
const pageLink = (id, label) => link(label, routeOf(id))

/** @param {string} name */
const commandId = (name) => `${TIER}/commands/${commandSlug(name)}`

/** @param {Command} command */
const commandLink = (command) => pageLink(commandId(command.name), code(`microvm ${command.name}`))

/**
 * Plain text for a frontmatter description: no code markers, one line, cut at a word boundary.
 *
 * @param {string} text
 */
const plain = (text) => {
  const flat = text.replaceAll("`", "").replace(/\s+/g, " ").trim()
  if (flat.length <= DESCRIPTION_LIMIT) return flat
  const cut = flat.slice(0, DESCRIPTION_LIMIT)
  const boundary = cut.lastIndexOf(" ")
  return `${(boundary === -1 ? cut : cut.slice(0, boundary)).replace(/[,;:.-]$/, "")}...`
}

/**
 * The closing section every page carries: which file it came from, and how to regenerate both.
 *
 * @param {"manifest" | "schema"} which
 * @param {string} what what the page reads out of the source
 * @returns {Section}
 */
const provenance = (which, what) => ({
  title: "Provenance",
  body: [
    `This page is generated from ${code(SOURCES[which])}, ${what} ${code("site/scripts/gen-reference.mjs")} writes it into the site's content directory on every ${code("pnpm run sync")}, so an edit made here is overwritten by the next run.`,
    `To change the page, change the source. Regenerate the source with ${code(REGENERATE[which])} from the repository root; ${code(`${REGENERATE[which]}:check`)} fails when the committed file no longer matches what the binary emits.`
  ].join("\n\n")
})

/* =================================================================================================
 * Commands.
 * ================================================================================================= */

/** @param {Parameter} parameter */
const synopsisToken = (parameter) => {
  if (parameter.positional)
    return parameter.required ? `<${parameter.name}>` : `[${parameter.name}]`
  const flag = `--${parameter.name}`
  return parameter.type === "boolean" ? flag : `${flag} <${parameter.type}>`
}

/**
 * One line a caller can copy: the binary, the command, each positional, each required flag, then
 * `[options]` when any optional flag exists.
 *
 * @param {Manifest} manifest
 * @param {Command} command
 */
export const synopsis = (manifest, command) => {
  const positionals = command.parameters.filter((parameter) => parameter.positional)
  const requiredFlags = command.parameters.filter(
    (parameter) => !parameter.positional && parameter.required
  )
  const optional = command.parameters.some(
    (parameter) => !parameter.positional && !parameter.required
  )
  return [
    manifest.data.cli,
    command.name,
    ...positionals.map(synopsisToken),
    ...requiredFlags.map(synopsisToken),
    ...(optional ? ["[options]"] : [])
  ].join(" ")
}

/**
 * The overview's section titles, in order. Held as data so a command page can link to a section by
 * anchor without restating its number.
 */
const OVERVIEW_TITLES = Object.freeze([
  "What this tier is",
  "The commands",
  "Global flags",
  "The annotated pages",
  "Provenance"
])

/** The anchor of an overview section, derived from its position and title. */
const overviewAnchor = (title) => {
  const at = OVERVIEW_TITLES.indexOf(title)
  if (at === -1) throw new Error(`no overview section titled ${JSON.stringify(title)}`)
  return `${routeOf(TIER)}#${slug(sectionHeading(`${at + 1}.`, title))}`
}

/**
 * Whether the manifest marks every command as accepting `--json`.
 *
 * The manifest carries no list of command-wide flags. What it carries is `supportsJson` on each
 * command, so that is the one global flag this tier can state. The three flags the CLI reads off raw
 * argv (`--json`, `--dense`, `--quiet`) never appear in any command's `parameters`, so there is nothing
 * to factor out of the per-command tables.
 *
 * @param {Manifest} manifest
 */
export const everyCommandSupportsJson = (manifest) =>
  manifest.data.commands.every((command) => command.supportsJson)

/**
 * @param {Manifest} manifest
 * @param {Command} command
 * @returns {Section}
 */
const parametersSection = (manifest, command) => {
  const rows = command.parameters.map((parameter) => [
    parameter.positional ? code(parameter.name) : code(`--${parameter.name}`),
    parameter.positional ? "positional" : "flag",
    code(parameter.type),
    parameter.required ? "yes" : "no",
    parameter.default === null ? "none" : code(parameter.default),
    parameter.choices === null ? "any" : codeList(parameter.choices),
    cell(parameter.help)
  ])
  const json = command.supportsJson
    ? everyCommandSupportsJson(manifest)
      ? `${code("--json")} is accepted here as on every command, and is left out of the table above for that reason; see ${link("Global flags", overviewAnchor("Global flags"))}.`
      : `${code("--json")} is accepted here: the manifest marks this command ${code("supportsJson")}.`
    : `The manifest does not mark this command ${code("supportsJson")}, so ${code("--json")} is not part of its surface.`
  return {
    title: "Parameters",
    body: [
      rows.length === 0
        ? "This command takes no parameters of its own."
        : table(["Parameter", "Kind", "Type", "Required", "Default", "Choices", "Help"], rows),
      json
    ].join("\n\n")
  }
}

/**
 * @param {Command} command
 * @returns {Section}
 */
const responseSection = (command) => {
  const alternate = command.alternateResponse
  return {
    title: "Response",
    body: [
      `On success stdout carries one envelope whose ${code("type")} is ${code(command.responseType)}. Its ${code("data")} object carries these keys: ${codeList(command.responseKeys)}.`,
      `${pageLink(`${TIER}/envelope`, "The envelope")} describes the fields around ${code("data")}. ${pageLink(`${TIER}/response-types`, "Response types")} lists every ${code("type")} the CLI emits and which commands share each one.`
    ].join("\n\n"),
    children:
      alternate === null
        ? []
        : [
            {
              title: `With ${code(alternate.when)}`,
              body: [
                `${code(alternate.when)} changes what stdout carries: ${inlineText(alternate.stdout)}`,
                `The final line is an envelope whose ${code("type")} is ${code(alternate.responseType)}, with these keys in ${code("data")}: ${codeList(alternate.responseKeys)}.`
              ].join("\n\n")
            }
          ]
  }
}

/** @returns {Section} */
const failureSection = () => ({
  title: "Failures",
  body: [
    `A failure exits with one of the statuses on ${pageLink(`${TIER}/exit-codes`, "Exit codes")} and writes the error shape on ${pageLink(`${TIER}/envelope`, "The envelope")}: a stable ${code("code")} to branch on, an ${code("exitCode")} that matches the process status, a human-readable ${code("error")}, and ${code("suggestions")}.`,
    `Where a failure is one this project has measured on the platform, the envelope's ${code("finding")} names the section of the platform notes that documents it. The exit-code table links each one.`
  ].join("\n\n")
})

/**
 * @param {Manifest} manifest
 * @param {Command} command
 * @param {number} index position in the manifest, which is the sidebar order
 * @returns {ReferencePage}
 */
const commandPage = (manifest, command, index) => {
  const id = commandId(command.name)
  return {
    id,
    path: `${id}.md`,
    route: routeOf(id),
    title: `${manifest.data.cli} ${command.name}`,
    description: plain(command.summary),
    sidebarOrder: ORDER.commands + index,
    sidebarLabel: command.name,
    source: SOURCES.manifest,
    body: sections([
      {
        title: "Synopsis",
        body: [inlineText(command.summary), fence("sh", synopsis(manifest, command))].join("\n\n")
      },
      parametersSection(manifest, command),
      responseSection(command),
      failureSection(),
      provenance(
        "manifest",
        `the output of ${code(`${manifest.data.cli} manifest`)}, which the CLI derives from its own argument tree. This page reads the ${code(command.name)} entry of ${code("data.commands")}.`
      )
    ])
  }
}

/* =================================================================================================
 * Response types: a join across the commands.
 * ================================================================================================= */

/**
 * @typedef {object} Emitter
 * @property {Command} command
 * @property {string | undefined} via the flag that selects this response, when it is the alternate
 * @property {ReadonlyArray<string>} keys
 */

/**
 * Every distinct `responseType`, in order of first appearance, with the commands that emit it.
 *
 * @param {Manifest} manifest
 * @returns {ReadonlyArray<{ type: string, emitters: ReadonlyArray<Emitter> }>}
 */
export const responseTypes = (manifest) => {
  /** @type {Map<string, Emitter[]>} */
  const byType = new Map()
  const add = (type, emitter) => {
    const list = byType.get(type) ?? []
    list.push(emitter)
    byType.set(type, list)
  }
  for (const command of manifest.data.commands) {
    add(command.responseType, { command, via: undefined, keys: command.responseKeys })
    if (command.alternateResponse !== null) {
      add(command.alternateResponse.responseType, {
        command,
        via: command.alternateResponse.when,
        keys: command.alternateResponse.responseKeys
      })
    }
  }
  return [...byType].map(([type, emitters]) => ({ type, emitters }))
}

/** @param {ReadonlyArray<Emitter>} emitters */
const unionKeys = (emitters) => [...new Set(emitters.flatMap((emitter) => emitter.keys))]

/** @param {ReadonlyArray<Emitter>} emitters */
const sharedKeys = (emitters) =>
  unionKeys(emitters).filter((key) => emitters.every((emitter) => emitter.keys.includes(key)))

/** @param {Emitter} emitter */
const emitterLink = (emitter) =>
  emitter.via === undefined
    ? commandLink(emitter.command)
    : `${commandLink(emitter.command)} with ${code(emitter.via)}`

/**
 * @param {Manifest} manifest
 * @returns {ReferencePage}
 */
const responseTypesPage = (manifest) => {
  const types = responseTypes(manifest)
  const disagreements = types
    .map(({ type, emitters }) => {
      const shared = sharedKeys(emitters)
      const extras = emitters
        .map((emitter) => ({
          emitter,
          extra: emitter.keys.filter((key) => !shared.includes(key))
        }))
        .filter(({ extra }) => extra.length > 0)
      return { type, extras }
    })
    .filter(({ extras }) => extras.length > 0)
  const id = `${TIER}/response-types`
  return {
    id,
    path: `${id}.md`,
    route: routeOf(id),
    title: "Response types",
    description:
      "Every value the envelope's type field takes, the commands that emit each one, and the keys its data object carries.",
    sidebarOrder: ORDER.responseTypes,
    sidebarLabel: "Response types",
    source: SOURCES.manifest,
    body: sections([
      {
        title: "How to read the table",
        body: [
          `A success envelope names its payload shape in ${code("type")}, and a caller reads that field before it parses ${code("data")}. Each row below is one value of ${code("type")}, the commands whose success envelope carries it, and the union of the ${code("data")} keys those commands declare.`,
          `The manifest lists ${manifest.data.commands.length} commands and ${types.length} distinct types, so several commands share a shape. A command that emits a second shape under a flag is listed twice, with the flag named.`
        ].join("\n\n")
      },
      {
        title: "The types",
        body: [
          table(
            ["Type", "Emitted by", "Keys in `data`"],
            types.map(({ type, emitters }) => [
              code(type),
              emitters.map(emitterLink).join(", "),
              codeList(unionKeys(emitters))
            ])
          ),
          disagreements.length === 0
            ? "Every command that shares a type declares the same keys for it."
            : [
                "Where commands share a type and declare different keys, the row above is the union. The differences:",
                bullets(
                  disagreements.flatMap(({ type, extras }) =>
                    extras.map(
                      ({ emitter, extra }) =>
                        `${code(type)}: ${emitterLink(emitter)} also carries ${codeList(extra)}.`
                    )
                  )
                )
              ].join("\n\n")
        ].join("\n\n")
      },
      provenance(
        "manifest",
        `the output of ${code(`${manifest.data.cli} manifest`)}. The table is a join over every command's ${code("responseType")}, ${code("responseKeys")} and ${code("alternateResponse")}.`
      )
    ])
  }
}

/* =================================================================================================
 * Exit codes.
 * ================================================================================================= */

/**
 * The link for an exit code's `finding`: the platform page, anchored at the section of that title.
 *
 * @param {string} finding a section title from `docs/PLATFORM.md`, backticks included
 */
export const findingLink = (finding) =>
  link(inlineText(finding), `${PLATFORM_PAGE}#${slug(finding)}`)

/**
 * @param {Manifest} manifest
 * @returns {ReferencePage}
 */
const exitCodesPage = (manifest) => {
  const rows = manifest.data.exitCodes
  const withFinding = rows.filter((row) => row.finding !== "")
  const id = `${TIER}/exit-codes`
  return {
    id,
    path: `${id}.md`,
    route: routeOf(id),
    title: "Exit codes",
    description:
      "Every process exit status the CLI uses, the stable code that accompanies it, what it means, and the platform finding behind it.",
    sidebarOrder: ORDER.exitCodes,
    sidebarLabel: "Exit codes",
    source: SOURCES.manifest,
    body: sections([
      {
        title: "How to read the table",
        body: [
          `Every invocation ends with one of the ${rows.length} statuses below. The process exit status is the coarse signal for a shell; the ${code("code")} string is the stable one for a program, and it arrives in the error envelope's ${code("code")} field beside an ${code("exitCode")} that matches the process status. ${pageLink(`${TIER}/envelope`, "The envelope")} has the full shape.`,
          `${withFinding.length} of the rows name a finding. A finding is a section of the platform notes, ${link("measured platform behavior", PLATFORM_PAGE)}, that documents the condition the code reports; the link opens that section.`
        ].join("\n\n")
      },
      {
        title: "The codes",
        body: table(
          ["Exit", "Code", "Meaning", "Finding"],
          rows.map((row) => [
            code(String(row.exit)),
            row.code === null ? "none" : code(row.code),
            cell(row.meaning),
            row.finding === "" ? "none" : findingLink(row.finding)
          ])
        )
      },
      provenance(
        "manifest",
        `the output of ${code(`${manifest.data.cli} manifest`)}. This page reads ${code("data.exitCodes")}; the ${code("finding")} strings are section titles the CLI carries beside each code.`
      )
    ])
  }
}

/* =================================================================================================
 * The envelope.
 * ================================================================================================= */

/**
 * @param {Manifest} manifest
 * @returns {ReferencePage}
 */
const envelopePage = (manifest) => {
  const envelope = manifest.data.envelope
  const fieldTable = (fields) =>
    table(
      ["Field", "Meaning"],
      Object.entries(fields).map(([field, meaning]) => [code(field), cell(meaning)])
    )
  const id = `${TIER}/envelope`
  return {
    id,
    path: `${id}.md`,
    route: routeOf(id),
    title: "The envelope",
    description:
      "The one JSON object every command writes to stdout: its success shape, its error shape, and the conventions around both.",
    sidebarOrder: ORDER.envelope,
    sidebarLabel: "Envelope",
    source: SOURCES.manifest,
    body: sections([
      {
        title: "Two shapes, one discriminator",
        body: `Every command that accepts ${code("--json")} writes one envelope object to stdout. It is one of two shapes, and the field ${code(envelope.discriminator)} says which: ${codeList(Object.keys(envelope).filter((key) => key !== "discriminator"))}. Both shapes carry ${code("apiVersion")}, which this manifest sets to ${code(manifest.data.apiVersion)}.`
      },
      {
        title: "The success shape",
        body: [
          `${Object.keys(envelope.ok).length} fields. ${code("type")} names the payload shape and ${code("data")} carries it; ${pageLink(`${TIER}/response-types`, "Response types")} lists every value ${code("type")} takes.`,
          fieldTable(envelope.ok)
        ].join("\n\n")
      },
      {
        title: "The error shape",
        body: [
          `${Object.keys(envelope.error).length} fields. Branch on ${code("code")}; ${pageLink(`${TIER}/exit-codes`, "Exit codes")} lists every value it takes beside the process status it maps to.`,
          fieldTable(envelope.error)
        ].join("\n\n")
      },
      {
        title: "Conventions",
        body: [
          `The manifest states ${manifest.data.conventions.length} conventions about the envelope, quoted here as the CLI publishes them:`,
          bullets(manifest.data.conventions.map((convention) => inlineText(convention)))
        ].join("\n\n")
      },
      provenance(
        "manifest",
        `the output of ${code(`${manifest.data.cli} manifest`)}. This page reads ${code("data.envelope")} and ${code("data.conventions")}.`
      )
    ])
  }
}

/* =================================================================================================
 * The wire schema.
 * ================================================================================================= */

/** The wire-schema page's section titles, so a `$ref` can link to a definition by anchor. */
const SCHEMA_TITLES = Object.freeze(["The document", "Routes", "Types", "Provenance"])

/** @param {ReadonlyArray<string>} names @param {string} name */
const definitionAnchor = (names, name) => {
  const section = SCHEMA_TITLES.indexOf("Types") + 1
  const at = names.indexOf(name)
  if (at === -1) throw new Error(`${SOURCES.schema}: a $ref points at an undefined type ${name}`)
  return `#${slug(sectionHeading(`${section}.${at + 1}.`, name))}`
}

/** @param {string} ref */
const refName = (ref) => {
  const prefix = "#/$defs/"
  if (!ref.startsWith(prefix)) throw new Error(`${SOURCES.schema}: unsupported $ref ${ref}`)
  return ref.slice(prefix.length)
}

/**
 * A property's type, rendered. A `$ref` links to the definition's own subsection on this page.
 *
 * @param {SchemaProperty} property
 * @param {ReadonlyArray<string>} names every definition name, for anchors
 * @returns {string}
 */
export const typeText = (property, names) => {
  const format = property.format === undefined ? "" : ` (${code(property.format)})`
  if (property.$ref !== undefined) {
    const name = refName(property.$ref)
    return link(code(name), definitionAnchor(names, name))
  }
  if (property.anyOf !== undefined) {
    return property.anyOf.map((variant) => typeText(variant, names)).join(" or ")
  }
  if (Array.isArray(property.type)) return `${property.type.join(" or ")}${format}`
  if (property.type === "array") {
    return `array of ${property.items === undefined ? "any" : typeText(property.items, names)}`
  }
  return `${property.type ?? "any"}${format}`
}

/**
 * A rustdoc intra-doc reference, `[\`Name\`]`, pointed at the named type's subsection when the schema
 * defines it. schemars copies doc comments verbatim, so the description of `DiskHealth` reads "the
 * disk half of [`Health`]": a shortcut reference with no definition, which Markdown prints as
 * literal brackets. Left alone when the name is not a definition, because inventing a target would
 * be worse than printing the brackets.
 *
 * @param {string} text
 * @param {ReadonlyArray<string>} names
 */
const rustdocLinks = (text, names) =>
  text.replace(/\[`([A-Za-z0-9_]+)`\](?!\()/g, (match, name) =>
    names.includes(name) ? link(code(name), definitionAnchor(names, name)) : match
  )

/**
 * @param {string} name
 * @param {SchemaDefinition} definition
 * @param {ReadonlyArray<string>} names
 * @returns {Section}
 */
const definitionSection = (name, definition, names) => {
  const required = new Set(definition.required ?? [])
  const properties = Object.entries(definition.properties ?? {})
  const parts = [paragraphs(rustdocLinks(definition.description ?? "", names))]
  if (definition.enum !== undefined) {
    parts.push(`A string with ${definition.enum.length} values: ${codeList(definition.enum)}.`)
  } else if (definition.oneOf !== undefined) {
    parts.push(
      `A string with ${definition.oneOf.length} values:`,
      table(
        ["Value", "Meaning"],
        definition.oneOf.map((variant) => [code(variant.const), cell(variant.description ?? "")])
      )
    )
  } else if (properties.length === 0) {
    parts.push(`An object with no declared properties.`)
  } else {
    parts.push(
      `An object with ${properties.length} properties, ${required.size} of them required.`,
      table(
        ["Property", "Type", "Required", "Description"],
        properties.map(([field, property]) => [
          code(field),
          typeText(property, names),
          required.has(field) ? "yes" : "no",
          property.description === undefined ? "" : cell(rustdocLinks(property.description, names))
        ])
      )
    )
  }
  return { title: name, body: parts.filter((part) => part !== "").join("\n\n") }
}

/**
 * @param {Schema} schema
 * @returns {ReferencePage}
 */
const wireSchemaPage = (schema) => {
  const names = Object.keys(schema.$defs)
  const id = `${TIER}/wire-schema`
  const titled = (title, body, children) => {
    if (!SCHEMA_TITLES.includes(title)) throw new Error(`unlisted wire-schema section ${title}`)
    return { title, body, children }
  }
  return {
    id,
    path: `${id}.md`,
    route: routeOf(id),
    title: "Wire schema",
    description:
      "The daemon's JSON schema: every route it serves and every request and response type, as its own serde types declare them.",
    sidebarOrder: ORDER.wireSchema,
    sidebarLabel: "Wire schema",
    source: SOURCES.schema,
    body: sections([
      titled(
        "The document",
        [
          `${inlineText(schema.title)}, generated from ${inlineText(schema.generated_from)}. The same document is served at ${link(code("schema.json"), "/schema.json")}, and the daemon answers every response with the ${code(schema.version_header)} header so a client can check which version it is talking to.`,
          table(
            ["Field", "Value"],
            [
              ["`$schema`", code(schema.$schema)],
              ["`protocol_version`", code(schema.protocol_version)],
              ["`daemon_version`", code(schema.daemon_version)],
              ["`version_header`", code(schema.version_header)],
              ["`hook_prefix`", code(schema.hook_prefix)]
            ]
          )
        ].join("\n\n")
      ),
      titled(
        "Routes",
        [
          `The daemon serves ${schema.routes.length} routes. Each request and response body named below is one of the types in the next section.`,
          table(
            ["Method", "Path", "Auth", "Summary"],
            schema.routes.map((route) => [
              code(route.method),
              code(route.path),
              cell(route.auth),
              cell(route.summary)
            ])
          )
        ].join("\n\n")
      ),
      titled(
        "Types",
        `${names.length} types under ${code("$defs")}, one subsection each, in the order the schema declares them. A type column that names another type links to its subsection.`,
        names.map((name) => definitionSection(name, schema.$defs[name], names))
      ),
      provenance(
        "schema",
        `the daemon's JSON schema, derived from its own serde types. This page reads the root fields, ${code("routes")} and ${code("$defs")}.`
      )
    ])
  }
}

/* =================================================================================================
 * The overview.
 * ================================================================================================= */

/**
 * @param {Manifest} manifest
 * @param {Schema} schema
 * @returns {ReferencePage}
 */
const overviewPage = (manifest, schema) => {
  const data = manifest.data
  const types = responseTypes(manifest)
  const census = [
    [
      link("Commands", overviewAnchor("The commands")),
      "one page per subcommand",
      data.commands.length
    ],
    [
      pageLink(`${TIER}/exit-codes`, "Exit codes"),
      "process statuses and their stable codes",
      data.exitCodes.length
    ],
    [
      pageLink(`${TIER}/envelope`, "The envelope"),
      "fields across the success and error shapes",
      Object.keys(data.envelope.ok).length + Object.keys(data.envelope.error).length
    ],
    [pageLink(`${TIER}/response-types`, "Response types"), "distinct payload shapes", types.length],
    [
      pageLink(`${TIER}/wire-schema`, "Wire schema"),
      "daemon types under `$defs`",
      Object.keys(schema.$defs).length
    ]
  ]
  const titled = (title, body) => {
    if (!OVERVIEW_TITLES.includes(title)) throw new Error(`unlisted overview section ${title}`)
    return { title, body }
  }
  const supportsJson = data.commands.filter((command) => command.supportsJson)
  const withoutJson = data.commands.filter((command) => !command.supportsJson)
  return {
    id: TIER,
    path: `${TIER}/index.md`,
    route: routeOf(TIER),
    title: "Reference",
    description: `The ${data.cli} command surface and the daemon's wire schema, generated from the files the binaries themselves emit.`,
    sidebarOrder: ORDER.overview,
    sidebarLabel: "Overview",
    source: SOURCES.manifest,
    body: sections([
      titled(
        "What this tier is",
        [
          `This tier is generated. Its pages are built from two files the binaries emit about themselves: ${code(SOURCES.manifest)}, the output of ${code(`${data.cli} manifest`)} at version ${code(data.version)}, and ${code(SOURCES.schema)}, the daemon's JSON schema. Where a page states a count, the number is the length of an array in one of those files, so a page here cannot name a command or a code the binary does not have.`,
          table(
            ["Page", "Holds", "Members"],
            census.map(([page, holds, members]) => [page, inlineText(holds), String(members)])
          )
        ].join("\n\n")
      ),
      titled(
        "The commands",
        [
          `${code(data.cli)} accepts ${data.commands.length} commands, each with its own page. The order is the manifest's, which is also the sidebar's.`,
          table(
            ["Command", "Summary", "Response type"],
            data.commands.map((command) => [
              commandLink(command),
              cell(command.summary),
              code(command.responseType)
            ])
          )
        ].join("\n\n")
      ),
      titled(
        "Global flags",
        [
          withoutJson.length === 0
            ? `The manifest marks each command with ${code("supportsJson")}, and all ${data.commands.length} set it. So ${code("--json")} is accepted by every command, selects the envelope described on ${pageLink(`${TIER}/envelope`, "The envelope")}, and is left out of every per-command parameter table rather than repeated ${data.commands.length} times.`
            : `${supportsJson.length} of the ${data.commands.length} commands set ${code("supportsJson")}; the ones that do not are ${codeList(withoutJson.map((command) => command.name))}. Each command page states which case it is in.`,
          `The manifest names no other command-wide flag, so a flag a command page does not list is not part of that command's surface as the manifest states it.`
        ].join("\n\n")
      ),
      titled(
        "The annotated pages",
        [
          `${ANNOTATED_PAGES.length} further pages share this directory and are not generated from the manifest. They were produced by a per-file documentation pass over the source tree, and every factual claim in them carries a ${code("path:line")} citation:`,
          bullets(ANNOTATED_PAGES.map((page) => pageLink(page.id, code(page.label))))
        ].join("\n\n")
      ),
      provenance(
        "manifest",
        `the output of ${code(`${data.cli} manifest`)}, together with ${code(SOURCES.schema)} for the wire-schema page.`
      )
    ])
  }
}

/* =================================================================================================
 * The tier.
 * ================================================================================================= */

/**
 * Every page in the tier, in a stable order: the overview, one page per command in manifest order,
 * then the cross-cutting pages.
 *
 * @param {Manifest} manifest a validated manifest
 * @param {Schema} schema a validated schema
 * @returns {ReadonlyArray<ReferencePage>}
 */
export const referencePages = (manifest, schema) => [
  overviewPage(manifest, schema),
  ...manifest.data.commands.map((command, index) => commandPage(manifest, command, index)),
  envelopePage(manifest),
  exitCodesPage(manifest),
  responseTypesPage(manifest),
  wireSchemaPage(schema)
]
