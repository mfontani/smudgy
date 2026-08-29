import assert from "node:assert/strict";
import test from "node:test";
import {
  coordinateWriteAllowed,
  reconcilableResidentIds,
} from "./coordinate-write-policy.ts";
import { reconciliationUpdates } from "./layout-reconciliation.ts";

test("a no-move pass cannot write resident coordinates; new placements land", () => {
  // A plan that (incorrectly) moves a resident under moveExisting=false: the
  // claimed move is discarded on both write paths — the final reconciliation
  // diff and the per-room coordinate write — while a newly created room's
  // placement still writes.
  const residents = new Map([
    ["room:1", { position: { x: 0, y: 0, level: 0 } }],
  ]);
  const positions = new Map([
    ["room:1", { x: 5, y: 0, level: 0 }],
    ["vnum:200", { x: 1, y: 0, level: 0 }],
  ]);

  const ids = reconcilableResidentIds(false, ["room:1"], []);
  assert.equal(ids.size, 0);
  assert.deepEqual(
    reconciliationUpdates(residents, positions, (room) => room, ids),
    [],
  );
  assert.equal(coordinateWriteAllowed(false, false, false), false);
  assert.equal(coordinateWriteAllowed(true, false, false), true);
});

test("a move-permitted pass reconciles claimed and checkpoint-applied residents", () => {
  const residents = new Map([
    ["room:1", { position: { x: 0, y: 0, level: 0 } }],
    ["room:2", { position: { x: 3, y: 0, level: 0 } }],
  ]);
  const positions = new Map([
    ["room:1", { x: 5, y: 0, level: 0 }],
    ["room:2", { x: 3, y: 0, level: 0 }],
  ]);

  const ids = reconcilableResidentIds(true, ["room:1"], ["room:2"]);
  assert.deepEqual([...ids].sort(), ["room:1", "room:2"]);
  const updates = reconciliationUpdates(residents, positions, (room) => room, ids);
  assert.deepEqual(updates.map((update) => update.id), ["room:1"]);
  assert.equal(coordinateWriteAllowed(false, false, true), true);
  assert.equal(coordinateWriteAllowed(false, true, true), false);
});

test("durably applied checkpoint coordinates always reconcile", () => {
  // Progressive writes exist only when moves were allowed, but a coordinate
  // already written durably reconciles regardless of the flag.
  assert.deepEqual([...reconcilableResidentIds(false, [], ["room:9"])], ["room:9"]);
});
