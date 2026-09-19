/** SemVer 2.0.0 precedence, for the update checks.
 *
 *  Kept free of imports so `scripts/semver.test.mjs` can run it under plain
 *  Node. It must agree with `hoard_agent::update::is_newer`: the window, the
 *  service and the CLI deciding differently whether `1.2.0` is newer than
 *  `1.2.0-1` is how an app ends up nagging about an update the service
 *  says is not there. */

type Parsed = { core: [string, string, string]; pre: string[] };

// The grammar from semver.org, plus the tag's optional leading `v`.
const NUM = "0|[1-9]\\d*";
const PRE_ID = "0|[1-9]\\d*|\\d*[a-zA-Z-][0-9a-zA-Z-]*";
const SEMVER = new RegExp(
  `^v?(${NUM})\\.(${NUM})\\.(${NUM})` +
    `(?:-((?:${PRE_ID})(?:\\.(?:${PRE_ID}))*))?` +
    `(?:\\+[0-9a-zA-Z-]+(?:\\.[0-9a-zA-Z-]+)*)?$`,
);

function parse(v: string): Parsed | null {
  const m = SEMVER.exec(v.trim());
  if (!m) return null;
  return { core: [m[1], m[2], m[3]], pre: m[4] ? m[4].split(".") : [] };
}

/** Numeric identifiers without leading zeros: longer is larger, and equal
 *  lengths compare as text. Exact for any size, unlike `Number`. */
function cmpNum(a: string, b: string): number {
  if (a.length !== b.length) return a.length < b.length ? -1 : 1;
  return a < b ? -1 : a > b ? 1 : 0;
}

function cmpPreId(a: string, b: string): number {
  const an = /^\d+$/.test(a);
  const bn = /^\d+$/.test(b);
  if (an && bn) return cmpNum(a, b);
  // Numeric identifiers have lower precedence than alphanumeric ones.
  if (an) return -1;
  if (bn) return 1;
  return a < b ? -1 : a > b ? 1 : 0;
}

/** `-1`, `0` or `1` by SemVer precedence; `null` when either side does not
 *  parse. Build metadata is ignored. */
export function semverCompare(a: string, b: string): number | null {
  const x = parse(a);
  const y = parse(b);
  if (!x || !y) return null;
  for (let i = 0; i < 3; i++) {
    const c = cmpNum(x.core[i], y.core[i]);
    if (c !== 0) return c;
  }
  // A version with a pre-release sits below the same version without one.
  if (x.pre.length === 0 || y.pre.length === 0) {
    return x.pre.length === y.pre.length ? 0 : x.pre.length === 0 ? 1 : -1;
  }
  const n = Math.min(x.pre.length, y.pre.length);
  for (let i = 0; i < n; i++) {
    const c = cmpPreId(x.pre[i], y.pre[i]);
    if (c !== 0) return c;
  }
  return x.pre.length === y.pre.length ? 0 : x.pre.length < y.pre.length ? -1 : 1;
}

/** `a > b` by SemVer precedence (`1.1.7 < 1.2.0-1 < 1.2.0-2 < 1.2.0`),
 *  tolerating the tag's `v`. Unparseable → `false`: never nag about
 *  something we cannot compare. */
export function semverIsNewer(a: string, b: string): boolean {
  return semverCompare(a, b) === 1;
}
