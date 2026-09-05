// SPDX-License-Identifier: Apache-2.0
import { readFileSync } from "node:fs"
import { dirname, join } from "node:path"
import { fileURLToPath } from "node:url"
import { describe, expect, it } from "vitest"

/**
 * The toolchain versions this project declares in two places, and the assertion that they agree.
 *
 * Two CI failures in a row came from this gap and neither could reproduce locally, which is the whole
 * reason the file exists. A developer's shell runs whatever Node `mise` resolved — 22.22.3 here — so it
 * satisfies every floor by accident and says nothing about the version CI pins. The runner pins one
 * exact version, and a floor raised in `package.json` without raising that pin fails 18 seconds into the
 * job with a message about a package manager rather than about a version mismatch.
 *
 * The two failures, both measured on this branch:
 *
 *   `No pnpm version is specified`                       — the action read no `packageManager` field.
 *   `This version of pnpm requires at least Node.js v22.13` — the pin was 22.12, Astro's floor, while
 *                                                             pnpm 11.21 wants one minor higher.
 *
 * So this reads the workflow and the manifest as text and compares them. It is the same shape as the
 * repository's other two-places-must-agree guards, and like those it exists because the disagreement is
 * invisible until something far away breaks.
 */

const root = dirname(dirname(fileURLToPath(import.meta.url)))
const WORKFLOW = join(root, "..", ".github", "workflows", "docs.yml")

interface Manifest {
  readonly engines?: { readonly node?: string }
  readonly packageManager?: string
}

const manifest = (): Manifest =>
  JSON.parse(readFileSync(join(root, "package.json"), "utf8")) as Manifest

const workflow = (): string => readFileSync(WORKFLOW, "utf8")

/** `>=22.13` -> [22, 13, 0]. Only the `>=` form is accepted, because that is what a floor is. */
const floor = (range: string): ReadonlyArray<number> => {
  const match = /^>=\s*(\d+)\.(\d+)(?:\.(\d+))?$/.exec(range.trim())
  if (match === null) {
    throw new Error(
      `engines.node is \`${range}\`, which this gate cannot compare. Write it as \`>=major.minor\`, ` +
        "because a floor is the only form that makes the workflow pin checkable against it."
    )
  }
  return [Number(match[1]), Number(match[2]), Number(match[3] ?? 0)]
}

/** `'22.13'` -> [22, 13, 0]. */
const pinned = (version: string): ReadonlyArray<number> => {
  const parts = version.trim().split(".").map(Number)
  if (parts.length < 2 || parts.some((part) => !Number.isInteger(part))) {
    throw new Error(
      `the workflow pins node-version \`${version}\`, which is not major.minor[.patch]`
    )
  }
  return [parts[0] ?? 0, parts[1] ?? 0, parts[2] ?? 0]
}

const atLeast = (candidate: ReadonlyArray<number>, minimum: ReadonlyArray<number>): boolean => {
  for (let at = 0; at < 3; at += 1) {
    const left = candidate[at] ?? 0
    const right = minimum[at] ?? 0
    if (left !== right) return left > right
  }
  return true
}

describe("the declared toolchain agrees with the one CI installs", () => {
  it("pins a node-version in the docs workflow", () => {
    /*
     * Read as text rather than through a YAML parser: adding a parser to assert one scalar is a
     * dependency for a regex, and the pin is a single unambiguous line in a file this repo owns.
     */
    const found = /^\s*node-version:\s*'([^']+)'\s*$/m.exec(workflow())
    expect(found, `${WORKFLOW} declares no quoted node-version`).not.toBeNull()
  })

  it("installs a node that satisfies `engines.node`", () => {
    const declared = manifest().engines?.node
    expect(declared, "site/package.json declares no engines.node").toBeTypeOf("string")
    const found = /^\s*node-version:\s*'([^']+)'\s*$/m.exec(workflow())?.[1] ?? ""
    expect(
      atLeast(pinned(found), floor(declared as string)),
      `the workflow pins node ${found}, below the declared floor ${declared}`
    ).toBe(true)
  })

  it("pins the package manager where the action reads it", () => {
    /*
     * `pnpm/action-setup` reads `packageManager` from the file named by `package_json_file`, whose
     * default is a `package.json` at the REPOSITORY root. This project's manifest is one directory
     * down, so the default finds nothing and the action refuses to guess a version. Both halves are
     * asserted, because either one alone fails the job.
     */
    const declared = manifest().packageManager
    expect(declared, "site/package.json declares no packageManager").toMatch(/^pnpm@\d+\.\d+\.\d+$/)
    expect(workflow()).toContain("package_json_file: site/package.json")
  })

  it("runs on a node that satisfies the floor it declares", () => {
    // The floor is a claim about what this project needs, so the interpreter reading it has to qualify.
    const declared = manifest().engines?.node as string
    expect(atLeast(pinned(process.version.replace(/^v/, "")), floor(declared))).toBe(true)
  })
})
