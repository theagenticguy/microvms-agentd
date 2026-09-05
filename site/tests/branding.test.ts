// SPDX-License-Identifier: Apache-2.0
import { readFileSync } from "node:fs"
import { dirname, join } from "node:path"
import { fileURLToPath } from "node:url"
import { inflateSync } from "node:zlib"
import { describe, expect, it } from "vitest"

import {
  APPLE_TOUCH_SIZE,
  GRID,
  ICO_SIZES,
  ICON_PNG_SIZE,
  MARK,
  type MarkRect,
  markArtifacts,
  markRaster,
  markSvg,
  PALETTE
} from "../src/branding/mark.ts"
import {
  BRAND_LINE,
  DESCRIPTION_BUDGET,
  INK_SECONDARY,
  OG_HEIGHT,
  OG_PADDING,
  OG_TEXT_BAND,
  ogAlt,
  ogCard,
  ogId,
  ogSlug,
  TITLE_BUDGET,
  worstCaseTextHeight
} from "../src/branding/og-card.ts"

/**
 * The mark, its committed artifacts, the palette's measured claims, and the social card.
 *
 * The artifacts are compared as PIXELS, not as bytes. A byte comparison would fail the moment a
 * different zlib packed the same image differently, which is a false drift; a pixel comparison fails
 * only when the committed icon stops drawing the current mark, which is the drift worth catching. The
 * PNG is decoded here rather than by the module under test, so a broken encoder cannot agree with
 * itself. Ported from memhtml-public's apps/docs/tests/branding.test.ts and adjusted to this mark.
 */

const root = dirname(dirname(fileURLToPath(import.meta.url)))
const asset = (file: string): Uint8Array => new Uint8Array(readFileSync(join(root, "public", file)))
const rfcCss = (): string => readFileSync(join(root, "src", "styles", "rfc.css"), "utf8")

interface Decoded {
  readonly width: number
  readonly height: number
  readonly pixels: Uint8Array
}

/** An RGBA8 PNG with a single IDAT and no interlacing, which is all `markPng` emits. */
const decodePng = (bytes: Uint8Array): Decoded => {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  expect([...bytes.subarray(0, 8)]).toEqual([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])

  let at = 8
  let header: Decoded | undefined
  const data: Array<Uint8Array> = []
  while (at < bytes.length) {
    const length = view.getUint32(at)
    const type = String.fromCharCode(...bytes.subarray(at + 4, at + 8))
    const payload = bytes.subarray(at + 8, at + 8 + length)
    if (type === "IHDR") {
      const bitDepth = payload[8]
      const colorType = payload[9]
      const interlace = payload[12]
      expect({ bitDepth, colorType, interlace }).toEqual({
        bitDepth: 8,
        colorType: 6,
        interlace: 0
      })
      header = {
        width: view.getUint32(at + 8),
        height: view.getUint32(at + 12),
        pixels: new Uint8Array(0)
      }
    }
    if (type === "IDAT") data.push(payload)
    at += length + 12
  }
  if (header === undefined) throw new Error("no IHDR")

  const raw = new Uint8Array(inflateSync(Buffer.concat(data)))
  const stride = header.width * 4
  const pixels = new Uint8Array(stride * header.height)
  for (let y = 0; y < header.height; y += 1) {
    // Every scanline in these images uses filter type 0, so the row is copied straight out.
    expect(raw[y * (stride + 1)]).toBe(0)
    pixels.set(raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1)), y * stride)
  }
  return { ...header, pixels }
}

const colorAt = ({ width, pixels }: Decoded, x: number, y: number): string => {
  const at = (y * width + x) * 4
  const hex = (channel: number | undefined): string =>
    (channel ?? 0).toString(16).padStart(2, "0").toUpperCase()
  return `#${hex(pixels[at])}${hex(pixels[at + 1])}${hex(pixels[at + 2])}`
}

/**
 * WCAG 2.x relative luminance and contrast ratio, restated here so the figures written into rfc.css
 * and mark.ts as comments are checked by arithmetic rather than trusted as prose.
 */
