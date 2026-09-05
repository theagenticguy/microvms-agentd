// SPDX-License-Identifier: Apache-2.0
import { deflateSync } from "node:zlib"

/**
 * The mark, and every raster derived from it.
 *
 * The mark is a MicroVM reduced to three shapes: an ink frame for the VM, a filled ink square inside it
 * for the daemon that runs there, and one accent tick where the frame opens for the endpoint the client
 * calls. It is the whole system at the smallest size that object still reads.
 *
 * Three properties make it survive 16px, and each is a constraint rather than a preference:
 *
 * - The geometry is whole units on a 16-unit grid, so a 16px icon lands on whole pixels and no edge is
 *   antialiased into gray. `GRID` is the unit count, and every raster size is a scale of it.
 * - The ground is opaque paper rather than transparent. Ink at 16.83:1 against paper is invisible
 *   against a dark browser tab strip, so the mark carries the page it is drawn on.
 * - Nothing is thinner than two units. A one-unit stroke is a 1px hairline at favicon size, which
 *   disappears on a fractional-scale display.
 *
 * SVG, PNG and ICO are all derived from `MARK`, so the vector and the rasters cannot disagree about
 * what the mark is. `tests/branding.test.ts` decodes the committed artifacts independently and compares
 * pixels, not bytes: a zlib that packs the same image differently is not a drift.
 *
 * Everything under `branding/` is imported with its real `.ts` extension, where the rest of this
 * package writes `.js`. `write-icons.ts` is run by node directly, which resolves a specifier literally
 * and has no `.js`-to-`.ts` fallback. `allowImportingTsExtensions` is on in Astro's base tsconfig, so
 * this is also what typechecks. Ported from memhtml-public's apps/docs/src/branding/mark.ts.
 */

/** The unit count of the mark's coordinate system. Sixteen, so a 16px raster is 1 unit per pixel. */
export const GRID = 16

/**
 * The three palette values the mark uses, each carrying its measured ratio against the ground. Every
 * value here is declared in `src/styles/rfc.css` under the `--docs-` prefix, and the test reads that
 * file back to check the two agree.
 */
export const PALETTE = {
  /** Warm paper: the ground a printed specification is read on. */
  paper: "#FCFBF7",
  /** 16.83:1 on paper. */
  ink: "#1A1A18",
  /** Deep teal, 8.46:1 on paper. This project's own accent, not memhtml's standards red. */
  accent: "#0A5450"
} as const

/** One axis-aligned rectangle in grid units. */
export interface MarkRect {
  readonly x: number
  readonly y: number
  readonly w: number
  readonly h: number
  readonly fill: string
  /** What this stroke is, so a later edit knows what it would be deleting. */
  readonly role: string
}

/**
 * The mark, in drawing order.
 *
 * The frame is drawn as five strokes rather than four because its right side is OPEN at the port: the
 * two ink strokes above and below the gap and the accent tick that fills it share exact edges in unit
 * space, so no stroke is painted over another and every raster size places the shared edge on the same
 * pixel. The tick runs to `x = GRID` on purpose: a tick that stopped short would leave a paper margin
 * at 16px that reads as a rendering fault, and an endpoint is the one part of the system that reaches
 * outside the VM.
 */
export const MARK: ReadonlyArray<MarkRect> = [
  { x: 0, y: 0, w: GRID, h: GRID, fill: PALETTE.paper, role: "ground" },
  { x: 2, y: 2, w: 12, h: 2, fill: PALETTE.ink, role: "frame top" },
  { x: 2, y: 12, w: 12, h: 2, fill: PALETTE.ink, role: "frame bottom" },
  { x: 2, y: 4, w: 2, h: 8, fill: PALETTE.ink, role: "frame left" },
  { x: 12, y: 4, w: 2, h: 3, fill: PALETTE.ink, role: "frame right, above the port" },
  { x: 12, y: 9, w: 2, h: 3, fill: PALETTE.ink, role: "frame right, below the port" },
  { x: 6, y: 6, w: 4, h: 4, fill: PALETTE.ink, role: "daemon" },
  { x: 12, y: 7, w: 4, h: 2, fill: PALETTE.accent, role: "port" }
]

