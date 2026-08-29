import assert from "node:assert/strict";
import test from "node:test";
import {
  reconciliationUpdates,
  type ReconciliationPosition,
  type ReconciliationRoom,
} from "./layout-reconciliation.ts";

interface Room extends ReconciliationRoom {
  key: number;
}

function at(x: number, y = 0, level = 0): ReconciliationPosition {
  return { x, y, level };
}

function apply(
  rooms: Map<string, Room>,
  positions: ReadonlyMap<string, ReconciliationPosition>,
  ids?: Iterable<string>,
): string[] {
  const updates = reconciliationUpdates(rooms, positions, (room) => room.key, ids);
  for (const update of updates) {
    const room = rooms.get(update.id);
    assert.ok(room);
    room.position = { ...update.position };
  }
  return updates.map((update) => update.id);
}

test("a tied final plan restores rooms moved only by a progressive candidate", () => {
  const rooms = new Map<string, Room>([
    ["current", { key: 1, position: at(0) }],
    ["outside-snapshot", { key: 2, position: at(2) }],
  ]);

  const checkpointIds = apply(rooms, new Map([
    ["current", at(5)],
    ["outside-snapshot", at(7)],
  ]));
  assert.deepEqual(checkpointIds, ["current", "outside-snapshot"]);

  // `movedExisting` on this final plan would be empty because both final
  // positions equal the original request. Reconciliation still compares the
  // full plan with the now-mutated live mirror and explicitly restores both.
  const finalReconciliationIds = new Set<string>(); // final movedExisting
  for (const id of checkpointIds) finalReconciliationIds.add(id);
  assert.deepEqual(apply(rooms, new Map([
    ["current", at(0)],
    ["outside-snapshot", at(2)],
  ]), finalReconciliationIds), ["current", "outside-snapshot"]);
  assert.deepEqual(rooms.get("current")?.position, at(0));
  assert.deepEqual(rooms.get("outside-snapshot")?.position, at(2));
});

test("aborting after a checkpoint preserves its durable improvement", () => {
  const rooms = new Map<string, Room>([["room", { key: 7, position: at(0) }]]);
  const committedIds = apply(rooms, new Map([["room", at(4)]]));
  assert.deepEqual(committedIds, ["room"]);
  const markerPosition = committedIds.includes("room")
    ? rooms.get("room")?.position
    : undefined;

  // An aborted run never enters final reconciliation. The next snapshot sees
  // the checkpoint as its live starting position instead of rolling it back;
  // the committed id also gives the mapper the same coordinate for its marker.
  assert.deepEqual(rooms.get("room")?.position, at(4));
  assert.deepEqual(markerPosition, at(4));
});

test("reconciliation reports ids so route and current-marker refresh use the same writes", () => {
  const rooms = new Map<string, Room>([
    ["current", { key: 11, position: at(3) }],
    ["unchanged", { key: 12, position: at(8) }],
  ]);
  const updates = reconciliationUpdates(rooms, new Map([
    ["current", at(0)],
    ["unchanged", at(8)],
  ]), (room) => room.key);

  assert.deepEqual(updates, [{ id: "current", key: 11, position: at(0) }]);
});

test("a timeout-retained Worker result does not roll back progressive coordinates", () => {
  const rooms = new Map<string, Room>([
    ["a", { key: 1, position: at(0) }],
    ["b", { key: 2, position: at(1) }],
  ]);
  const progressivePositions = new Map([
    ["a", at(10)],
    ["b", at(11)],
  ]);
  assert.deepEqual(apply(rooms, progressivePositions), ["a", "b"]);

  // A retained result whose positions match the applied checkpoints diffs to
  // nothing: the durable improvements stand.
  assert.deepEqual(
    reconciliationUpdates(rooms, progressivePositions, (room) => room.key),
    [],
  );
  assert.deepEqual(rooms.get("a")?.position, at(10));
  assert.deepEqual(rooms.get("b")?.position, at(11));
});