const linear = (channel: number): number => {
  const c = channel / 255
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4
}
const luminance = (hex: string): number => {
  const [r, g, b] = [1, 3, 5].map((at) => Number.parseInt(hex.slice(at, at + 2), 16))
  return 0.2126 * linear(r ?? 0) + 0.7152 * linear(g ?? 0) + 0.0722 * linear(b ?? 0)
}
const contrast = (a: string, b: string): number => {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x)
  return ((hi ?? 0) + 0.05) / ((lo ?? 0) + 0.05)
}

const strokes = (): ReadonlyArray<MarkRect> => MARK.filter((rect) => rect.role !== "ground")

describe("the mark", () => {
  it("draws every rectangle on whole units of the grid", () => {
    for (const rect of MARK) {
      for (const value of [rect.x, rect.y, rect.w, rect.h]) {
        expect(Number.isInteger(value)).toBe(true)
      }
      expect(rect.x + rect.w).toBeLessThanOrEqual(GRID)
      expect(rect.y + rect.h).toBeLessThanOrEqual(GRID)
      // Nothing thinner than two units, or it is a hairline at favicon size.
      expect(Math.min(rect.w, rect.h)).toBeGreaterThanOrEqual(2)
    }
  })

  /*
   * No stroke is painted over another. The frame is open at the port and the tick fills the gap edge
   * to edge, so every stroke's own center pixel carries its own fill at every raster size, and the
   * color check below can read each one back without knowing the drawing order.
   */
  it("paints no stroke over another", () => {
    const list = strokes()
    for (const [i, a] of list.entries()) {
      for (const b of list.slice(i + 1)) {
        const apart = a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y
        expect(apart, `${a.role} overlaps ${b.role}`).toBe(true)
      }
    }
  })

  /*
   * Every color the mark and the card use has to be one `rfc.css` measured a ratio for. A fourth
   * value invented here would be the one color on the site with no contrast figure behind it. The
   * three `--docs-` tokens are parsed out of the `:root` block so the CSS and the TypeScript cannot
   * name two different accents.
   */
  it("uses only the palette rfc.css declares", () => {
    const css = rfcCss()
    for (const value of [...Object.values(PALETTE), INK_SECONDARY]) {
      expect(css).toContain(value.toLowerCase())
    }
    const token = (name: string): string | undefined =>
      new RegExp(`--docs-${name}:\\s*(#[0-9a-f]{6})`).exec(css)?.[1]?.toUpperCase()
    expect(token("paper")).toBe(PALETTE.paper)
    expect(token("ink")).toBe(PALETTE.ink)
    expect(token("accent")).toBe(PALETTE.accent)
    expect(token("ink-secondary")).toBe(INK_SECONDARY)
    expect(new Set(MARK.map((rect) => rect.fill))).toEqual(new Set(Object.values(PALETTE)))
  })

  /*
   * The ratios the comments state, recomputed. Body text needs 4.5:1 (SC 1.4.3); the accent carries
   * text in aside labels and the card rule, so it is held to AAA's 7:1.
   */
  it("measures the contrast the palette claims", () => {
    expect(contrast(PALETTE.ink, PALETTE.paper)).toBeGreaterThanOrEqual(16.8)
    expect(contrast(INK_SECONDARY, PALETTE.paper)).toBeGreaterThanOrEqual(4.5)
    expect(contrast(PALETTE.accent, PALETTE.paper)).toBeGreaterThanOrEqual(7)
    expect(contrast(PALETTE.accent, PALETTE.paper)).toBeCloseTo(8.46, 2)
  })

  it("covers every pixel of a 16px raster, so nothing shows through", () => {
    const raster = markRaster(GRID)
    for (let at = 3; at < raster.length; at += 4) expect(raster[at]).toBe(255)
  })

  it("names each stroke", () => {
    expect(MARK.map((rect) => rect.role)).toEqual([
      "ground",
      "frame top",
      "frame bottom",
      "frame left",
      "frame right, above the port",
      "frame right, below the port",
      "daemon",
      "port"
    ])
  })

  it("is one accent tick, reaching the frame's outer edge", () => {
    const accent = strokes().filter((rect) => rect.fill === PALETTE.accent)
    expect(accent).toHaveLength(1)
    const [port] = accent
    expect(port?.x).toBeGreaterThanOrEqual(12)
    expect((port?.x ?? 0) + (port?.w ?? 0)).toBe(GRID)
  })
})

