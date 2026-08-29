/**
 * Golden differential corpus: deterministic scenario definitions shared by
 * `layout-golden.test.ts` and `layout-golden-update.mjs`.
 *
 * Every scenario produces the engine's exact public outputs — final positions,
 * the full quality tuple, and the deterministic work counters the results and
 * trace expose. Optimization passes that must be bit-for-bit behavior
 * preserving prove themselves against these pinned outputs.
 *
 * Nothing here may depend on wall-clock time: all repair budgets are
 * deterministic work counts with an infinite deadline, and trace telemetry
 * classes throttled by `PROGRESS_INTERVAL_MS` are excluded from the counters.
 * All randomness comes from a locally seeded xorshift32.
 */

import {
  measureIntegralLayoutQuality,
  planIntegralLayout,
  repairIntegralLayoutCrossingsDeep,
  type ConstraintRepairOptions,
  type ConstraintRepairReport,
  type GridPosition,
  type IntegralLayoutPlan,
  type IntegralLayoutRequest,
  type LayoutDirection,
  type LayoutEdge,
  type LayoutQuality,
  type LayoutTraceEvent,
} from "./layout.ts";
import {
  constraintLayoutInternalsForTesting,
  repairIntegralLayoutConstraints,
} from "./constraint-layout.ts";
import { planLayoutModel, type LayoutModel } from "./model.ts";

const at = (x: number, y: number, level = 0): GridPosition => ({ x, y, level });

/**
 * Code-unit string ordering, matching the engine's own collation. Golden
 * output ordering must be identical across installs and ICU locales.
 */
function compareStrings(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

/** The engine's own xorshift32, reseeded per scenario for full determinism. */
function xorshift32(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    return (state >>> 0) / 0x1_0000_0000;
  };
}

// ---------------------------------------------------------------------------
// Deterministic output snapshots
// ---------------------------------------------------------------------------

type PositionRow = [string, number, number, number];

function snapshotPositions(positions: ReadonlyMap<string, GridPosition>): PositionRow[] {
  return [...positions]
    .sort(([a], [b]) => compareStrings(a, b))
    .map(([id, { x, y, level }]) => [id, x, y, level]);
}

function snapshotPlan(plan: IntegralLayoutPlan): {
  positions: PositionRow[];
  movedExisting: string[];
  quality: LayoutQuality;
} {
  return {
    positions: snapshotPositions(plan.positions),
    movedExisting: [...plan.movedExisting].sort(compareStrings),
    quality: { ...plan.quality },
  };
}

/** The public repair report minus its wall-clock duration fields. */
function snapshotConstraintReport(report: Readonly<ConstraintRepairReport>): unknown {
  const { searchMs, compactionMs, polishMs, firstIncumbentMs, crossingRepair, ...rest } = report;
  void searchMs;
  void compactionMs;
  void polishMs;
  void firstIncumbentMs;
  const { elapsedMs, ...crossingRest } = crossingRepair;
  void elapsedMs;
  return { ...rest, crossingRepair: { ...crossingRest } };
}

interface TraceCounters {
  events: Record<string, number>;
  batches: Record<string, { generated: number; collisionFree: number }>;
}

/**
 * Count trace events whose emission is deterministic. Telemetry throttled by
 * wall-clock spacing is excluded entirely; event classes that mix throttled
 * and forced emissions are counted only at their deterministic completion or
 * per-occurrence variants:
 *
 * - `constraint-progress`: search/compaction phases are throttled — skipped.
 *   The polish phase publishes exactly once per planner pass — counted.
 * - `crossing-progress`: `progress` status is throttled — skipped. `complete`
 *   publishes exactly once per repair — counted, keyed by mode.
 * - `axis-progress`: intermediate emissions are throttled — skipped. The
 *   forced completion (`complete: true`) is deterministic — counted by phase.
 * - Every other class publishes once per occurrence and is counted by type.
 */
