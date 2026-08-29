/**
 * Version-independent invariants over the realistic-map corpus
 * (`realistic-fixtures.ts`). Every assertion here must hold for ANY engine
 * version — these are the hard-validity and structural-sanity contracts, not
 * quality goals. Quality goals live in `realistic-ratchet.json`, enforced
 * equal-or-better by `realistic-ratchet.test.ts`.
 *
 * Invariants asserted for every fixture/pipeline:
 * - hard validity: every room placed on an integral cell, collision-free;
 *   `movable: false` residents exactly where the user pinned them;
 *   `movedExisting` is exactly the set of residents whose cell changed;
 * - honest reporting: the plan's claimed quality tuple equals an independent
 *   recount over its final positions — wherever the engine claims a retained
 *   ray or a zero, the recount confirms it;
 * - determinism: rebuilding the fixture and rerunning the pipeline is
 *   bit-identical.
 *
 * Plus per-class structural sanity (stratified tower levels, disjoint
 * disconnected components, the pinned cluster never moving, and so on).
 */

import assert from "node:assert/strict";
import test from "node:test";
import {
  compareLayoutQuality,
  measureIntegralLayoutQuality,
  planIntegralLayout,
  type ConstraintRepairReport,
  type GridPosition,
  type IntegralLayoutPlan,
  type IntegralLayoutRequest,
} from "./layout.ts";
import { repairIntegralLayoutConstraints } from "./constraint-layout.ts";
import {
  denseGridArea,
  disconnectedArea,
  hubArea,
  linkReciprocity,
  lockedClusterArea,
  LOCKED_CLUSTER_REPAIR_OPTIONS,
  oneWayMazeArea,
  REALISTIC_SEEDS,
  replayTruncatedGrowth,
  towerArea,
  truncatedGrowthArea,
} from "./realistic-fixtures.ts";

