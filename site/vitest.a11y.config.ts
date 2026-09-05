// SPDX-License-Identifier: Apache-2.0
import { defineConfig } from "vitest/config"

/**
 * The browser tier: the accessibility audit and the layout-stability probe. One worker, never
 * parallel. Each suite opens a Chromium and a static server on a real port, and running them at once
 * would put several browsers on a two-core CI runner competing for the same measurement. The layout
 * probe is the sharper case for that: it measures WHEN things move, so a second browser stealing the
 * CPU is not noise around its answer, it is a different answer.
 *
 * The timeouts are long because each suite visits every audited page once in `beforeAll` and a cold
 * Chromium on a CI runner can take tens of seconds to first paint. Reached through `mise run
 * docs:a11y`, and through `mise run docs:gate` after the node tier.
 */
export default defineConfig({
  test: {
    include: ["tests/a11y.test.ts", "tests/layout-stability.test.ts"],
    fileParallelism: false,
    testTimeout: 300_000,
    hookTimeout: 300_000,
    environment: "node"
  }
})