function createTraceRecorder(): {
  trace: (event: LayoutTraceEvent) => void;
  snapshot: () => TraceCounters;
} {
  const events = new Map<string, number>();
  const batches = new Map<string, { generated: number; collisionFree: number }>();
  const bump = (key: string): void => {
    events.set(key, (events.get(key) ?? 0) + 1);
  };
  const trace = (event: LayoutTraceEvent): void => {
    if (event.type === "constraint-progress") {
      if (event.phase === "polish") bump("constraint-progress:polish");
      return;
    }
    if (event.type === "crossing-progress") {
      if (event.status === "complete") bump(`crossing-progress:${event.mode}:complete`);
      return;
    }
    if (event.type === "axis-progress") {
      if (event.complete) bump(`axis-progress:${event.phase}:complete`);
      return;
    }
    if (event.type === "candidate-batch") {
      bump(`candidate-batch:${event.stage}`);
      const batch = batches.get(event.stage) ?? { generated: 0, collisionFree: 0 };
      batch.generated += event.generated;
      batch.collisionFree += event.collisionFree;
      batches.set(event.stage, batch);
      return;
    }
    bump(event.type);
  };
  const sorted = <T>(source: Map<string, T>): Record<string, T> => {
    const result: Record<string, T> = {};
    for (const key of [...source.keys()].sort(compareStrings)) {
      result[key] = source.get(key) as T;
    }
    return result;
  };
  return {
    trace,
    snapshot: () => ({ events: sorted(events), batches: sorted(batches) }),
  };
}

// ---------------------------------------------------------------------------
// Named fixtures, reconstructed locally from the behavioral suites
// ---------------------------------------------------------------------------

/** The 12-room nested deep-crossing fixture from crossing-repair.test.ts. */
function nestedDeepFixture(): IntegralLayoutRequest {
  const cells = [
    [-8, 11],
    [-5, 5],
    [10, 2],
    [-2, -3],
    [-7, -6],
    [9, -4],
    [-8, -3],
    [-8, 5],
    [8, 4],
    [1, -6],
    [5, -10],
    [-6, 0],
  ] as const;
  const parents = [0, 1, 1, 0, 4, 0, 5, 7, 2, 3, 0] as const;
  const directions = [
    "West",
    "South",
    "West",
    "South",
    "West",
    "North",
    "Other",
    "Other",
    "Other",
    "Other",
    "Other",
  ] as const;
  const reverses = {
    North: "South",
    East: "West",
    South: "North",
    West: "East",
    Other: "Other",
  } as const;
  const residents = cells.map(([x, y], index) => ({
    id: `nested-${index}`,
    position: at(x, y),
    movable: index !== 0,
  }));
  const edges: LayoutEdge[] = parents.flatMap((parent, offset) => {
    const child = offset + 1;
    const forward = directions[offset];
    return [
      { from: `nested-${parent}`, to: `nested-${child}`, direction: forward },
      { from: `nested-${child}`, to: `nested-${parent}`, direction: reverses[forward] },
    ];
  });
  return {
    residents,
    nodes: [],
    edges,
    centerId: "nested-0",
    allowExistingMoves: true,
  };
}

/** The 14-room six-crossing nested-bridge cluster from crossing-repair.test.ts. */
function sixCrossingCluster(): IntegralLayoutRequest {
  const cells = [
    [4, 4],
    [5, -2],
    [4, 1],
    [-3, -3],
    [4, 0],
    [3, -2],
    [3, 2],
    [1, 0],
    [6, -5],
    [-2, 2],
    [4, -6],
    [0, -6],
    [-1, -3],
    [-1, 4],
  ] as const;
  const parents = [0, 1, 0, 3, 4, 5, 2, 1, 4, 3, 9, 2, 5] as const;
  const id = (index: number): string => `cluster-${index}`;
  const residents = cells.map(([x, y], index) => ({
    id: id(index),
    position: at(x, y),
    movable: index !== 0,
  }));
  const edges: LayoutEdge[] = parents.flatMap((parent, offset) => {
    const child = offset + 1;
    return [
      { from: id(parent), to: id(child), direction: "Other" as const },
      { from: id(child), to: id(parent), direction: "Other" as const },
    ];
  });
  return {
    residents,
    nodes: [],
    edges,
    centerId: id(0),
    allowExistingMoves: true,
  };
}

