// SPDX-License-Identifier: Apache-2.0
/**
 * The two source-of-truth documents behind the Reference tier, loaded and checked for shape.
 *
 * `docs/manifest.json` is what `microvm manifest` prints: the command surface, the exit codes, the
 * envelope and the conventions. `docs/schema.json` is the daemon's JSON schema, from its own serde
 * types. Both are generated artifacts, and a generated artifact drifts in one of two ways: the content
 * changes, which `mise run manifest:check` and `mise run schema:check` catch, or the SHAPE changes,
 * which is what this module catches. A page builder reading `command.parameters` after the CLI renamed
 * the field to `params` would print an empty table for every command and the build would stay green.
 *
 * Every check names the path that failed. A validator that says "invalid manifest" sends the reader to
 * diff two large files by eye.
 */

import { readFileSync } from "node:fs"

/**
 * @typedef {object} Parameter
 * @property {string} name
 * @property {boolean} positional
 * @property {boolean} required
 * @property {string} type
 * @property {string | null} default
 * @property {ReadonlyArray<string> | null} choices
 * @property {string} help
 */

/**
 * @typedef {object} AlternateResponse
 * @property {string} when the flag that selects it, e.g. `--stream`
 * @property {string} responseType
 * @property {ReadonlyArray<string>} responseKeys
 * @property {string} stdout what stdout carries instead of one envelope
 */

/**
 * @typedef {object} Command
 * @property {string} name
 * @property {string} summary
 * @property {ReadonlyArray<Parameter>} parameters
 * @property {string} responseType
 * @property {ReadonlyArray<string>} responseKeys
 * @property {AlternateResponse | null} alternateResponse
 * @property {boolean} supportsJson
 */

/**
 * @typedef {object} ExitCode
 * @property {number} exit
 * @property {string | null} code
 * @property {string} meaning
 * @property {string} finding a `docs/PLATFORM.md` section title, or empty
 */

/**
 * @typedef {object} Envelope
 * @property {string} discriminator
 * @property {Readonly<Record<string, string>>} ok
 * @property {Readonly<Record<string, string>>} error
 */

/**
 * @typedef {object} ManifestData
 * @property {string} apiVersion
 * @property {string} cli
 * @property {string} version
 * @property {ReadonlyArray<Command>} commands
 * @property {ReadonlyArray<ExitCode>} exitCodes
 * @property {Envelope} envelope
 * @property {ReadonlyArray<string>} conventions
 */

/**
 * @typedef {object} Manifest
 * @property {string} status
 * @property {string} type
 * @property {ManifestData} data
 */

/**
 * @typedef {object} SchemaProperty
 * @property {string | ReadonlyArray<string>} [type]
 * @property {string} [$ref]
 * @property {ReadonlyArray<{ type?: string, $ref?: string }>} [anyOf]
 * @property {{ type?: string, $ref?: string }} [items]
 * @property {string} [format]
 * @property {string} [description]
 */

/**
 * @typedef {object} SchemaDefinition
 * @property {string} [description]
 * @property {string | ReadonlyArray<string>} [type]
 * @property {Readonly<Record<string, SchemaProperty>>} [properties]
 * @property {ReadonlyArray<string>} [required]
 * @property {ReadonlyArray<string>} [enum]
 * @property {ReadonlyArray<{ const: string, description?: string }>} [oneOf]
 */

/**
 * @typedef {object} Route
 * @property {string} method
 * @property {string} path
 * @property {string} auth
 * @property {string} summary
 */

/**
 * @typedef {object} Schema
 * @property {string} $schema
 * @property {string} title
 * @property {string} generated_from
 * @property {string} protocol_version
 * @property {string} daemon_version
 * @property {string} version_header
 * @property {string} hook_prefix
 * @property {ReadonlyArray<Route>} routes
 * @property {Readonly<Record<string, SchemaDefinition>>} $defs
 */

