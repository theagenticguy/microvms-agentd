// SPDX-License-Identifier: Apache-2.0
import { describe, expect, it } from "vitest"

import { braceOffenders, maskedBody } from "../scripts/brace-gate.mjs"

/**
 * The negative controls for the brace gate.
 *
 * The gate asserts over a found set — "no bare brace anywhere" — and every assertion over a found set is
 * green on an empty corpus and green on a broken scanner. Without a poison case, "the tree is clean" and
 * "the check does not work" are the same result.
 *
 * The poison is a synthetic input to a pure function, so nothing on disk moves and the control cannot rot
 * away from the gate it verifies: both read the same two exports.
 */

describe("the brace gate", () => {
  it("finds the REST path in a heading, which is how this corpus breaks the build", () => {
    const offenders = braceOffenders("# Title\n\n## GET /v1/exec/{id}/stream\n\nProse.\n")
    expect(offenders).toHaveLength(1)
    expect(offenders[0]?.line).toBe(3)
    expect(offenders[0]?.text).toContain("/v1/exec/{id}/stream")
  })

  it("accepts the same path inside backticks, which is the only legal repair", () => {
    /*
     * The other half of the pair. Without it the gate is satisfied by a scanner that refuses every brace,
     * including the ones the build accepts — and the fix the message recommends would not clear it.
     */
    expect(braceOffenders("## GET `/v1/exec/{id}/stream`\n")).toEqual([])
    expect(braceOffenders("A fence:\n\n```mermaid\ngraph TD\n  A{Decision} --> B\n```\n")).toEqual([])
  })

  it("reports the line the brace is on, not the line it would be on with code deleted", () => {
    /*
     * The mask blanks fenced and inline code while KEEPING every newline. Deleting the regions instead is
     * enough to answer "is there a brace", and it moves every later line so the report points at the
     * wrong one — which sends a reader to a line that looks fine.
     */
    const body = ["# Title", "", "```json", '{ "a": 1 }', "```", "", "## GET /v1/x/{id}", ""].join("\n")
    expect(maskedBody(body).split("\n")).toHaveLength(body.split("\n").length)
    expect(braceOffenders(body).map((offender) => offender.line)).toEqual([7])
  })

  it("skips a brace the MDX parser reads as literal text", () => {
    expect(braceOffenders("Escaped: \\{not an expression}\n")).toEqual([])
  })

  it("ignores frontmatter, which reaches a different parser entirely", () => {
    expect(braceOffenders('---\ntitle: "a {brace} in a title"\n---\n\nProse.\n')).toEqual([])
  })
})
