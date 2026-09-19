// Unit tests for `src/lib/semver.ts`. Plain `node --test`, no dependency:
// Node strips the types itself (22.18+, or 22.6+ with
// `--experimental-strip-types`).
import { test } from "node:test";
import assert from "node:assert/strict";
import { semverCompare, semverIsNewer } from "../src/lib/semver.ts";

test("pre-releases order between their neighbours", () => {
  const chain = ["1.1.7", "1.2.0-1", "1.2.0-2", "1.2.0"];
  for (let i = 0; i < chain.length; i++) {
    for (let j = i + 1; j < chain.length; j++) {
      assert.equal(semverIsNewer(chain[j], chain[i]), true, `${chain[j]} > ${chain[i]}`);
      assert.equal(semverIsNewer(chain[i], chain[j]), false, `${chain[i]} < ${chain[j]}`);
    }
    assert.equal(semverIsNewer(chain[i], chain[i]), false);
  }
});

test("components compare as numbers", () => {
  assert.equal(semverIsNewer("1.10.0", "1.9.9"), true);
  assert.equal(semverIsNewer("1.2.0-10", "1.2.0-9"), true);
  assert.equal(semverIsNewer("2.0.0", "1.99.99"), true);
});

test("the SemVer spec's own ordering", () => {
  const spec = [
    "1.0.0-alpha",
    "1.0.0-alpha.1",
    "1.0.0-alpha.beta",
    "1.0.0-beta",
    "1.0.0-beta.2",
    "1.0.0-beta.11",
    "1.0.0-rc.1",
    "1.0.0",
  ];
  for (let i = 1; i < spec.length; i++) {
    assert.equal(semverCompare(spec[i], spec[i - 1]), 1, `${spec[i]} > ${spec[i - 1]}`);
  }
});

test("a leading v and build metadata", () => {
  assert.equal(semverIsNewer("v1.2.0", "1.2.0-2"), true);
  assert.equal(semverIsNewer(" v1.2.0-2 ", "v1.2.0-1"), true);
  assert.equal(semverCompare("1.2.0+build.5", "1.2.0"), 0);
});

test("unparseable is never newer", () => {
  assert.equal(semverIsNewer("garbage", "1.2.0"), false);
  assert.equal(semverIsNewer("1.3.0", "nightly"), false);
  assert.equal(semverIsNewer("1.3", "1.2.0"), false);
  assert.equal(semverIsNewer("01.3.0", "1.2.0"), false);
  assert.equal(semverCompare("", "1.2.0"), null);
});