/** Code-unit string ordering, matching the engine's own collation. */
function compareStrings(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function samePosition(a: GridPosition, b: GridPosition): boolean {
  return a.x === b.x && a.y === b.y && a.level === b.level;
}

/** A canonical, clock-free byte representation of a plan for bit-comparison. */
function canonicalPlan(plan: IntegralLayoutPlan): string {
  return JSON.stringify({
    positions: [...plan.positions]
      .sort(([a], [b]) => compareStrings(a, b))
      .map(([id, { x, y, level }]) => [id, x, y, level]),
    movedExisting: [...plan.movedExisting].sort(compareStrings),
    quality: { ...plan.quality },
  });
}

/** The public repair report minus its wall-clock duration fields. */
function canonicalReport(report: Readonly<ConstraintRepairReport>): string {
  const { searchMs, compactionMs, polishMs, firstIncumbentMs, crossingRepair, ...rest } = report;
  void searchMs;
  void compactionMs;
  void polishMs;
  void firstIncumbentMs;
  const { elapsedMs, ...crossingRest } = crossingRepair;
  void elapsedMs;
  return JSON.stringify({ ...rest, crossingRepair: { ...crossingRest } });
}

/**
 * The hard-validity contract every published plan must satisfy, regardless of
 * engine version.
 */
function assertHardValidity(request: IntegralLayoutRequest, plan: IntegralLayoutPlan): void {
  const cellOwner = new Map<string, string>();
  const ids = new Set([
    ...request.residents.map((resident) => resident.id),
    ...request.nodes.map((node) => node.id),
  ]);
  for (const id of ids) {
    const position = plan.positions.get(id);
    assert.ok(position, `room ${id} was not placed`);
    assert.ok(
      Number.isInteger(position.x) && Number.isInteger(position.y) &&
        Number.isInteger(position.level),
      `room ${id} sits on a non-integral cell ${JSON.stringify(position)}`,
    );
    const cell = `${position.x}|${position.y}|${position.level}`;
    const owner = cellOwner.get(cell);
    assert.equal(owner, undefined, `rooms ${owner} and ${id} collide on cell ${cell}`);
    cellOwner.set(cell, id);
  }

  const changed = new Set<string>();
  for (const resident of request.residents) {
    const position = plan.positions.get(resident.id) as GridPosition;
    if (samePosition(position, resident.position)) continue;
    changed.add(resident.id);
    assert.ok(
      resident.movable,
      `pinned room ${resident.id} moved from ${JSON.stringify(resident.position)} ` +
        `to ${JSON.stringify(position)}`,
    );
  }
  assert.deepEqual(
    [...plan.movedExisting].sort(compareStrings),
    [...changed].sort(compareStrings),
    "movedExisting must be exactly the residents whose cell changed",
  );

  // Honest reporting: the claimed tuple must survive an independent recount.
  assert.deepEqual(
    { ...plan.quality },
    measureIntegralLayoutQuality(plan.positions, request.edges),
    "the plan's claimed quality tuple must equal a recount over its positions",
  );
}

/** Rebuild the fixture and rerun the pipeline: results must be bit-identical. */
function assertDeterministic(
  build: () => IntegralLayoutRequest,
  pipeline: (request: IntegralLayoutRequest) => IntegralLayoutPlan,
): { request: IntegralLayoutRequest; plan: IntegralLayoutPlan } {
  const first = build();
  const second = build();
  assert.equal(
    JSON.stringify(second),
    JSON.stringify(first),
    "fixture constructor must be deterministic",
  );
  const firstPlan = pipeline(first);
  const secondPlan = pipeline(second);
  assert.equal(
    canonicalPlan(secondPlan),
    canonicalPlan(firstPlan),
    "two pipeline runs must be bit-identical",
  );
  return { request: first, plan: firstPlan };
}

function levelCensus(plan: IntegralLayoutPlan): Set<number> {
  const levels = new Set<number>();
  for (const position of plan.positions.values()) levels.add(position.level);
  return levels;
}

test("dense wilderness grid: hard validity, determinism, grid-scale sanity", () => {
  const { request, plan } = assertDeterministic(
    () => denseGridArea(REALISTIC_SEEDS.denseGrid),
    (built) => planIntegralLayout(built),
  );
  assert.equal(request.residents.length, 220);
  assertHardValidity(request, plan);
  // A flat wilderness never sprouts levels...
  assert.deepEqual([...levelCensus(plan)], [0]);
  // ...and stays grid-shaped: 220 rooms of near-full 4-connectivity fit a
  // 15x15 chart (area 225). Triple that bounding-box area is gross scatter,
  // not layout — a structural backstop, far above any real engine's result.
  assert.ok(
    plan.quality.footprintArea <= 3 * 225,
    `wilderness grid scattered to footprint ${plan.quality.footprintArea}`,
  );
});

test("one-way maze: hard validity, determinism, and its one-way mix", () => {
  const { request, plan } = assertDeterministic(
    () => oneWayMazeArea(REALISTIC_SEEDS.oneWayMaze),
    (built) => planIntegralLayout(built),
  );
  assert.equal(request.residents.length, 120);
  // Fixture self-check: the maze exists to exercise reciprocal-preference
  // weighting, which needs a genuinely one-way-heavy area.
  const { links, oneWay } = linkReciprocity(request.edges);
  assert.ok(
    oneWay / links >= 0.4,
    `maze one-way share ${oneWay}/${links} fell below the 40% the class requires`,
  );
  assertHardValidity(request, plan);
  assert.deepEqual([...levelCensus(plan)], [0]);
});

test("tower: hard validity, determinism, and level stratification", () => {
  const { request, plan } = assertDeterministic(
    () => towerArea(REALISTIC_SEEDS.tower),
    (built) => planIntegralLayout(built),
  );
  assertHardValidity(request, plan);
  // The vertical stacks pin every level to the immovable ground-floor anchor:
  // a room leaving its charted level would break a satisfied cross-level ray.
  for (const resident of request.residents) {
    const position = plan.positions.get(resident.id) as GridPosition;
    assert.equal(
      position.level,
      resident.position.level,
      `tower room ${resident.id} left its stratum`,
    );
  }
  // Same-level projected verticals stay projected: each loft shares its
  // spine end's level.
  for (let level = 0; level < 10; level += 1) {
    for (const [loft, spine] of [["loft-w", "s0"], ["loft-e", "s2"]] as const) {
      const loftPosition = plan.positions.get(`tower-${level}-${loft}`) as GridPosition;
      const spinePosition = plan.positions.get(`tower-${level}-${spine}`) as GridPosition;
      assert.equal(
        loftPosition.level,
        spinePosition.level,
        `projected vertical tower-${level}-${loft} left its spine's level`,
      );
    }
  }
});

test("hub: hard validity, determinism, and spokes on their proper sides", () => {
  const { request, plan } = assertDeterministic(
    () => hubArea(REALISTIC_SEEDS.hub),
    (built) => planIntegralLayout(built),
  );
  assertHardValidity(request, plan);
  assert.deepEqual([...levelCensus(plan)], [0]);
  const plaza = plan.positions.get("plaza") as GridPosition;
  assert.deepEqual(plaza, { x: 0, y: 0, level: 0 }, "the pinned plaza moved");
  // Structural sanity, weaker than on-ray on purpose: each reciprocal
  // cardinal spoke's first room must at least sit strictly on the plaza's
  // correct side along the spoke axis. (The ratchet holds the stronger exact
  // on-ray result the engine already achieves.)
  const firstHops: readonly [string, "x" | "y", number][] = [
    ["spoke-north-1", "y", -1],
    ["spoke-east-1", "x", 1],
    ["spoke-south-1", "y", 1],
    ["spoke-west-1", "x", -1],
  ];
  for (const [id, axis, sign] of firstHops) {
    const position = plan.positions.get(id) as GridPosition;
    const delta = axis === "x" ? position.x - plaza.x : position.y - plaza.y;
    assert.equal(
      Math.sign(delta),
      sign,
      `hub spoke room ${id} sits on the wrong side of the plaza (delta ${delta})`,
    );
  }
});

test("locked cluster: reflow validity and the pinned core never moves", () => {
  const { request, plan } = assertDeterministic(
    () => lockedClusterArea(REALISTIC_SEEDS.lockedCluster),
    (built) => planIntegralLayout(built),
  );
  const pinned = request.residents.filter((resident) => !resident.movable);
  assert.equal(pinned.length, 40, "the user-pinned core is a 40-room neighborhood");
  assertHardValidity(request, plan);
  // Redundant with hard validity, but the class exists for this guarantee:
  // active re-charting around a user-pinned neighborhood never touches it.
  for (const resident of pinned) {
    assert.deepEqual(plan.positions.get(resident.id), resident.position);
  }
});

test("locked cluster: constraint repair is honest, seed-safe, deterministic", () => {
  const runRepair = (): {
    request: IntegralLayoutRequest;
    standard: IntegralLayoutPlan;
    repaired: IntegralLayoutPlan;
  } => {
    const request = lockedClusterArea(REALISTIC_SEEDS.lockedCluster);
    const standard = planIntegralLayout(request);
    const repaired = repairIntegralLayoutConstraints(
      request,
      standard,
      LOCKED_CLUSTER_REPAIR_OPTIONS,
    );
    return { request, standard, repaired };
  };
  const first = runRepair();
  const second = runRepair();
  assert.equal(
    canonicalPlan(second.repaired),
    canonicalPlan(first.repaired),
    "two constraint repairs must be bit-identical",
  );

  const { request, standard, repaired } = first;
  assertHardValidity(request, repaired);
  for (const resident of request.residents) {
    if (resident.movable) continue;
    assert.deepEqual(repaired.positions.get(resident.id), resident.position);
  }
  // Repair may keep its seed but must never publish something publicly worse.
  assert.ok(
    compareLayoutQuality(repaired.quality, standard.quality) >= 0,
    "constraint repair regressed the standard plan it was seeded with",
  );
  // The report is part of the public contract: present, clock-free
  // deterministic, and agreeing with the plan it describes.
  const report = repaired.constraintRepair;
  assert.ok(report, "a bounded always-on repair must return its report");
  assert.ok(second.repaired.constraintRepair);
  assert.equal(canonicalReport(second.repaired.constraintRepair), canonicalReport(report));
  assert.equal(
    report.finalViolations,
    repaired.quality.cardinalRayViolations,
    "the report's final violation count must match the published plan",
  );
});

test("disconnected components: validity, separation, isolated room placed", () => {
  const { request, plan } = assertDeterministic(
    () => disconnectedArea(REALISTIC_SEEDS.disconnected),
    (built) => planIntegralLayout(built),
  );
  assertHardValidity(request, plan);
  assert.deepEqual([...levelCensus(plan)], [0]);
  // Components must not interleave: each keeps a bounding box of its own,
  // and the isolated room lands inside neither. Cell-disjointness alone would
  // allow one partial chart to weave through another, which no reader of the
  // map could untangle.
  const box = (prefix: string): { minX: number; maxX: number; minY: number; maxY: number } => {
    let minX = Infinity;
    let maxX = -Infinity;
    let minY = Infinity;
    let maxY = -Infinity;
    for (const [id, position] of plan.positions) {
      if (!id.startsWith(prefix)) continue;
      minX = Math.min(minX, position.x);
      maxX = Math.max(maxX, position.x);
      minY = Math.min(minY, position.y);
      maxY = Math.max(maxY, position.y);
    }
    return { minX, maxX, minY, maxY };
  };
  const a = box("a-");
  const b = box("b-");
  const boxesOverlap = a.minX <= b.maxX && b.minX <= a.maxX &&
    a.minY <= b.maxY && b.minY <= a.maxY;
  assert.ok(
    !boxesOverlap,
    `disconnected components interleave: a=${JSON.stringify(a)} b=${JSON.stringify(b)}`,
  );
  const hermit = plan.positions.get("hermit") as GridPosition;
  for (const [name, bounds] of [["a", a], ["b", b]] as const) {
    const inside = hermit.x >= bounds.minX && hermit.x <= bounds.maxX &&
      hermit.y >= bounds.minY && hermit.y <= bounds.maxY;
    assert.ok(!inside, `the isolated room landed inside component ${name}`);
  }
});

test("truncated growth: per-step validity and whole-replay determinism", () => {
  const buildSteps = () => replayTruncatedGrowth(truncatedGrowthArea(REALISTIC_SEEDS.truncatedGrowth));
  const first = buildSteps();
  const second = buildSteps();
  assert.equal(first.length, 4);
  assert.equal(
    second.map((step) => canonicalPlan(step.plan)).join("\n"),
    first.map((step) => canonicalPlan(step.plan)).join("\n"),
    "two growth replays must be bit-identical at every step",
  );
  // Fixture self-check: snapshots grow strictly and only ever add rooms.
  for (let index = 1; index < first.length; index += 1) {
    const previous = new Set(first[index - 1].request.residents.map((resident) => resident.id));
    const current = first[index].request.residents;
    assert.ok(current.length > previous.size, "snapshots must grow strictly");
    let carried = 0;
    for (const resident of current) if (previous.has(resident.id)) carried += 1;
    assert.equal(carried, previous.size, "a later chart never loses a charted room");
  }
  // Each step is a complete plan in its own right: the replay substitutes the
  // previous plan's positions as the residents' charted cells, and every
  // resulting plan must satisfy the full hard-validity contract.
  for (const step of first) assertHardValidity(step.request, step.plan);
});
