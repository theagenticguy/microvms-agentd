#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
/**
 * The brace gate: refuses a Markdown corpus that carries a bare `{` where an MDX parser reads a JSX
 * expression.
 *
 * `starlight-md-txt` parses every page's body through `remark-parse` plus `remark-mdx`
 * unconditionally — authored and generated alike, `.md` as well as `.mdx` — so a brace in flow or text
 * position is handed to acorn as the start of an expression. `## GET /v1/exec/{id}` is not one, and the
 * build fails with `Could not parse expression with acorn`: a message that names neither the page nor
 * the field it came from, on a build whose Markdown looked correct in every editor and every renderer.
 *
 * The fix is always the same and always local: wrap the braced span in backticks. Inside a code span or
 * a fence the brace is leaf content the MDX parser never enters, so the page keeps reading the way its
 * author wrote it. That is why a Mermaid diagram containing `A{Decision}` builds and a REST path in a
 * heading does not.
 *
 * Run it BEFORE the site build. A gate that reports `file:line` in 30 ms is worth more than a build
 * that reports a parser position 90 seconds in.
 *
 *   node scripts/brace-gate.mjs src/content/docs
 *   node scripts/brace-gate.mjs src/content/docs --json
 *
 * Exit 0: clean. Exit 1: offenders, one `file:line:column` per line on stdout. Exit 2: bad invocation.
 */

import { readdirSync, readFileSync, statSync } from "node:fs"
import { join, relative, resolve } from "node:path"
import { pathToFileURL } from "node:url"

/** Extensions handed to the MDX parser. A `.txt` beside them is not. */
const EXTENSIONS = [".md", ".mdx"]

/**
 * A fenced block, opener through the matching closer of the same run.
 *
 * An unterminated fence never matches, so its body is scanned as prose. That is the useful direction of
 * the error: an unclosed fence is itself a defect, and a brace inside one really does reach the parser.
 */
const FENCED = /^ {0,3}(`{3,}|~{3,})[^\n]*\n[\s\S]*?^ {0,3}\1[^\n]*$/gm

/** An inline code span: a backtick run closed by a run of the same length, which may cross lines. */
const CODE_SPAN = /(`+)(?:(?!\1)[\s\S])*?\1/g

/** Leading YAML frontmatter, which reaches the frontmatter parser and never the expression parser. */
const FRONTMATTER = /^---\r?\n[\s\S]*?\r?\n---[^\n]*(?:\r?\n|$)/

/**
 * Blank a region while keeping every newline, so a later line number is the line number in the file.
 *
 * Deleting the region instead — which is enough when the only question is whether a brace exists — moves
 * every subsequent line and makes the report point at the wrong one.
 */
const blank = (text) => text.replace(/[^\n]/g, " ")

/** The body as the MDX expression parser sees it: code and frontmatter blanked, lines preserved. */
export const maskedBody = (markdown) =>
  markdown.replace(FRONTMATTER, blank).replace(FENCED, blank).replace(CODE_SPAN, blank)

/**
 * Every brace the MDX parser reads as opening an expression, with the line it sits on.
 *
 * A backslash-escaped `\{` is literal text in MDX and is skipped: flagging it would send an author to
 * re-fix a page that already builds.
 */
export const braceOffenders = (markdown) => {
  const masked = maskedBody(markdown)
  const source = markdown.split("\n")
  return masked.split("\n").flatMap((line, index) => {
    const columns = [...line.matchAll(/\{/g)]
      .map((match) => match.index ?? 0)
      .filter((column) => line[column - 1] !== "\\")
    if (columns.length === 0) return []
    return [{ line: index + 1, column: (columns[0] ?? 0) + 1, text: (source[index] ?? "").trim() }]
  })
}

const walk = (directory) =>
  readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) return entry.name.startsWith(".") ? [] : walk(path)
    return EXTENSIONS.some((extension) => entry.name.endsWith(extension)) ? [path] : []
  })

const main = () => {
  const args = process.argv.slice(2)
  const asJson = args.includes("--json")
  const target = resolve(args.find((argument) => !argument.startsWith("--")) ?? "src/content/docs")

  if (!statSync(target, { throwIfNoEntry: false })?.isDirectory()) {
    process.stderr.write(`brace-gate: \`${target}\` is not a directory\n`)
    process.stderr.write("usage: brace-gate.mjs <directory> [--json]\n")
    process.exit(2)
  }

  const files = walk(target)

  /*
   * Report each path so it is clickable from where the gate was run: relative to the working directory,
   * which is what an editor and a CI annotation resolve. A run from outside the tree would print a
   * ladder of `../` segments that buries the filename, so that case falls back to the path relative to
   * the tree itself. A finding nobody can open is a finding nobody acts on.
   */
  const label = (file) => {
    const fromCwd = relative(process.cwd(), file)
    return fromCwd.startsWith("..") ? relative(target, file) : fromCwd
  }

  const findings = files.flatMap((file) =>
    braceOffenders(readFileSync(file, "utf8")).map((offender) => ({ file: label(file), ...offender }))
  )

  if (asJson) {
    process.stdout.write(
      `${JSON.stringify(
        { scanned: files.length, findings: findings.length, offenders: findings },
        null,
        2
      )}\n`
    )
  } else if (findings.length === 0) {
    process.stdout.write(`brace-gate: ${files.length} files, no bare brace\n`)
  } else {
    for (const { file, line, column, text } of findings) {
      process.stdout.write(`${file}:${line}:${column}  ${text}\n`)
    }
    const pages = new Set(findings.map((finding) => finding.file)).size
    process.stdout.write(
      `\nbrace-gate: ${findings.length} bare braces across ${pages} of ${files.length} files.\n` +
        "Wrap each braced span in backticks; inside a code span the MDX parser never enters it.\n"
    )
  }

  process.exit(findings.length === 0 ? 0 : 1)
}

/*
 * Run the scan only when this file IS the entry point. Without the guard, importing `braceOffenders` to
 * test it scans the corpus and calls `process.exit`, which a test runner reports as a suite that failed
 * to load — so the negative controls that prove this gate works could not exist.
 */
if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main()
}
