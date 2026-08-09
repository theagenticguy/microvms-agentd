// SPDX-License-Identifier: Apache-2.0
//
// Exact decimal arithmetic over the **strings** the cost surface answers with.
//
// # Why this file exists rather than `Number()`
//
// The whole of COST-2 at this boundary is that money never becomes a JS `number`: the rates are
// figures like `0.0000276944` and the core holds them as exact decimals, so a double cannot
// round-trip them. A test that checked the arithmetic by parsing to `Number` would therefore be
// testing something weaker than the property under test — and worse, it would be the exact
// laundering step the binding refuses to provide.
//
// So the assertions are done in `BigInt`, on the digit strings, with no precision lost anywhere.
// A quantity of 28 significant digits (division by 730 hours produces them) compares and multiplies
// exactly here.
//
// No dependency: this is thirty lines of scaled-integer arithmetic, and adding a decimal library to
// a test harness for it would be a dependency in the one place a reader is least expecting one.

/** A decimal string as a scaled `BigInt`: `{ value, scale }` means `value / 10**scale`. */
export function decimal(text) {
  const negative = text.startsWith('-');
  const body = negative ? text.slice(1) : text;
  const [whole, fraction = ''] = body.split('.');
  const digits = (whole === '' ? '0' : whole) + fraction;
  return {
    value: BigInt(digits) * (negative ? -1n : 1n),
    scale: fraction.length,
  };
}

/** Both operands at a common scale, so they can be compared or added as integers. */
function aligned(left, right) {
  const scale = Math.max(left.scale, right.scale);
  return [
    left.value * 10n ** BigInt(scale - left.scale),
    right.value * 10n ** BigInt(scale - right.scale),
    scale,
  ];
}

/** Exact equality of two decimal strings, trailing zeroes and all. */
export function decimalsEqual(left, right) {
  const [a, b] = aligned(decimal(left), decimal(right));
  return a === b;
}

/** `-1`, `0`, or `1`, comparing two decimal strings exactly. */
export function compareDecimals(left, right) {
  const [a, b] = aligned(decimal(left), decimal(right));
  if (a < b) return -1;
  return a > b ? 1 : 0;
}

/** The exact product of two decimal strings, as a decimal string. */
export function multiplyDecimals(left, right) {
  const a = decimal(left);
  const b = decimal(right);
  return render({ value: a.value * b.value, scale: a.scale + b.scale });
}

/** Whether two decimal strings agree to within `rust_decimal`'s own precision.
 *
 * # Why an exact comparison is the wrong assertion for a *product*
 *
 * Measured. The core holds money in `rust_decimal`, which carries **28 significant digits** and
 * rounds there. A quantity like a GB-month figure (`0.4602739726027397260273972603` — 28 digits of
 * a division by 730) multiplied by a rate produces an exact product with ~38 digits, and the core's
 * answer is that rounded to 28. So an exact `decimalsEqual` on a re-derived product fails on
 * digits past the 28th:
 *
 *   exact  0.03733332960000000000000000000211111090
 *   core   0.0373333296000000000000000000
 *
 * Comparing exactly there would be a test about `rust_decimal`'s rounding rather than about the
 * rate multiplication, and the tempting "fix" — rounding the expectation to two decimal places —
 * would hide a genuine 1% error. So the tolerance is set just inside the representable precision:
 * anything the core could legitimately round away passes, and any real arithmetic mistake (a wrong
 * rate, a peak-instead-of-baseline quantity, a factor of 730) is orders of magnitude larger.
 *
 * `decimalsEqual` stays the right assertion for a **sum**, where no rounding happens, and for any
 * figure compared against another figure the core produced.
 */
export function decimalsCloseEnough(left, right) {
  const difference = sumDecimals([left, negate(right)]);
  const magnitude = compareDecimals(difference, '0') < 0 ? negate(difference) : difference;
  // 1e-27: inside 28 significant digits for any figure of order 1 or below, which every rate and
  // every per-line dollar amount on this surface is.
  return compareDecimals(magnitude, '0.000000000000000000000000001') <= 0;
}

/** A decimal string with its sign flipped. */
function negate(text) {
  if (text.startsWith('-')) return text.slice(1);
  return `-${text}`;
}

/** The exact sum of decimal strings, as a decimal string. */
export function sumDecimals(values) {
  let total = { value: 0n, scale: 0 };
  for (const text of values) {
    const [a, b, scale] = aligned(total, decimal(text));
    total = { value: a + b, scale };
  }
  return render(total);
}

/** A scaled `BigInt` back to its decimal string. */
function render({ value, scale }) {
  const sign = value < 0n ? '-' : '';
  const digits = (value < 0n ? -value : value).toString().padStart(scale + 1, '0');
  if (scale === 0) return sign + digits;
  return `${sign}${digits.slice(0, -scale)}.${digits.slice(-scale)}`;
}