/** The `type` field the manifest envelope carries, so a different command's output is refused. */
export const MANIFEST_TYPE = "microvm.manifest"

/** Repo-relative paths, named on every generated page so a reader can go regenerate the source. */
export const SOURCES = {
  manifest: "docs/manifest.json",
  schema: "docs/schema.json"
}

/** The task that regenerates each source, quoted on the page beside the source. */
export const REGENERATE = {
  manifest: "mise run manifest",
  schema: "mise run schema"
}

/**
 * @param {string} source
 * @param {string} path
 * @param {string} expected
 * @param {unknown} actual
 */
const drift = (source, path, expected, actual) =>
  new Error(
    `${source} has drifted from the shape this generator reads: \`${path}\` should be ${expected}, ` +
      `got ${describe(actual)}. Regenerate the file, then update site/scripts/reference/manifest.mjs ` +
      "if the CLI changed its output on purpose."
  )

/** @param {unknown} value */
const describe = (value) => {
  if (value === null) return "null"
  if (Array.isArray(value)) return `an array of ${value.length}`
  if (typeof value === "object") return `an object with keys ${Object.keys(value).join(", ")}`
  if (typeof value === "string")
    return JSON.stringify(value.length > 60 ? `${value.slice(0, 57)}...` : value)
  return `${typeof value} ${String(value)}`
}

/** @param {unknown} value @returns {value is Record<string, unknown>} */
const isRecord = (value) => typeof value === "object" && value !== null && !Array.isArray(value)

/**
 * A small checker bound to one source file, so every message names it.
 *
 * @param {string} source
 */
const checker = (source) => {
  const check = {
    /** @param {unknown} value @param {string} path @returns {Record<string, unknown>} */
    record: (value, path) => {
      if (!isRecord(value)) throw drift(source, path, "an object", value)
      return value
    },
    /** @param {unknown} value @param {string} path @returns {string} */
    string: (value, path) => {
      if (typeof value !== "string") throw drift(source, path, "a string", value)
      return value
    },
    /** @param {unknown} value @param {string} path @returns {string} */
    nonEmptyString: (value, path) => {
      const text = check.string(value, path)
      if (text.trim() === "") throw drift(source, path, "a non-empty string", value)
      return text
    },
    /** @param {unknown} value @param {string} path @returns {boolean} */
    boolean: (value, path) => {
      if (typeof value !== "boolean") throw drift(source, path, "a boolean", value)
      return value
    },
    /** @param {unknown} value @param {string} path @returns {number} */
    integer: (value, path) => {
      if (typeof value !== "number" || !Number.isInteger(value)) {
        throw drift(source, path, "an integer", value)
      }
      return value
    },
    /** @param {unknown} value @param {string} path @returns {ReadonlyArray<unknown>} */
    array: (value, path) => {
      if (!Array.isArray(value)) throw drift(source, path, "an array", value)
      return value
    },
    /** @param {unknown} value @param {string} path @returns {ReadonlyArray<unknown>} */
    nonEmptyArray: (value, path) => {
      const list = check.array(value, path)
      if (list.length === 0) throw drift(source, path, "a non-empty array", value)
      return list
    },
    /** @param {unknown} value @param {string} path @returns {ReadonlyArray<string>} */
    strings: (value, path) =>
      check.array(value, path).map((item, at) => check.string(item, `${path}[${at}]`)),
    /** @param {unknown} value @param {string} path @returns {string | null} */
    stringOrNull: (value, path) => (value === null ? null : check.string(value, path))
  }
  return check
}

/**
 * The manifest, checked field by field, or a thrown `Error` naming the first field that drifted.
 *
 * Pure: a test hands it a corrupted copy and reads the message. The returned value is the same object
 * the caller passed, now known to have the shape the page builders read.
 *
 * @param {unknown} parsed
 * @param {string} [source] the path named in messages
 * @returns {Manifest}
 */