/** The accessible name every rendering of the mark carries. */
export const MARK_LABEL = "microvms-agentd"

/**
 * The mark as SVG.
 *
 * `shape-rendering="crispEdges"` is what keeps a browser scaling the vector to 17px from softening the
 * strokes into gray; the geometry is integral, so there is nothing to interpolate.
 */
export const markSvg = (): string =>
  [
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${GRID} ${GRID}" width="${GRID}" height="${GRID}" shape-rendering="crispEdges" role="img" aria-label="${MARK_LABEL}">`,
    `  <title>${MARK_LABEL}</title>`,
    ...MARK.map(
      (rect) =>
        `  <rect x="${rect.x}" y="${rect.y}" width="${rect.w}" height="${rect.h}" fill="${rect.fill}"/>`
    ),
    "</svg>",
    ""
  ].join("\n")

const channel = (hex: string, at: number): number =>
  Number.parseInt(hex.slice(1 + at * 2, 3 + at * 2), 16)

/**
 * The mark rasterised to RGBA8 at `size` pixels square.
 *
 * Edges are placed with `Math.round` rather than floor/ceil so a size that is not a multiple of `GRID`
 * (180, the Apple touch size) distributes its remainder instead of accumulating it on one side. Two
 * strokes that share an edge in unit space round that edge to the same pixel, so the open frame stays
 * closed around its port at every size this ships.
 */
export const markRaster = (size: number): Uint8Array => {
  const scale = size / GRID
  const pixels = new Uint8Array(size * size * 4)
  for (const rect of MARK) {
    const [red, green, blue] = [channel(rect.fill, 0), channel(rect.fill, 1), channel(rect.fill, 2)]
    const x0 = Math.round(rect.x * scale)
    const x1 = Math.round((rect.x + rect.w) * scale)
    const y0 = Math.round(rect.y * scale)
    const y1 = Math.round((rect.y + rect.h) * scale)
    for (let y = y0; y < y1; y += 1) {
      for (let x = x0; x < x1; x += 1) {
        const at = (y * size + x) * 4
        pixels[at] = red
        pixels[at + 1] = green
        pixels[at + 2] = blue
        pixels[at + 3] = 255
      }
    }
  }
  return pixels
}

const CRC_TABLE: Uint32Array = (() => {
  const table = new Uint32Array(256)
  for (let n = 0; n < 256; n += 1) {
    let c = n
    for (let bit = 0; bit < 8; bit += 1) c = (c & 1) === 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
    table[n] = c >>> 0
  }
  return table
})()

const crc32 = (bytes: Uint8Array): number => {
  let crc = 0xffffffff
  for (const byte of bytes) crc = (CRC_TABLE[(crc ^ byte) & 0xff] ?? 0) ^ (crc >>> 8)
  return (crc ^ 0xffffffff) >>> 0
}

const PNG_SIGNATURE = Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)

const concat = (parts: ReadonlyArray<Uint8Array>): Uint8Array => {
  const out = new Uint8Array(parts.reduce((total, part) => total + part.length, 0))
  let at = 0
  for (const part of parts) {
    out.set(part, at)
    at += part.length
  }
  return out
}

/** One PNG chunk: length, type, payload, CRC over type and payload. */
const pngChunk = (type: string, data: Uint8Array): Uint8Array => {
  const out = new Uint8Array(data.length + 12)
  const view = new DataView(out.buffer)
  view.setUint32(0, data.length)
  for (let at = 0; at < 4; at += 1) out[4 + at] = type.charCodeAt(at)
  out.set(data, 8)
  view.setUint32(data.length + 8, crc32(out.subarray(4, data.length + 8)))
  return out
}