/** The 13-room budget-monotonic fixture from crossing-repair.test.ts. */
function budgetMonotonicFixture(): IntegralLayoutRequest {
  const cells = [
    [-7, -4],
    [-8, -4],
    [-2, 2],
    [-8, -2],
    [4, 3],
    [8, -5],
    [3, 3],
    [-2, 7],
    [2, -6],
    [-6, 8],
    [3, 5],
    [5, 7],
    [8, -8],
  ] as const;
  const parents = [0, 0, 0, 1, 1, 2, 4, 1, 6, 8, 5, 2] as const;
  const residents = cells.map(([x, y], index) => ({
    id: `budget-${index}`,
    position: at(x, y),
    movable: index !== 0,
  }));
  const edges: LayoutEdge[] = parents.flatMap((parent, offset) => {
    const child = offset + 1;
    return [
      { from: `budget-${parent}`, to: `budget-${child}`, direction: "Other" as const },
      { from: `budget-${child}`, to: `budget-${parent}`, direction: "Other" as const },
    ];
  });
  return {
    residents,
    nodes: [],
    edges,
    centerId: "budget-0",
    allowExistingMoves: true,
  };
}

/** The soft-separator fixture from constraint-layout.test.ts. */
function softSeparatorFixture(): {
  request: IntegralLayoutRequest;
  standard: IntegralLayoutPlan;
} {
  const residents = [
    { id: "from", position: at(0, 0), movable: true },
    { id: "to", position: at(0, -3), movable: true },
    { id: "block-a", position: at(0, -1), movable: true },
    { id: "block-b", position: at(0, -2), movable: true },
  ];
  const edges: LayoutEdge[] = [
    { from: "from", to: "to", direction: "North" },
    { from: "to", to: "from", direction: "South" },
    { from: "block-a", to: "block-b", direction: "North" },
    { from: "block-b", to: "block-a", direction: "South" },
  ];
  const request: IntegralLayoutRequest = {
    residents,
    nodes: [],
    edges,
    allowExistingMoves: true,
  };
  return {
    request,
    standard: planIntegralLayout({ ...request, allowExistingMoves: false }),
  };
}