describe("the committed artifacts still draw the mark", () => {
  it("ships one file per declared artifact", () => {
    for (const { file } of markArtifacts()) {
      expect(asset(file).length).toBeGreaterThan(0)
    }
  })

  it("keeps favicon.svg byte-identical to the generator", () => {
    expect(readFileSync(join(root, "public", "favicon.svg"), "utf8")).toBe(markSvg())
  })

  it.for([
    { file: `icon-${ICON_PNG_SIZE}.png`, size: ICON_PNG_SIZE },
    { file: "apple-touch-icon.png", size: APPLE_TOUCH_SIZE }
  ])("draws the mark in $file", ({ file, size }) => {
    const decoded = decodePng(asset(file))
    expect([decoded.width, decoded.height]).toEqual([size, size])
    expect([...decoded.pixels]).toEqual([...markRaster(size)])
  })

  it("carries one PNG per declared size in favicon.ico, in order", () => {
    const ico = asset("favicon.ico")
    const view = new DataView(ico.buffer, ico.byteOffset, ico.byteLength)
    expect(view.getUint16(0, true)).toBe(0)
    expect(view.getUint16(2, true)).toBe(1)
    expect(view.getUint16(4, true)).toBe(ICO_SIZES.length)

    ICO_SIZES.forEach((size, at) => {
      const entry = 6 + at * 16
      expect(ico[entry]).toBe(size)
      expect(ico[entry + 1]).toBe(size)
      expect(view.getUint16(entry + 6, true)).toBe(32)
      const offset = view.getUint32(entry + 12, true)
      const length = view.getUint32(entry + 8, true)
      const decoded = decodePng(ico.subarray(offset, offset + length))
      expect([decoded.width, decoded.height]).toEqual([size, size])
      expect([...decoded.pixels]).toEqual([...markRaster(size)])
    })
  })

  /*
   * The three colors, read out of the raster at the coordinate each stroke owns.
   *
   * This is the check that would fail on a mark that encodes and decodes perfectly and draws the wrong
   * thing: a transposed rectangle, a swapped fill, a port that lost its bleed.
   */
  it("puts paper at the corners, ink on the frame and daemon, and the tick through the right edge", () => {
    const decoded = decodePng(asset(`icon-${ICON_PNG_SIZE}.png`))
    const unit = ICON_PNG_SIZE / GRID
    const center = (rect: { x: number; y: number; w: number; h: number }): [number, number] => [
      Math.round((rect.x + rect.w / 2) * unit),
      Math.round((rect.y + rect.h / 2) * unit)
    ]

    const last = ICON_PNG_SIZE - 1
    // The frame is inset, so all four corners are paper: the mark does not bleed except at the port.
    for (const [x, y] of [
      [0, 0],
      [last, 0],
      [0, last],
      [last, last]
    ] as const) {
      expect(colorAt(decoded, x, y)).toBe(PALETTE.paper)
    }
    for (const rect of strokes()) {
      const [x, y] = center(rect)
      expect(colorAt(decoded, x, y), rect.role).toBe(rect.fill)
    }
    // The gap between the frame and the daemon is paper, so the daemon reads as inside a frame.
    expect(colorAt(decoded, Math.round(5 * unit), Math.round(5 * unit))).toBe(PALETTE.paper)
    // The port reaches the right edge of the canvas at its own rows.
    const port = strokes().find((rect) => rect.role === "port")
    if (port === undefined) throw new Error("no port")
    expect(colorAt(decoded, last, Math.round((port.y + port.h / 2) * unit))).toBe(PALETTE.accent)
  })
})