/**
 * The mark as a PNG, encoded here rather than by an image library.
 *
 * The site optimises no images and runs Astro's passthrough image service, so there is no rasteriser
 * to borrow. The mark is eight axis-aligned rectangles, which is well inside what `node:zlib` plus a
 * CRC table can encode: one IHDR, one IDAT of unfiltered RGBA8 scanlines, one IEND.
 */
export const markPng = (size: number): Uint8Array => {
  const pixels = markRaster(size)
  const stride = size * 4
  // Filter type 0 (None) prefixes each scanline. A predictor buys nothing on flat color fields.
  const scanlines = new Uint8Array((stride + 1) * size)
  for (let y = 0; y < size; y += 1) {
    scanlines[y * (stride + 1)] = 0
    scanlines.set(pixels.subarray(y * stride, (y + 1) * stride), y * (stride + 1) + 1)
  }

  const header = new Uint8Array(13)
  const headerView = new DataView(header.buffer)
  headerView.setUint32(0, size)
  headerView.setUint32(4, size)
  header[8] = 8 // bit depth
  header[9] = 6 // color type: truecolor with alpha
  header[10] = 0 // deflate
  header[11] = 0 // adaptive filtering
  header[12] = 0 // no interlace

  return concat([
    PNG_SIGNATURE,
    pngChunk("IHDR", header),
    pngChunk("IDAT", new Uint8Array(deflateSync(scanlines, { level: 9 }))),
    pngChunk("IEND", new Uint8Array(0))
  ])
}

/** The sizes the `.ico` carries: the tab strip, the bookmark bar, and the Windows desktop. */
export const ICO_SIZES: ReadonlyArray<number> = [16, 32, 48]

/**
 * The mark as an `.ico` holding PNG-encoded entries.
 *
 * An ICO directory entry stores its side length in ONE byte, so 256 is written as 0: irrelevant at
 * these sizes and stated because it is the trap in this format. PNG payloads inside ICO have been read
 * by every shipping browser since IE11; the alternative, an uncompressed DIB with a separate AND mask,
 * buys nothing here.
 */
export const markIco = (sizes: ReadonlyArray<number> = ICO_SIZES): Uint8Array => {
  const images = sizes.map((size) => markPng(size))
  const directory = new Uint8Array(6 + sizes.length * 16)
  const view = new DataView(directory.buffer)
  view.setUint16(0, 0, true) // reserved
  view.setUint16(2, 1, true) // type 1: icon
  view.setUint16(4, sizes.length, true)

  let offset = directory.length
  sizes.forEach((size, at) => {
    const entry = 6 + at * 16
    directory[entry] = size % 256
    directory[entry + 1] = size % 256
    directory[entry + 2] = 0 // palette size: not paletted
    directory[entry + 3] = 0 // reserved
    view.setUint16(entry + 4, 1, true) // color planes
    view.setUint16(entry + 6, 32, true) // bits per pixel
    view.setUint32(entry + 8, images[at]?.length ?? 0, true)
    view.setUint32(entry + 12, offset, true)
    offset += images[at]?.length ?? 0
  })

  return concat([directory, ...images])
}

/** The Apple touch icon's side length. iOS scales anything else and softens the strokes doing it. */
export const APPLE_TOUCH_SIZE = 180

/**
 * The PNG icon's side length, and the mark at the head of every social card.
 *
 * It ships at exactly the size it is drawn at because the card renderer resamples with a linear
 * filter: scaling the 180px icon down to 48 would soften every stroke the geometry went to the trouble
 * of putting on a whole pixel.
 */
export const ICON_PNG_SIZE = 48

/** Every artifact the site serves, as a path under `public/` and the bytes that belong there. */
export const markArtifacts = (): ReadonlyArray<{
  readonly file: string
  readonly bytes: Uint8Array
}> => [
  { file: "favicon.svg", bytes: new TextEncoder().encode(markSvg()) },
  { file: "favicon.ico", bytes: markIco() },
  { file: `icon-${ICON_PNG_SIZE}.png`, bytes: markPng(ICON_PNG_SIZE) },
  { file: "apple-touch-icon.png", bytes: markPng(APPLE_TOUCH_SIZE) }
]