export const validateManifest = (parsed, source = SOURCES.manifest) => {
  const check = checker(source)
  const root = check.record(parsed, "$")
  const status = check.string(root.status, "status")
  if (status !== "ok") throw drift(source, "status", JSON.stringify("ok"), status)
  const type = check.string(root.type, "type")
  if (type !== MANIFEST_TYPE) throw drift(source, "type", JSON.stringify(MANIFEST_TYPE), type)
  const data = check.record(root.data, "data")

  check.nonEmptyString(data.apiVersion, "data.apiVersion")
  check.nonEmptyString(data.cli, "data.cli")
  check.nonEmptyString(data.version, "data.version")

  const commands = check.nonEmptyArray(data.commands, "data.commands")
  const names = new Set()
  commands.forEach((entry, at) => {
    const where = `data.commands[${at}]`
    const command = check.record(entry, where)
    const name = check.nonEmptyString(command.name, `${where}.name`)
    if (names.has(name)) throw drift(source, `${where}.name`, "a unique command name", name)
    names.add(name)
    check.nonEmptyString(command.summary, `${where}.summary`)
    check.nonEmptyString(command.responseType, `${where}.responseType`)
    check.strings(command.responseKeys, `${where}.responseKeys`)
    check.boolean(command.supportsJson, `${where}.supportsJson`)
    check.array(command.parameters, `${where}.parameters`).forEach((item, index) => {
      const at = `${where}.parameters[${index}]`
      const parameter = check.record(item, at)
      check.nonEmptyString(parameter.name, `${at}.name`)
      check.boolean(parameter.positional, `${at}.positional`)
      check.boolean(parameter.required, `${at}.required`)
      check.nonEmptyString(parameter.type, `${at}.type`)
      check.stringOrNull(parameter.default, `${at}.default`)
      if (parameter.choices !== null) check.strings(parameter.choices, `${at}.choices`)
      check.string(parameter.help, `${at}.help`)
    })
    if (command.alternateResponse !== null) {
      const at = `${where}.alternateResponse`
      const alternate = check.record(command.alternateResponse, at)
      check.nonEmptyString(alternate.when, `${at}.when`)
      check.nonEmptyString(alternate.responseType, `${at}.responseType`)
      check.strings(alternate.responseKeys, `${at}.responseKeys`)
      check.nonEmptyString(alternate.stdout, `${at}.stdout`)
    }
  })

  const exits = new Set()
  check.nonEmptyArray(data.exitCodes, "data.exitCodes").forEach((entry, at) => {
    const where = `data.exitCodes[${at}]`
    const row = check.record(entry, where)
    const exit = check.integer(row.exit, `${where}.exit`)
    if (exits.has(exit)) throw drift(source, `${where}.exit`, "a unique exit status", exit)
    exits.add(exit)
    check.stringOrNull(row.code, `${where}.code`)
    check.nonEmptyString(row.meaning, `${where}.meaning`)
    check.string(row.finding, `${where}.finding`)
  })

  const envelope = check.record(data.envelope, "data.envelope")
  check.nonEmptyString(envelope.discriminator, "data.envelope.discriminator")
  for (const shape of ["ok", "error"]) {
    const fields = check.record(envelope[shape], `data.envelope.${shape}`)
    if (Object.keys(fields).length === 0) {
      throw drift(source, `data.envelope.${shape}`, "an object with at least one field", fields)
    }
    for (const [field, meaning] of Object.entries(fields)) {
      check.nonEmptyString(meaning, `data.envelope.${shape}.${field}`)
    }
  }

  check.nonEmptyArray(data.conventions, "data.conventions").forEach((item, at) => {
    check.nonEmptyString(item, `data.conventions[${at}]`)
  })

  return /** @type {Manifest} */ (parsed)
}

