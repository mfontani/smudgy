/**
 * The realistic-map quality ratchet. `realistic-ratchet.json` records the
 * full public quality tuple each fixture/pipeline last achieved; this test
 * requires the live engine to be EQUAL OR BETTER, lexicographically, on every
 * scenario. A strictly better result passes — and prompts a deliberate
 * regeneration (`node realistic-ratchet-update.mjs`) so the improvement is
 * banked and can never silently erode. A worse result fails, naming the first
 * tuple field that regressed.
 *
 * Each record also carries the fixture's `aspiration` — the layout the
 * fixture exists to reach. Aspirations are data, never asserted; the gap
 * between the recorded tuple and its aspiration is the engine's open quality
 * work, visible in reports.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { compareLayoutQuality, type LayoutQuality } from "./layout.ts";
import {
  normalizedQuality,
  QUALITY_TUPLE_FIELDS,
  realisticScenarios,
} from "./realistic-fixtures.ts";

const RATCHET_URL = new URL("./realistic-ratchet.json", import.meta.url);
const REGENERATE_HINT =
  "regenerate deliberately with `node realistic-ratchet-update.mjs` from packages/map-layout";

interface RatchetRecord {
  aspiration: string;
  rooms: number;
  edges: number;
  quality: Record<string, number>;
}

interface RatchetFile {
  formatVersion: number;
  scenarios: Record<string, RatchetRecord>;
}

function compareStrings(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function digest(quality: Record<string, number>): string {
  return `[${QUALITY_TUPLE_FIELDS.map((field) => quality[field] ?? 0).join(",")}]`;
}

function firstRegressedField(
  current: Record<string, number>,
  recorded: Record<string, number>,
): string | undefined {
  for (const field of QUALITY_TUPLE_FIELDS) {
    const currentValue = current[field] ?? 0;
    const recordedValue = recorded[field] ?? 0;
    if (currentValue === recordedValue) continue;
    return currentValue > recordedValue
      ? `${field} (${recordedValue} -> ${currentValue})`
      : undefined;
  }
  return undefined;
}

const ratchet: RatchetFile = JSON.parse(readFileSync(RATCHET_URL, "utf8"));
const scenarios = realisticScenarios();

test("ratchet file version and scenario names match the corpus exactly", () => {
  assert.equal(ratchet.formatVersion, 1, REGENERATE_HINT);
  assert.deepEqual(
    Object.keys(ratchet.scenarios).sort(compareStrings),
    scenarios.map((scenario) => scenario.name).sort(compareStrings),
    `ratchet scenario set drifted from the corpus; ${REGENERATE_HINT}`,
  );
});

for (const scenario of scenarios) {
  test(`ratchet: ${scenario.name}`, () => {
    const record = ratchet.scenarios[scenario.name];
    assert.ok(record, `scenario ${scenario.name} has no ratchet record; ${REGENERATE_HINT}`);
    // Rooms and edges are the fixture's identity, not engine behavior: when
    // they move, the fixture changed and the record must be regenerated.
    assert.equal(
      scenario.rooms,
      record.rooms,
      `fixture shape changed (rooms ${record.rooms} -> ${scenario.rooms}); ${REGENERATE_HINT}`,
    );
    assert.equal(
      scenario.edges,
      record.edges,
      `fixture shape changed (edges ${record.edges} -> ${scenario.edges}); ${REGENERATE_HINT}`,
    );

    const current = normalizedQuality(scenario.run());
    const comparison = compareLayoutQuality(
      current as unknown as LayoutQuality,
      record.quality as unknown as LayoutQuality,
    );
    if (comparison < 0) {
      assert.fail(
        `${scenario.name} regressed at ${firstRegressedField(current, record.quality)}: ` +
          `recorded ${digest(record.quality)}, current ${digest(current)}`,
      );
    }
    if (comparison > 0) {
      // Strictly better passes — bank it so it can never erode.
      console.log(
        `ratchet: ${scenario.name} IMPROVED ${digest(record.quality)} -> ${digest(current)}; ` +
          REGENERATE_HINT,
      );
    }
  });
}
