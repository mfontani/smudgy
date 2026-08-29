/**
 * Golden differential harness. Every corpus scenario's exact public outputs —
 * final positions, the full quality tuple, and the deterministic work
 * counters — are pinned in `layout-golden.json`. A behavior-preserving
 * optimization pass must leave every scenario bit-for-bit identical.
 *
 * Regenerate the goldens deliberately with `node layout-golden-update.mjs`
 * from this directory; its per-scenario summary shows what changed.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { goldenScenarios } from "./layout-golden-scenarios.ts";

const GOLDEN_URL = new URL("./layout-golden.json", import.meta.url);
const REGENERATE_HINT =
  "regenerate deliberately with `node layout-golden-update.mjs` from packages/map-layout";

interface GoldenFile {
  formatVersion: number;
  scenarios: Record<string, unknown>;
}

function compareStrings(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function describeValue(value: unknown): string {
  const text = JSON.stringify(value);
  return text === undefined
    ? "undefined"
    : text.length > 120
    ? `${text.slice(0, 120)}…`
    : text;
}

/**
 * Locate the first differing field between the golden and the fresh result in
 * a stable traversal order, so a mismatch names exactly where behavior moved.
 */
function firstDifference(golden: unknown, current: unknown, path: string): string | undefined {
  if (golden === current) return undefined;
  const bothObjects = typeof golden === "object" && golden !== null &&
    typeof current === "object" && current !== null;
  if (!bothObjects) {
    return `${path}: golden ${describeValue(golden)} != current ${describeValue(current)}`;
  }
  const goldenIsArray = Array.isArray(golden);
  if (goldenIsArray !== Array.isArray(current)) {
    return `${path}: golden ${describeValue(golden)} != current ${describeValue(current)}`;
  }
  if (goldenIsArray) {
    const goldenRows = golden as unknown[];
    const currentRows = current as unknown[];
    const length = Math.max(goldenRows.length, currentRows.length);
    for (let index = 0; index < length; index += 1) {
      if (index >= goldenRows.length) {
        return `${path}[${index}]: current has extra ${describeValue(currentRows[index])}`;
      }
      if (index >= currentRows.length) {
        return `${path}[${index}]: current lost ${describeValue(goldenRows[index])}`;
      }
      const difference = firstDifference(goldenRows[index], currentRows[index], `${path}[${index}]`);
      if (difference) return difference;
    }
    return undefined;
  }
  const goldenRecord = golden as Record<string, unknown>;
  const currentRecord = current as Record<string, unknown>;
  const keys = [...new Set([...Object.keys(goldenRecord), ...Object.keys(currentRecord)])]
    .sort(compareStrings);
  for (const key of keys) {
    const keyPath = `${path}.${key}`;
    if (!(key in goldenRecord)) {
      return `${keyPath}: current has extra ${describeValue(currentRecord[key])}`;
    }
    if (!(key in currentRecord)) {
      return `${keyPath}: current lost ${describeValue(goldenRecord[key])}`;
    }
    const difference = firstDifference(goldenRecord[key], currentRecord[key], keyPath);
    if (difference) return difference;
  }
  return undefined;
}

const golden: GoldenFile = JSON.parse(readFileSync(GOLDEN_URL, "utf8"));
const scenarios = goldenScenarios();

test("golden file version and scenario names match the corpus exactly", () => {
  assert.equal(golden.formatVersion, 1, REGENERATE_HINT);
  assert.deepEqual(
    Object.keys(golden.scenarios).sort(compareStrings),
    scenarios.map((scenario) => scenario.name).sort(compareStrings),
    `golden scenario set drifted from the corpus; ${REGENERATE_HINT}`,
  );
});

for (const scenario of scenarios) {
  test(`golden: ${scenario.name}`, () => {
    const expected = golden.scenarios[scenario.name];
    assert.ok(
      expected !== undefined,
      `scenario ${scenario.name} has no golden; ${REGENERATE_HINT}`,
    );
    // The JSON round trip gives both sides identical undefined-stripping
    // semantics before comparison.
    const actual: unknown = JSON.parse(JSON.stringify(scenario.run()));
    const difference = firstDifference(expected, actual, "$");
    if (difference !== undefined) {
      assert.fail(`scenario ${scenario.name}: first difference at ${difference}`);
    }
    assert.deepEqual(actual, expected);
  });
}