/**
 * The wire schema, checked for the shape the wire-schema page reads.
 *
 * Deliberately shallow below `$defs`: JSON Schema admits many spellings for one type, and the page
 * renders whichever it finds. What must hold is that `$defs` is a non-empty object of objects, every
 * `properties` entry is an object, and every `required` list names a declared property.
 *
 * @param {unknown} parsed
 * @param {string} [source]
 * @returns {Schema}
 */
export const validateSchema = (parsed, source = SOURCES.schema) => {
  const check = checker(source)
  const root = check.record(parsed, "$")
  check.nonEmptyString(root.$schema, "$schema")
  check.nonEmptyString(root.title, "title")
  check.nonEmptyString(root.generated_from, "generated_from")
  check.nonEmptyString(root.protocol_version, "protocol_version")
  check.nonEmptyString(root.daemon_version, "daemon_version")
  check.nonEmptyString(root.version_header, "version_header")
  check.nonEmptyString(root.hook_prefix, "hook_prefix")
  check.nonEmptyArray(root.routes, "routes").forEach((entry, at) => {
    const where = `routes[${at}]`
    const route = check.record(entry, where)
    check.nonEmptyString(route.method, `${where}.method`)
    check.nonEmptyString(route.path, `${where}.path`)
    check.nonEmptyString(route.auth, `${where}.auth`)
    check.nonEmptyString(route.summary, `${where}.summary`)
  })

  const defs = check.record(root.$defs, "$defs")
  if (Object.keys(defs).length === 0) throw drift(source, "$defs", "a non-empty object", defs)
  for (const [name, entry] of Object.entries(defs)) {
    const where = `$defs.${name}`
    const definition = check.record(entry, where)
    if (
      definition.type === undefined &&
      definition.oneOf === undefined &&
      definition.enum === undefined
    ) {
      throw drift(source, where, "a definition with `type`, `oneOf` or `enum`", definition)
    }
    if (definition.description !== undefined)
      check.string(definition.description, `${where}.description`)
    const properties =
      definition.properties === undefined
        ? {}
        : check.record(definition.properties, `${where}.properties`)
    for (const [field, property] of Object.entries(properties)) {
      check.record(property, `${where}.properties.${field}`)
    }
    if (definition.required !== undefined) {
      for (const field of check.strings(definition.required, `${where}.required`)) {
        if (!(field in properties)) {
          throw drift(source, `${where}.required`, `a list of declared properties`, field)
        }
      }
    }
    if (definition.enum !== undefined) check.nonEmptyArray(definition.enum, `${where}.enum`)
    if (definition.oneOf !== undefined) {
      check.nonEmptyArray(definition.oneOf, `${where}.oneOf`).forEach((item, at) => {
        const variant = check.record(item, `${where}.oneOf[${at}]`)
        check.string(variant.const, `${where}.oneOf[${at}].const`)
      })
    }
  }

  return /** @type {Schema} */ (parsed)
}

/**
 * @param {string} path
 * @param {string} source
 * @returns {unknown}
 */
const readJson = (path, source) => {
  let text
  try {
    text = readFileSync(path, "utf8")
  } catch (cause) {
    throw new Error(
      `${source} is absent or unreadable at ${path}. Regenerate it with \`${
        source === SOURCES.schema ? REGENERATE.schema : REGENERATE.manifest
      }\` from the repository root.`,
      { cause }
    )
  }
  try {
    return JSON.parse(text)
  } catch (cause) {
    throw new Error(
      `${source} at ${path} is not JSON: ${cause instanceof Error ? cause.message : cause}`
    )
  }
}

/**
 * Read and validate `docs/manifest.json`.
 *
 * @param {string} path
 * @returns {Manifest}
 */
export const loadManifest = (path) => validateManifest(readJson(path, SOURCES.manifest))

/**
 * Read and validate `docs/schema.json`.
 *
 * @param {string} path
 * @returns {Schema}
 */
export const loadSchema = (path) => validateSchema(readJson(path, SOURCES.schema))
