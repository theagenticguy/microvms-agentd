// SPDX-License-Identifier: Apache-2.0
import { defineConfig } from "vitest/config"

/**
 * The suites read `dist/`, so they are worthless without a build and must say so rather than skip.
 * `tests/agent-surface.test.ts` throws with the command to run when a file it needs is absent; wiring
 * the build into the task runner is what keeps an ordered run honest. See `mise run docs:check`.
 */
export default defineConfig({
  test: {
    include: ["tests/**/*.test.ts", "tests/**/*.test.mjs"],
    environment: "node"
  }
})
