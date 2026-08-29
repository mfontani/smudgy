import assert from "node:assert/strict";
import test from "node:test";
import {
  CurrentLocationFreshness,
  type CurrentLocationObservation,
} from "./current-location-freshness.ts";
import { SnapshotLatencyLanes } from "./latency-lanes.ts";

interface Snapshot {
  center: number;
}

function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

async function settle(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

function requireObservation(
  observation: CurrentLocationObservation | undefined,
): CurrentLocationObservation {
  assert.ok(observation);
  return observation;
}

test("invalid observations do not supersede the latest usable room", () => {
  const freshness = new CurrentLocationFreshness();
  const current = requireObservation(freshness.observe(100));

  assert.equal(freshness.observe(-1), undefined);
  assert.equal(freshness.observe(1.5), undefined);
  assert.equal(freshness.isCurrent(current), true);
});

test("an observed cache miss still suppresses an older publisher", () => {
  const freshness = new CurrentLocationFreshness();
  const old = requireObservation(freshness.observe(100));
  const unmappedCurrent = requireObservation(freshness.observe(200));

  assert.equal(freshness.isCurrent(old), false);
  assert.equal(freshness.isCurrent(unmappedCurrent), true);
});

test("same-vnum Room.Info preserves a cache-miss Map.Local topology ticket", async () => {
  const freshness = new CurrentLocationFreshness();
  const observations = new WeakMap<Snapshot, CurrentLocationObservation>();
  const topologyBlocked = deferred();
  const published: number[] = [];
  const lanes = new SnapshotLatencyLanes<Snapshot>({
    snapshotKey: (snapshot) => snapshot.center,
    followCurrent: (snapshot) => {
      observations.set(
        snapshot,
        requireObservation(freshness.observe(snapshot.center)),
      );
    },
    runTopology: async (snapshot) => {
      await topologyBlocked.promise;
      const observation = observations.get(snapshot);
      if (observation && freshness.isCurrent(observation)) {
        published.push(observation.vnum);
      }
    },
    runFullReflow: async () => {},
  });

  const snapshot = { center: 100 };
  lanes.start();
  lanes.enqueue(snapshot);
  const mapLocalObservation = observations.get(snapshot);
  assert.ok(mapLocalObservation);

  const roomInfoObservation = requireObservation(freshness.observe(100));
  assert.equal(roomInfoObservation, mapLocalObservation);
  topologyBlocked.resolve();
  await settle();

  assert.deepEqual(published, [100]);
  lanes.stop();
});

test("returning to A after B does not revive A's old observation", () => {
  const freshness = new CurrentLocationFreshness();
  const oldA = requireObservation(freshness.observe(100));
  requireObservation(freshness.observe(200));
  const currentA = requireObservation(freshness.observe(100));

  assert.notEqual(currentA.ticket, oldA.ticket);
  assert.equal(freshness.isCurrent(oldA), false);
  assert.equal(freshness.isCurrent(currentA), true);
});

test("blocked topology cannot publish A after the fast lane follows B", async () => {
  const freshness = new CurrentLocationFreshness();
  const observations = new WeakMap<Snapshot, CurrentLocationObservation>();
  const firstTopology = deferred();
  const published: number[] = [];
  const publish = (observation: CurrentLocationObservation): void => {
    if (freshness.isCurrent(observation)) published.push(observation.vnum);
  };
  const lanes = new SnapshotLatencyLanes<Snapshot>({
    snapshotKey: (snapshot) => snapshot.center,
    followCurrent: (snapshot) => {
      const observation = requireObservation(freshness.observe(snapshot.center));
      observations.set(snapshot, observation);
      publish(observation);
    },
    runTopology: async (snapshot) => {
      if (snapshot.center === 100) await firstTopology.promise;
      publish(observations.get(snapshot) as CurrentLocationObservation);
    },
    runFullReflow: async () => {},
  });

  lanes.start();
  lanes.enqueue({ center: 100 });
  lanes.enqueue({ center: 200 });
  assert.deepEqual(published, [100, 200]);

  firstTopology.resolve();
  await settle();
  assert.deepEqual(
    published,
    [100, 200, 200],
    "old topology finishes losslessly but cannot restore its old marker",
  );
  lanes.stop();
});

test("a superseded reflow cannot recenter A after its commit returns", async () => {
  const freshness = new CurrentLocationFreshness();
  const old = requireObservation(freshness.observe(100));
  const committed = deferred();
  const published: number[] = [];
  const oldReflow = (async () => {
    await committed.promise;
    if (freshness.isCurrent(old)) published.push(old.vnum);
  })();

  const current = requireObservation(freshness.observe(200));
  published.push(current.vnum);
  committed.resolve();
  await oldReflow;

  assert.deepEqual(published, [200]);
});

test("a stale plan retargets coordinate refresh to the latest moved room", () => {
  const freshness = new CurrentLocationFreshness();
  const staleCenter = requireObservation(freshness.observe(100));
  const current = requireObservation(freshness.observe(200));
  const rooms = new Map([
    [100, { vnum: 100, area: "same", id: "room:1" }],
    [200, { vnum: 200, area: "same", id: "room:2" }],
  ]);
  const planArea = "same";
  const movedResidentIds = new Set(["room:1", "room:2"]);
  const published: number[] = [];

  if (freshness.isCurrent(staleCenter)) published.push(staleCenter.vnum);
  freshness.publishIfCurrent(
    (vnum) => {
      const room = rooms.get(vnum);
      return room?.area === planArea && movedResidentIds.has(room.id)
        ? room
        : undefined;
    },
    (room, observation) => {
      assert.equal(observation, current);
      published.push(room.vnum);
    },
  );

  assert.deepEqual(published, [200]);
});

test("a coordinate commit does not recenter an unchanged current room", () => {
  const freshness = new CurrentLocationFreshness();
  requireObservation(freshness.observe(200));
  const movedResidentIds = new Set(["room:1"]);
  const published: number[] = [];

  freshness.publishIfCurrent(
    (vnum) => {
      const room = { vnum, id: "room:2" };
      return movedResidentIds.has(room.id) ? room : undefined;
    },
    (room) => published.push(room.vnum),
  );

  assert.deepEqual(published, []);
});

test("clear invalidates old work even when a later run observes the same vnum", () => {
  const freshness = new CurrentLocationFreshness();
  const oldRun = requireObservation(freshness.observe(100));
  freshness.clear();
  assert.equal(freshness.isCurrent(oldRun), false);

  const newRun = requireObservation(freshness.observe(100));
  assert.notEqual(newRun.ticket, oldRun.ticket);
  assert.equal(freshness.isCurrent(oldRun), false);
  assert.equal(freshness.isCurrent(newRun), true);
});

test("retained Room.Info re-establishes restart authority without Map.Local", () => {
  const freshness = new CurrentLocationFreshness();
  const previousRun = requireObservation(freshness.observe(100));
  freshness.clear();

  const retainedRoomInfo = requireObservation(freshness.observe(200));
  const rooms = new Map<number, { readonly vnum: number }>();
  const published: number[] = [];
  const publishAfterRefresh = (): boolean => freshness.publishIfCurrent(
    (vnum) => rooms.get(vnum),
    (room) => published.push(room.vnum),
  );

  assert.equal(publishAfterRefresh(), false, "the room is initially absent from the mirror");
  rooms.set(200, { vnum: 200 });
  assert.equal(publishAfterRefresh(), true, "hydration makes the current room publishable");
  assert.equal(freshness.isCurrent(previousRun), false);
  assert.equal(freshness.isCurrent(retainedRoomInfo), true);
  assert.deepEqual(published, [200]);
});

test("retained Room.Info wins a restart tie with stale Map.Local", () => {
  const freshness = new CurrentLocationFreshness();
  requireObservation(freshness.observe(50));
  freshness.clear();

  const retainedMapLocal = requireObservation(freshness.observe(100));
  const retainedRoomInfo = requireObservation(freshness.observe(200));
  assert.equal(freshness.isCurrent(retainedMapLocal), false);
  assert.equal(freshness.isCurrent(retainedRoomInfo), true);
});