/** The infeasible-triangle repair fixture from constraint-layout.test.ts. */
function infeasibleTriangleFixture(): {
  request: IntegralLayoutRequest;
  standard: IntegralLayoutPlan;
} {
  const positions = new Map<string, GridPosition>([
    ["a", at(0, 0)],
    ["b", at(1, 0)],
    ["c", at(2, 0)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "a", to: "b", direction: "East" },
    { from: "b", to: "c", direction: "East" },
    { from: "c", to: "a", direction: "East" },
  ];
  return {
    request: {
      nodes: [],
      residents: [...positions].map(([id, position]) => ({ id, position, movable: true })),
      edges,
      allowExistingMoves: true,
    },
    standard: {
      positions,
      movedExisting: new Set(),
      quality: {
        cardinalRayViolations: 1,
        reciprocalRayViolations: 0,
        routingViolations: 0,
        exitPortViolations: 0,
        reciprocalExitPortViolations: 0,
        roomObstructions: 0,
        linkCrossings: 0,
        cardinalSlack: 0,
        footprintArea: 3,
        footprintPerimeter: 8,
      },
    },
  };
}

/** The 9-room multi-anchor polish fixture from constraint-layout.test.ts. */
function multiAnchorPolishFixture(): {
  request: IntegralLayoutRequest;
  seed: IntegralLayoutPlan;
} {
  const residents = [
    { id: "0", position: at(3, -2), movable: true },
    { id: "1", position: at(1, 4), movable: true },
    { id: "2", position: at(5, -1), movable: true },
    { id: "3", position: at(-6, 3), movable: true },
    { id: "4", position: at(0, -4), movable: true },
    { id: "5", position: at(-4, -2), movable: true },
    { id: "6", position: at(4, -5), movable: true },
    { id: "7", position: at(1, 3), movable: true },
    { id: "8", position: at(1, 0), movable: true },
  ];
  const edges: LayoutEdge[] = [
    { from: "0", to: "1", direction: "North" },
    { from: "1", to: "0", direction: "South" },
    { from: "0", to: "2", direction: "West" },
    { from: "2", to: "0", direction: "East" },
    { from: "0", to: "3", direction: "East" },
    { from: "3", to: "0", direction: "West" },
    { from: "1", to: "4", direction: "East" },
    { from: "4", to: "1", direction: "West" },
    { from: "4", to: "5", direction: "West" },
    { from: "5", to: "4", direction: "East" },
    { from: "1", to: "6", direction: "North" },
    { from: "6", to: "1", direction: "South" },
    { from: "0", to: "7", direction: "West" },
    { from: "7", to: "0", direction: "East" },
    { from: "2", to: "8", direction: "East" },
    { from: "8", to: "2", direction: "West" },
    { from: "6", to: "0", direction: "West" },
    { from: "0", to: "6", direction: "East" },
    { from: "6", to: "0", direction: "South" },
    { from: "0", to: "6", direction: "North" },
  ];
  const request: IntegralLayoutRequest = {
    residents,
    nodes: [],
    edges,
    centerId: "8",
    allowExistingMoves: true,
  };
  const seed = planIntegralLayout({
    ...request,
    residents: residents.map((resident) => ({
      ...resident,
      movable: resident.id !== request.centerId,
    })),
  });
  return { request, seed };
}

// ---------------------------------------------------------------------------
// Seeded random areas
// ---------------------------------------------------------------------------

const CARDINALS: readonly LayoutDirection[] = ["North", "East", "South", "West"];
const CARDINAL_REVERSE: Partial<Record<LayoutDirection, LayoutDirection>> = {
  North: "South",
  East: "West",
  South: "North",
  West: "East",
};

export interface SeededArea {
  request: IntegralLayoutRequest;
  addRoom: { from: string; direction: LayoutDirection };
}

/**
 * Build a deterministic pseudo-random area: a random spanning forest over
 * grid adjacency whose edges are ~80% reciprocal cardinal pairs, ~10% one-way
 * cardinal exits, and ~10% reciprocal "Other" links, with initial positions
 * scattered on a rough jittered grid plus a few deliberate collisions and
 * gross misplacements. Deriving the topology from grid adjacency keeps the
 * scatter mostly consistent with it, so the jitter and the deliberate defects
 * are the dominant repair workload — a plausible auto-mapper area rather than
 * a global untangling problem. Areas beyond a hundred rooms split into
 * disconnected column-band islands of roughly seventy-five rooms, the way
 * partially charted areas hold several components. Room 0 is the immovable
 * center. The same builder is duplicated verbatim in the local bench probe so
 * both measure identical workloads.
 */
export function seededArea(roomCount: number, seed: number): SeededArea {
  const random = xorshift32(seed);
  const id = (index: number): string => `r${String(index).padStart(3, "0")}`;
  const width = Math.ceil(Math.sqrt(roomCount));
  const islandCount = Math.max(1, Math.floor(roomCount / 75));
  const bandColumns = Math.ceil(width / islandCount);

  // Every room joins its island through its West or North grid neighbor; the
  // first column of each band starts a new island. Edge orientation is
  // randomized so all four cardinals appear as forward exits.
  const edges: LayoutEdge[] = [];
  for (let index = 1; index < roomCount; index += 1) {
    const column = index % width;
    const row = Math.floor(index / width);
    const west = column % bandColumns > 0 ? index - 1 : undefined;
    const north = row > 0 ? index - width : undefined;
    // A first-row, first-band-column room has no in-island neighbor: it roots
    // a new island component.
    if (west === undefined && north === undefined) continue;
    const viaWest = west !== undefined && (north === undefined || random() < 0.5);
    const parent = viaWest ? west as number : north as number;
    const outward: LayoutDirection = viaWest ? "East" : "South";
    const flip = random() < 0.5;
    const from = flip ? id(index) : id(parent);
    const to = flip ? id(parent) : id(index);
    const direction = flip ? CARDINAL_REVERSE[outward] as LayoutDirection : outward;
    const roll = random();
    if (roll < 0.8) {
      edges.push({ from, to, direction });
      edges.push({ from: to, to: from, direction: CARDINAL_REVERSE[direction] as LayoutDirection });
    } else if (roll < 0.9) {
      edges.push({ from, to, direction });
    } else {
      edges.push({ from, to, direction: "Other" });
      edges.push({ from: to, to: from, direction: "Other" });
    }
  }

  // A rough grid: packed unit cells with a one-cell aisle after every third
  // column, so removable slack exists at every aisle without every horizontal
  // edge carrying it. Rooms just west of an aisle occasionally drift into it,
  // which misaligns their vertical exits off their protected rays and moves
  // the slack to their west link. The defects below add the collision and
  // long-link repair workload on top.
  const positions: GridPosition[] = [];
  for (let index = 0; index < roomCount; index += 1) {
    const column = index % width;
    const row = Math.floor(index / width);
    const beforeAisle = (column + 1) % 3 === 0;
    const drift = random() < 0.3 && beforeAisle ? 1 : 0;
    positions.push(at(column + Math.floor(column / 3) + drift, row));
  }
  // Deliberate local collisions: a room observed onto its grid neighbor's
  // cell, the way duplicated auto-mapper observations land.
  const collisionCount = Math.max(1, Math.floor(roomCount / 40));
  for (let count = 0; count < collisionCount; count += 1) {
    const victim = 1 + Math.floor(random() * (roomCount - 1));
    const neighbor = victim % width > 0 ? victim - 1 : victim - width;
    if (neighbor >= 0) positions[victim] = { ...positions[neighbor] };
  }
  // Deliberate gross misplacements: a few rooms dropped well away from their
  // topological home, whose links chord across the area.
  const misplacementCount = Math.max(1, Math.floor(roomCount / 100));
  for (let count = 0; count < misplacementCount; count += 1) {
    const victim = 1 + Math.floor(random() * (roomCount - 1));
    const distance = 8 + Math.floor(random() * 5);
    const signX = random() < 0.5 ? -1 : 1;
    const signY = random() < 0.5 ? -1 : 1;
    positions[victim] = at(
      positions[victim].x + signX * distance,
      positions[victim].y + signY * distance,
    );
  }

  const residents = positions.map((position, index) => ({
    id: id(index),
    position,
    movable: index !== 0,
  }));
  const addRoom = {
    from: id(1 + Math.floor(random() * (roomCount - 1))),
    direction: CARDINALS[Math.floor(random() * CARDINALS.length)],
  };
  return {
    request: {
      residents,
      nodes: [],
      edges,
      centerId: id(0),
      allowExistingMoves: true,
    },
    addRoom,
  };
}

/** Rebuild a `LayoutModel` snapshot from a request and a planned layout. */
function modelFromPlan(request: IntegralLayoutRequest, plan: IntegralLayoutPlan): LayoutModel {
  return {
    rooms: request.residents.map((resident) => ({
      id: resident.id,
      position: plan.positions.get(resident.id) ?? resident.position,
      movable: resident.movable,
    })),
    edges: request.edges,
  };
}

// ---------------------------------------------------------------------------
// Scenario runners
// ---------------------------------------------------------------------------

function runReflow(request: IntegralLayoutRequest): {
  plan: IntegralLayoutPlan;
  result: unknown;
} {
  const recorder = createTraceRecorder();
  const plan = planIntegralLayout({ ...request, trace: recorder.trace });
  return {
    plan,
    result: { plan: snapshotPlan(plan), trace: recorder.snapshot() },
  };
}

function runDeepCrossing(
  request: IntegralLayoutRequest,
  seed: IntegralLayoutPlan,
  maximumWork: number,
): unknown {
  const recorder = createTraceRecorder();
  const repaired = repairIntegralLayoutCrossingsDeep(
    { ...request, trace: recorder.trace },
    seed,
    { maximumWork },
  );
  return {
    maximumWork,
    completed: repaired.completed,
    cancelled: repaired.cancelled,
    exhausted: repaired.exhausted,
    stats: { ...repaired.stats },
    plan: snapshotPlan(repaired.plan),
    trace: recorder.snapshot(),
  };
}

function runConstraintRepair(
  request: IntegralLayoutRequest,
  standard: IntegralLayoutPlan,
  options: ConstraintRepairOptions,
): unknown {
  const recorder = createTraceRecorder();
  const repaired = repairIntegralLayoutConstraints(request, standard, options, recorder.trace);
  return {
    plan: snapshotPlan(repaired),
    report: repaired.constraintRepair
      ? snapshotConstraintReport(repaired.constraintRepair)
      : undefined,
    trace: recorder.snapshot(),
  };
}

function runAddRoom(
  model: LayoutModel,
  change: { from: string; direction: LayoutDirection },
): unknown {
  const recorder = createTraceRecorder();
  const planned = planLayoutModel(
    model,
    { type: "add-room", from: change.from, direction: change.direction },
    { trace: recorder.trace },
  );
  return {
    change,
    patch: {
      moves: [...planned.patch.moves]
        .sort((a, b) => compareStrings(a.id, b.id))
        .map((move) => ({ id: move.id, from: { ...move.from }, to: { ...move.to } })),
      placements: [...planned.patch.placements]
        .sort((a, b) => compareStrings(a.id, b.id))
        .map((placement) => ({ id: placement.id, position: { ...placement.position } })),
    },
    positions: snapshotPositions(planned.positions),
    quality: { ...planned.quality },
    trace: recorder.snapshot(),
  };
}

function runPolish(request: IntegralLayoutRequest, seed: IntegralLayoutPlan): unknown {
  const recorder = createTraceRecorder();
  const polished = constraintLayoutInternalsForTesting.polish(request, seed, recorder.trace);
  return {
    plan: snapshotPlan(polished.plan),
    tournaments: polished.tournaments,
    passes: polished.passes,
    anchorsTried: polished.anchorsTried,
    improvements: polished.improvements,
    fixedPoint: polished.fixedPoint,
    cutoff: polished.cutoff,
    trace: recorder.snapshot(),
  };
}

/** Raw scattered residents as a measured seed plan, before any planning. */
function initialSeedPlan(request: IntegralLayoutRequest): IntegralLayoutPlan {
  const positions = new Map(
    request.residents.map((resident) => [resident.id, { ...resident.position }]),
  );
  return {
    positions,
    movedExisting: new Set(),
    quality: measureIntegralLayoutQuality(positions, request.edges),
  };
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

export interface GoldenScenario {
  name: string;
  run: () => unknown;
}

const RANDOM_AREA_SEEDS: Record<number, number> = {
  50: 0xA11CE5B,
  120: 0xA11CE12,
  300: 0xA11CE30,
};

/** Bounded, deterministic repair budget for the 50-room constraint scenario. */
const RANDOM_50_REPAIR_OPTIONS: ConstraintRepairOptions = {
  when: "always",
  maxDurationMs: Number.POSITIVE_INFINITY,
  maxRestarts: 12,
  maxLayouts: 2,
  maxExtensionStates: 192,
  maxMaskDiversifications: 2,
  maxPolishTournaments: 1,
  maxCrossingWork: 64,
};

const RANDOM_50_DEEP_CROSSING_WORK = 4_000;

/**
 * Lock every ninth room of the 50-room area in place. Scattered immovable
 * rooms make some directional constraints permanently unsatisfiable, so the
 * constraint scenario exercises the relaxation search, extension states, and
 * mask diversification rather than only polishing a solvable layout.
 */
function lockScatteredRooms(request: IntegralLayoutRequest): IntegralLayoutRequest {
  return {
    ...request,
    residents: request.residents.map((resident, index) =>
      index % 9 === 4 ? { ...resident, movable: false } : resident
    ),
  };
}

function memo<T>(compute: () => T): () => T {
  let value: T | undefined;
  let ready = false;
  return () => {
    if (!ready) {
      value = compute();
      ready = true;
    }
    return value as T;
  };
}

/**
 * The complete scenario list, in a stable order. Shared per-area work (the
 * full reflow whose plan seeds later scenarios) is memoized so the corpus
 * costs one reflow per area regardless of how many scenarios consume it.
 */
export function goldenScenarios(): GoldenScenario[] {
  const scenarios: GoldenScenario[] = [];

  const nested = memo(() => {
    const request = nestedDeepFixture();
    return { request, reflow: runReflow(request) };
  });
  scenarios.push({
    name: "nested-deep-12/reflow",
    run: () => nested().reflow.result,
  });
  scenarios.push({
    name: "nested-deep-12/deep-crossing-500",
    run: () => runDeepCrossing(nested().request, nested().reflow.plan, 500),
  });

  scenarios.push({
    name: "six-crossing-14/reflow",
    run: () => runReflow(sixCrossingCluster()).result,
  });

  const budget = memo(() => {
    const request = budgetMonotonicFixture();
    return { request, seed: initialSeedPlan(request) };
  });
  scenarios.push({
    name: "budget-monotonic-13/deep-crossing-20",
    run: () => runDeepCrossing(budget().request, budget().seed, 20),
  });
  scenarios.push({
    name: "budget-monotonic-13/deep-crossing-80",
    run: () => runDeepCrossing(budget().request, budget().seed, 80),
  });

  scenarios.push({
    name: "soft-separator/constraint-repair",
    run: () => {
      const { request, standard } = softSeparatorFixture();
      return runConstraintRepair(request, standard, {
        when: "always",
        maxDurationMs: Number.POSITIVE_INFINITY,
        maxLayouts: 1,
        maxExtensionStates: 2,
        maxMaskDiversifications: 1,
        maxCrossingWork: 0,
      });
    },
  });

  scenarios.push({
    name: "infeasible-triangle/constraint-repair",
    run: () => {
      const { request, standard } = infeasibleTriangleFixture();
      return runConstraintRepair(request, standard, {
        when: "always",
        maxDurationMs: Number.POSITIVE_INFINITY,
        maxRestarts: Number.POSITIVE_INFINITY,
        maxLayouts: 5,
      });
    },
  });

  scenarios.push({
    name: "multi-anchor-9/polish",
    run: () => {
      const { request, seed } = multiAnchorPolishFixture();
      return runPolish(request, seed);
    },
  });

  for (const roomCount of [50, 120, 300]) {
    const area = memo(() => {
      const { request, addRoom } = seededArea(roomCount, RANDOM_AREA_SEEDS[roomCount]);
      return { request, addRoom, reflow: runReflow(request) };
    });
    scenarios.push({
      name: `random-${roomCount}/reflow`,
      run: () => area().reflow.result,
    });
    scenarios.push({
      name: `random-${roomCount}/add-room`,
      run: () => {
        const { request, addRoom, reflow } = area();
        return runAddRoom(modelFromPlan(request, reflow.plan), addRoom);
      },
    });
    if (roomCount === 50) {
      scenarios.push({
        name: "random-50/constraint-repair",
        run: () => {
          const request = lockScatteredRooms(area().request);
          const standard = planIntegralLayout(request);
          return runConstraintRepair(request, standard, RANDOM_50_REPAIR_OPTIONS);
        },
      });
      scenarios.push({
        name: "random-50/deep-crossing",
        run: () => {
          const { request } = area();
          return runDeepCrossing(request, initialSeedPlan(request), RANDOM_50_DEEP_CROSSING_WORK);
        },
      });
    }
  }

  return scenarios;
}