describe("the social card", () => {
  it("slugs the root page by name rather than to a bare extension", () => {
    expect(ogSlug("")).toBe("index.png")
    expect(ogSlug("index")).toBe("index.png")
    expect(ogId("")).toBe(ogId("index"))
    expect(ogSlug("internals/platform")).toBe("internals/platform.png")
  })

  it("states the brand line on the root card and nowhere else", () => {
    expect(BRAND_LINE).toBe("microvms-agentd · EXEC · FILES · HEALTH")
    for (const id of ["", "index"]) {
      const card = ogCard({ id, title: "microvms-agentd", description: "A verified client stack." })
      expect(card.description).toContain("EXEC · FILES · HEALTH")
    }
    for (const id of ["learn/tutorial/install", "internals/platform", "glossary"]) {
      expect(ogCard({ id, title: "x", description: "y" }).description).not.toContain("EXEC")
    }
  })

  it("names the tier and the path in the running foot", () => {
    const card = ogCard({
      id: "internals/platform",
      title: "Platform",
      description: "Measured behavior."
    })
    expect(card.description).toContain("microvms-agentd · Internals")
    expect(card.description).toContain("/internals/platform/")
    expect(ogCard({ id: "learn", title: "Learn" }).description).toContain("microvms-agentd · Learn")
    expect(ogCard({ id: "reference/commands/exec", title: "exec" }).description).toContain(
      "microvms-agentd · Reference"
    )
    // A page outside the three tiers carries the site name alone.
    const glossary = ogCard({ id: "glossary", title: "Glossary" }).description ?? ""
    expect(glossary).toContain("microvms-agentd\n/glossary/")
    expect(glossary).not.toContain("·")
  })

  it("keeps a card free of a gradient, a border radius and a background image", () => {
    const card = ogCard({ id: "learn", title: "Learn", description: "Tutorials." })
    // A single stop is a flat fill; two or more would be the gradient this direction refuses.
    expect(card.bgGradient).toHaveLength(1)
    expect(card.bgImage).toBeUndefined()
    expect(card.border?.side).toBe("block-end")
    expect(card.border?.color).toEqual([0x0a, 0x54, 0x50])
  })

  it("trims an over-long title and description at a word boundary", () => {
    const long = "the exec envelope and every ordering constraint it imposes on a stream of frames "
    const card = ogCard({ id: "internals/x", title: long.repeat(2), description: long.repeat(4) })
    expect(card.title.length).toBeLessThanOrEqual(TITLE_BUDGET + 1)
    expect(card.title.endsWith("…")).toBe(true)
    expect(card.title).not.toMatch(/ …$/)
    const abstract = card.description?.split("\n\n")[0] ?? ""
    expect(abstract.length).toBeLessThanOrEqual(DESCRIPTION_BUDGET + 1)
    expect(abstract.endsWith("…")).toBe(true)
  })

  /*
   * The renderer clamps the text block's top and clips its bottom (read from
   * astro-og-canvas/dist/generateOpenGraphImage.js), so the budgets and the type sizes have to agree
   * with the band it leaves. Two of sixty-five cards clipped their running foot before this held.
   */
  it("fits the worst-case text block inside the renderer's band", () => {
    expect(OG_TEXT_BAND).toBe(OG_HEIGHT - 3 * OG_PADDING - ICON_PNG_SIZE)
    expect(worstCaseTextHeight()).toBeLessThanOrEqual(OG_TEXT_BAND)
    const card = ogCard({ id: "internals/x", title: "x", description: "y" })
    expect(card.padding).toBe(OG_PADDING)
  })

  it("leaves a title and description inside budget untouched", () => {
    const card = ogCard({ id: "glossary", title: "Glossary", description: "The vocabulary." })
    expect(card.title).toBe("Glossary")
    expect(card.description).toContain("The vocabulary.")
    expect(card.description).not.toContain("…")
  })

  it("describes the card rather than repeating the site name in the alt text", () => {
    expect(ogAlt("Exit codes")).toContain("Exit codes")
    expect(ogAlt("Exit codes")).toMatch(/paper|rule/)
    expect(ogAlt("Exit codes")).not.toContain("microvms-agentd")
  })
})
