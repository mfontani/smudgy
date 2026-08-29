import assert from "node:assert/strict";
import test from "node:test";
import {
  constraintLayoutInternalsForTesting as internals,
  repairIntegralLayoutConstraints,
} from "./constraint-layout.ts";
import {
  compareLayoutQuality,
  directionalViolationEdges,
  measureIntegralLayoutQuality,
  measureLayoutRoutingQuality,
  planIntegralLayout,
  type GridPosition,
  type IntegralLayoutPlan,
  type IntegralLayoutRequest,
  type LayoutDirection,
  type LayoutEdge,
  type LayoutTraceEvent,
} from "./layout.ts";
import { isLayoutWorkerProgress, LAYOUT_WORKER_PROTOCOL_VERSION } from "./worker-protocol.ts";

const at = (x: number, y: number, level = 0): GridPosition => ({ x, y, level });

function chain(size: number, cycle: boolean): {
  positions: Map<string, GridPosition>;
  edges: LayoutEdge[];
} {
  const id = (index: number) => `r${String(index).padStart(5, "0")}`;
  const positions = new Map<string, GridPosition>();
  const edges: LayoutEdge[] = [];
  for (let index = 0; index < size; index += 1) {
    positions.set(id(index), at(index, 0));
    if (index > 0) edges.push({ from: id(index - 1), to: id(index), direction: "East" });
  }
  if (cycle) edges.push({ from: id(size - 1), to: id(0), direction: "East" });
  return { positions, edges };
}

function repairFixture(): {
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

function multiAnchorPolishFixture(centerId = "8"): {
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
    centerId,
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

function unfinishedCrossingFixture(): IntegralLayoutRequest {
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
    id: `r${index}`,
    position: at(x, y),
    movable: index !== 0,
  }));
  const edges = parents.flatMap((parent, offset) => {
    const child = offset + 1;
    const forward = directions[offset];
    return [
      { from: `r${parent}`, to: `r${child}`, direction: forward },
      { from: `r${child}`, to: `r${parent}`, direction: reverses[forward] },
    ];
  });
  return {
    residents,
    nodes: [],
    edges,
    centerId: "r0",
    allowExistingMoves: true,
  };
}

function referenceConstraintFeasibility(
  positions: ReadonlyMap<string, GridPosition>,
  edges: readonly LayoutEdge[],
  removed: ReadonlySet<number>,
): boolean {
  const ids = [...positions.keys()].sort();
  const indexes = new Map(ids.map((id, index) => [id, index]));
  const parent = [0, 1, 2].map(() => Int32Array.from(ids, (_, index) => index));
  const find = (axis: number, value: number): number => {
    let root = value;
    while (parent[axis][root] !== root) root = parent[axis][root];
    while (parent[axis][value] !== value) {
      const next = parent[axis][value];
      parent[axis][value] = root;
      value = next;
    }
    return root;
  };
  const union = (axis: number, first: number, second: number): void => {
    const a = find(axis, first);
    const b = find(axis, second);
    if (a !== b) parent[axis][b] = a;
  };
  const relations: { axis: number; low: number; high: number }[] = [];
  for (let index = 0; index < edges.length; index += 1) {
    const edge = edges[index];
    const from = indexes.get(edge.from);
    const to = indexes.get(edge.to);
    if (from === undefined || to === undefined) continue;
    const vector = edge.direction === "East" ? [1, 0, 0]
      : edge.direction === "West" ? [-1, 0, 0]
      : edge.direction === "South" ? [0, 1, 0]
      : edge.direction === "North" ? [0, -1, 0]
      : edge.direction === "Up" ? [0, 0, 1]
      : edge.direction === "Down" ? [0, 0, -1]
      : undefined;
    if (!vector) continue;
    const axis = vector.findIndex((value) => value !== 0);
    if (axis !== 2) union(2, from, to);
    if (removed.has(index)) continue;
    for (let other = 0; other < 3; other += 1) {
      if (other !== axis) union(other, from, to);
    }
    relations.push({
      axis,
      low: vector[axis] > 0 ? from : to,
      high: vector[axis] > 0 ? to : from,
    });
  }

  for (const relation of relations) {
    if (find(relation.axis, relation.low) === find(relation.axis, relation.high)) return false;
  }
  for (let axis = 0; axis < 3; axis += 1) {
    const outgoing = new Map<number, number[]>();
    const indegree = new Map<number, number>();
    for (let node = 0; node < ids.length; node += 1) {
      const root = find(axis, node);
      if (!outgoing.has(root)) outgoing.set(root, []);
      indegree.set(root, 0);
    }
    for (const relation of relations) {
      if (relation.axis !== axis) continue;
      const from = find(axis, relation.low);
      const to = find(axis, relation.high);
      outgoing.get(from)?.push(to);
      indegree.set(to, (indegree.get(to) ?? 0) + 1);
    }
    const ready = [...indegree].filter(([, degree]) => degree === 0).map(([root]) => root);
    let visited = 0;
    while (ready.length > 0) {
      const root = ready.pop() as number;
      visited += 1;
      for (const target of outgoing.get(root) ?? []) {
        const degree = (indegree.get(target) as number) - 1;
        indegree.set(target, degree);
        if (degree === 0) ready.push(target);
      }
    }
    if (visited !== indegree.size) return false;
  }

  const triples = new Set<string>();
  for (let node = 0; node < ids.length; node += 1) {
    const triple = `${find(0, node)}:${find(1, node)}:${find(2, node)}`;
    if (triples.has(triple)) return false;
    triples.add(triple);
  }
  return true;
}

test("iterative constraint traversal handles 6,000- and 15,000-node chains", () => {
  const acyclic = chain(6_000, false);
  const acyclicResult = internals.analyze(acyclic.positions, acyclic.edges);
  assert.equal(acyclicResult.ok, true);
  assert.equal(acyclicResult.ok && acyclicResult.feasible, true);

  const cyclic = chain(15_000, true);
  const cyclicResult = internals.analyze(cyclic.positions, cyclic.edges);
  assert.equal(cyclicResult.ok, true);
  assert.equal(cyclicResult.ok && cyclicResult.feasible, false);
  assert.equal(cyclicResult.ok && cyclicResult.conflictSourceIndexes?.length, 15_000);
});

test("hard validity rejects fractional coordinates instead of rounding them", () => {
  const positions = new Map<string, GridPosition>([
    ["a", at(0, 0)],
    ["b", at(1, 0)],
  ]);
  const edges: LayoutEdge[] = [{ from: "a", to: "b", direction: "East" }];
  assert.equal(internals.hardValid(positions, edges, positions), true);
  assert.equal(internals.hardValid(positions, edges, new Map([
    ["a", at(0, 0)],
    ["b", at(1.25, 0)],
  ])), false);
  assert.equal(internals.hardValid(positions, edges, new Map([
    ["a", at(0, 0)],
    ["b", at(Number.MAX_SAFE_INTEGER + 1, 0)],
  ])), false);
});

test("relaxing a planar ray never relaxes its hard same-level topology", () => {
  const positions = new Map<string, GridPosition>([
    ["a", at(0, 0)],
    ["b", at(1, 0)],
  ]);
  const edges: LayoutEdge[] = [{ from: "a", to: "b", direction: "East" }];
  const movedOffRay = new Map<string, GridPosition>([
    ["a", at(0, 0)],
    ["b", at(-3, 4)],
  ]);
  const movedOffLevel = new Map<string, GridPosition>([
    ["a", at(0, 0)],
    ["b", at(-3, 4, 1)],
  ]);

  assert.equal(internals.hardValid(positions, edges, movedOffRay, [0]), true);
  assert.equal(internals.hardValid(positions, edges, movedOffLevel, [0]), false);
});

test("hard validity freezes absolute levels outside level-crossing components", () => {
  const positions = new Map<string, GridPosition>([
    ["plane-a", at(0, 0, 7)],
    ["plane-b", at(1, 0, 7)],
    ["isolated", at(5, 5, -3)],
  ]);
  const edges: LayoutEdge[] = [{ from: "plane-a", to: "plane-b", direction: "East" }];
  assert.equal(internals.hardValid(positions, edges, new Map<string, GridPosition>([
    ["plane-a", at(0, 0, 8)],
    ["plane-b", at(1, 0, 8)],
    ["isolated", at(5, 5, -3)],
  ]), [0]), false, "a whole planar component cannot drift to another level");
  assert.equal(internals.hardValid(positions, edges, new Map<string, GridPosition>([
    ["plane-a", at(0, 0, 7)],
    ["plane-b", at(1, 0, 7)],
    ["isolated", at(5, 5, -2)],
  ]), [0]), false, "an isolated room cannot drift to another level");

  const verticalPositions = new Map<string, GridPosition>([
    ["lower", at(0, 0)],
    ["upper", at(0, 0, 1)],
    ["upper-east", at(1, 0, 1)],
  ]);
  const verticalEdges: LayoutEdge[] = [
    { from: "lower", to: "upper", direction: "Up" },
    { from: "upper", to: "upper-east", direction: "East" },
  ];
  assert.equal(internals.hardValid(
    verticalPositions,
    verticalEdges,
    new Map<string, GridPosition>([
      ["lower", at(0, 0, 4)],
      ["upper", at(0, 0, 5)],
      ["upper-east", at(1, 0, 5)],
    ]),
  ), true, "a planar wing inherits its component's real level-crossing reachability");
});

test("soft separator selection skips an unrepairable first defect", () => {
  const selected = internals.firstAdmissibleSeparator(
    3,
    [{ axis: 0, from: 0, to: 1 }],
    [
      {
        kind: "cycle-or-no-op",
        alternatives: [
          { arcs: [{ axis: 0, from: 1, to: 0 }] },
          { arcs: [{ axis: 0, from: 0, to: 1 }] },
        ],
      },
      {
        kind: "later-repairable",
        alternatives: [{ arcs: [{ axis: 0, from: 1, to: 2 }] }],
      },
    ],
  );
  assert.equal(selected, 1);
});

test("a legal supplied layout survives incompatible compact shifts for multiple fixed anchors", () => {
  const positions = new Map<string, GridPosition>([
    ["a", at(0, 0)],
    ["b", at(5, 0)],
  ]);
  const edges: LayoutEdge[] = [{ from: "a", to: "b", direction: "East" }];
  const request: IntegralLayoutRequest = {
    residents: [
      { id: "a", position: at(0, 0), movable: false },
      { id: "b", position: at(5, 0), movable: false },
    ],
    nodes: [],
    edges,
    allowExistingMoves: true,
  };
  const standard: IntegralLayoutPlan = {
    positions,
    movedExisting: new Set(),
    quality: measureIntegralLayoutQuality(positions, edges),
  };
  const trace: LayoutTraceEvent[] = [];
  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: 1,
    maxLayouts: 1,
    maxExtensionStates: 0,
    maxMaskDiversifications: 1,
    maxCrossingWork: 0,
  });

  assert.deepEqual(repaired.positions, positions);
  assert.equal(repaired.constraintRepair?.rawIncumbents, 1);
  assert.equal(repaired.constraintRepair?.distinctLayouts, 1);
  assert.equal(repaired.constraintRepair?.separatorStates, 0);
  assert.equal(repaired.constraintRepair?.cutoff, "extensions");
  assert.deepEqual(repaired.constraintRepair?.extensionSearch, {
    completed: false,
    cancelled: false,
    exhausted: true,
  });
  assert.equal(repaired.constraintRepair?.geometricFixedPoint, false);
});

test("expected search and compaction failures return the supplied standard plan", () => {
  const { request, standard } = repairFixture();
  for (const failure of ["search", "compaction"] as const) {
    const trace: LayoutTraceEvent[] = [];
    const result = internals.repairWithFailure(
      request,
      standard,
      { when: "always", maxDurationMs: 1_000 },
      failure,
      (event) => trace.push(event),
    );
    assert.strictEqual(result, standard);
    assert.deepEqual(trace, []);
  }
});

test("deep repair polishes only distinct complete layouts within its configured bound", () => {
  const { request, standard } = repairFixture();
  const trace: LayoutTraceEvent[] = [];
  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: Number.POSITIVE_INFINITY,
    maxLayouts: 5,
  }, (event) => trace.push(event));

  const report = repaired.constraintRepair;
  assert.ok(report && report.layoutsConsidered >= 1 && report.layoutsConsidered <= 5);
  assert.ok(report && report.distinctLayouts >= report.layoutsConsidered);
  assert.ok(report && report.rawIncumbents >= report.distinctLayouts);
  assert.equal(repaired.constraintRepair?.cutoff, "none");
  assert.equal(report?.compactionAttempts, report?.maskDiversifications);
  assert.ok((report?.maskDiversifications ?? 0) >= 1);
  assert.ok(trace.some((event) =>
    event.type === "constraint-repair" && event.report.layoutsConsidered === report?.layoutsConsidered
  ));
  assert.equal(repaired.constraintRepair?.constraintOptimal, repaired.constraintRepair?.optimal);
  assert.equal(repaired.constraintRepair?.geometricFixedPoint, true);
  assert.equal(repaired.constraintRepair?.polishCutoff, "fixed-point");
  assert.ok((repaired.constraintRepair?.polishPasses ?? 0) > 0);
});

test("layout cutoff requires an actually unprocessed polish entry", () => {
  const { request, standard } = repairFixture();
  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: Number.POSITIVE_INFINITY,
    maxLayouts: 1,
  });

  assert.equal(repaired.constraintRepair?.distinctLayouts, 1);
  assert.equal(repaired.constraintRepair?.layoutsConsidered, 1);
  assert.equal(repaired.constraintRepair?.cutoff, "none");
});

test("geometric fixed point requires crossing repair to finish", () => {
  const request = unfinishedCrossingFixture();
  const standard = planIntegralLayout({ ...request, allowExistingMoves: false });
  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: 1,
    maxLayouts: 16,
    maxCrossingWork: 0,
  });

  const report = repaired.constraintRepair;
  assert.ok(report);
  assert.equal(report.layoutsConsidered, report.distinctLayouts);
  assert.equal(report.cutoff, "none", "the complete layout frontier was not capped");
  assert.deepEqual({
    completed: report.crossingRepair.completed,
    cancelled: report.crossingRepair.cancelled,
    exhausted: report.crossingRepair.exhausted,
  }, { completed: false, cancelled: false, exhausted: true });
  assert.equal(report.polishCutoff, "fixed-point");
  assert.equal(report.extensionSearch.completed, true);
  assert.equal(report.maskDiversification.completed, true);
  assert.equal(report.geometricFixedPoint, false);

  // The tournament cap keeps the truncated winner imperfect: an unbounded
  // fixed-point polish on this fixture reaches a perfect layout from the one
  // retained frontier entry, and a perfect winner truthfully reports "none"
  // rather than the truncation this variant exists to pin.
  const truncated = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: 1,
    maxLayouts: 1,
    maxPolishTournaments: 0,
    maxCrossingWork: 0,
  });
  assert.ok((truncated.constraintRepair?.distinctLayouts ?? 0) > 1);
  assert.equal(truncated.constraintRepair?.layoutsConsidered, 1);
  assert.equal(truncated.constraintRepair?.cutoff, "layouts");
  assert.equal(truncated.constraintRepair?.geometricFixedPoint, false);
});

test("post-constraint polish reaches the multi-anchor reflow fixed point", () => {
  const { request, seed } = multiAnchorPolishFixture();
  const trace: LayoutTraceEvent[] = [];
  const polished = internals.polish(request, seed, (event) => trace.push(event));

  assert.ok(compareLayoutQuality(polished.plan.quality, seed.quality) > 0);
  assert.equal(polished.fixedPoint, true);
  assert.equal(polished.cutoff, "fixed-point");
  assert.ok(polished.tournaments >= 2, "one improving tournament plus a fixed-point proof");
  assert.ok(polished.passes > polished.tournaments);
  assert.ok(polished.improvements > 0);
  assert.ok(trace.some((event) => event.type === "constraint-improvement"));
  assert.equal(
    trace.filter((event) => event.type === "constraint-progress" && event.phase === "polish").length,
    polished.passes + 1,
  );

  const repeated = internals.polish(request, polished.plan);
  assert.equal(compareLayoutQuality(repeated.plan.quality, polished.plan.quality), 0);
  assert.equal(repeated.tournaments, 1);
  assert.equal(repeated.improvements, 0);
});

test("a one-pass anchored preview publishes before MaxHS without starving it", () => {
  const { request, seed } = multiAnchorPolishFixture("0");
  const trace: LayoutTraceEvent[] = [];
  const repaired = repairIntegralLayoutConstraints(request, seed, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: 1,
    maxLayouts: 1,
    maxPolishTournaments: 2,
    maxPolishPasses: 2,
    maxExtensionStates: 0,
    maxMaskDiversifications: 1,
    maxCrossingWork: 0,
  }, (event) => trace.push(event));

  const first = trace.find((event) => event.type === "constraint-improvement");
  assert.ok(first && first.type === "constraint-improvement");
  assert.equal(first.feasibilityChecks, 0);
  assert.equal(first.compactionAttempts, 0);
  assert.equal(first.separatorStates, 0);
  assert.equal(first.layoutsConsidered, 1, "the complete preview pass is charged before publication");
  assert.ok(compareLayoutQuality(first.candidate.quality, seed.quality) > 0);
  assert.ok(compareLayoutQuality(repaired.quality, first.candidate.quality) >= 0);
  assert.ok(repaired.constraintRepair);
  assert.ok(repaired.constraintRepair.feasibilityChecks > 0, "MaxHS began after the preview");
  assert.equal(repaired.constraintRepair.polishPasses, 2);
  assert.equal(repaired.constraintRepair.polishAnchorsTried, 2);
  assert.equal(repaired.constraintRepair.polishTournaments, 0);
  assert.equal(repaired.constraintRepair.polishCutoff, "passes");
  assert.equal(repaired.constraintRepair.geometricFixedPoint, false);
  const streamed = trace.filter((event): event is Extract<LayoutTraceEvent, {
    type: "constraint-progress" | "constraint-improvement";
  }> => event.type === "constraint-progress" || event.type === "constraint-improvement");
  let layoutsConsidered = 0;
  let bestQuality = seed.quality;
  for (const event of streamed) {
    assert.ok(event.layoutsConsidered >= layoutsConsidered);
    layoutsConsidered = event.layoutsConsidered;
    const quality = event.type === "constraint-improvement" ? event.candidate.quality : event.bestQuality;
    if (!quality) continue;
    assert.ok(compareLayoutQuality(quality, bestQuality) >= 0);
    bestQuality = quality;
  }
});

test("a deterministic polish tournament ceiling retains the last complete winner", () => {
  const { request, seed } = multiAnchorPolishFixture();

  const skipped = internals.polish(request, seed, undefined, { maximumTournaments: 0 });
  assert.strictEqual(skipped.plan, seed);
  assert.equal(skipped.tournaments, 0);
  assert.equal(skipped.passes, 0);
  assert.equal(skipped.improvements, 0);
  assert.equal(skipped.fixedPoint, false);
  assert.equal(skipped.cutoff, "tournaments");

  const bounded = internals.polish(request, seed, undefined, { maximumTournaments: 1 });
  assert.equal(bounded.tournaments, 1, "tournament two was never started");
  assert.ok(bounded.passes > 1);
  assert.ok(bounded.improvements > 0);
  assert.ok(compareLayoutQuality(bounded.plan.quality, seed.quality) > 0);
  assert.equal(bounded.fixedPoint, false);
  assert.equal(bounded.cutoff, "tournaments");

  const passBounded = internals.polish(request, seed, undefined, { maximumPasses: 1 });
  assert.equal(passBounded.tournaments, 0, "an incomplete tournament is not charged as complete");
  assert.equal(passBounded.passes, 1);
  assert.equal(passBounded.anchorsTried, 1);
  assert.equal(passBounded.fixedPoint, false);
  assert.equal(passBounded.cutoff, "passes");
});

test("the public repair report truthfully identifies a polish tournament cutoff", () => {
  const { request, standard } = repairFixture();
  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: 1,
    maxLayouts: 1,
    maxPolishTournaments: 0,
    maxCrossingWork: 0,
  });

  const report = repaired.constraintRepair;
  assert.ok(report);
  assert.equal(report.polishTournaments, 0);
  assert.equal(report.polishPasses, 0);
  assert.equal(report.polishImprovements, 0);
  assert.equal(report.polishCutoff, "tournaments");
  assert.equal(report.geometricFixedPoint, false);
});

test("finite polish skips an expired budget and keeps the last complete adopted plan", () => {
  const { request, seed } = multiAnchorPolishFixture();
  const preExpiredTrace: LayoutTraceEvent[] = [];
  const preExpired = internals.polish(
    request,
    seed,
    (event) => preExpiredTrace.push(event),
    { now: () => 5, deadline: 5 },
  );
  assert.strictEqual(preExpired.plan, seed);
  assert.equal(preExpired.cutoff, "time");
  assert.equal(preExpired.fixedPoint, false);
  assert.equal(preExpired.tournaments, 0);
  assert.equal(preExpired.passes, 0);
  assert.equal(preExpired.improvements, 0);
  assert.equal(preExpiredTrace.length, 1);
  assert.equal(preExpiredTrace[0].type, "constraint-progress");

  let expired = false;
  const cutoffTrace: LayoutTraceEvent[] = [];
  const cutoff = internals.polish(
    request,
    seed,
    (event) => {
      cutoffTrace.push(event);
      // Expire immediately after the first complete improvement callback. The
      // nested planner may still emit synchronous events, so the sentinel must
      // be checked before event filtering and the adopted winner must survive.
      if (event.type === "constraint-improvement") expired = true;
    },
    { now: () => expired ? 1 : 0, deadline: 1 },
  );
  assert.equal(cutoff.cutoff, "time");
  assert.equal(cutoff.fixedPoint, false);
  assert.equal(cutoff.tournaments, 0, "the interrupted tournament never completed");
  assert.ok(cutoff.improvements > 0);
  assert.ok(compareLayoutQuality(cutoff.plan.quality, seed.quality) > 0);
  assert.ok(cutoffTrace.some((event) => event.type === "constraint-improvement"));
});

test("infinite polish keeps the fixed-point path without cooperative cutoff", () => {
  const { request, seed } = multiAnchorPolishFixture();
  const polished = internals.polish(request, seed, undefined, {
    now: () => 0,
    deadline: Number.POSITIVE_INFINITY,
  });
  assert.equal(polished.cutoff, "fixed-point");
  assert.equal(polished.fixedPoint, true);
  assert.ok(polished.tournaments >= 2);
  assert.ok(polished.improvements > 0);
});

test("an already clean layout retains its gain but reports a truncated polish frontier", () => {
  const positions = new Map<string, GridPosition>([
    ["a", at(0, 0)],
    ["b", at(10, 0)],
    ["c", at(20, 0)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "a", to: "b", direction: "East" },
    { from: "b", to: "a", direction: "West" },
    { from: "b", to: "c", direction: "East" },
    { from: "c", to: "b", direction: "West" },
  ];
  const request: IntegralLayoutRequest = {
    residents: [...positions].map(([id, position]) => ({ id, position, movable: true })),
    nodes: [],
    edges,
    centerId: "b",
    allowExistingMoves: true,
  };
  const standard: IntegralLayoutPlan = {
    positions,
    movedExisting: new Set(),
    quality: measureIntegralLayoutQuality(positions, edges),
  };
  assert.equal(standard.quality.cardinalRayViolations, 0);
  assert.equal(standard.quality.routingViolations, 0);
  assert.equal(standard.quality.linkCrossings, 0);

  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxLayouts: 1,
  });
  assert.ok(compareLayoutQuality(repaired.quality, standard.quality) > 0);
  assert.equal(repaired.quality.cardinalSlack, 0);
  assert.ok(
    (repaired.constraintRepair?.distinctLayouts ?? 0) >
      (repaired.constraintRepair?.layoutsConsidered ?? 0),
  );
  assert.equal(repaired.constraintRepair?.cutoff, "layouts");
  assert.equal(repaired.constraintRepair?.geometricFixedPoint, false);
  assert.ok((repaired.constraintRepair?.polishPasses ?? 0) > 0);
});

test("deep repair anchors locked residents instead of skipping the area", () => {
  const residents = [
    { id: "a", position: at(0, 0), movable: true },
    { id: "b", position: at(1, 0), movable: true },
    { id: "c", position: at(2, 0), movable: true },
    { id: "locked", position: at(10, 7), movable: false },
  ];
  const edges: LayoutEdge[] = [
    { from: "a", to: "b", direction: "East" },
    { from: "b", to: "c", direction: "East" },
    { from: "c", to: "a", direction: "East" },
  ];
  const request: IntegralLayoutRequest = {
    nodes: [],
    residents,
    edges,
    allowExistingMoves: true,
  };
  const standard = planIntegralLayout({ ...request, allowExistingMoves: false });
  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: 1_000,
    maxLayouts: 2,
  });

  assert.ok(repaired.constraintRepair, "the constraint stage ran despite the lock");
  assert.deepEqual(repaired.positions.get("locked"), at(10, 7));
  assert.equal(repaired.movedExisting.has("locked"), false);
});

test("deep repair separates two otherwise clean crossing corridors", () => {
  const residents = [
    { id: "west", position: at(-2, 0), movable: true },
    { id: "east", position: at(2, 0), movable: true },
    { id: "north", position: at(0, -2), movable: true },
    { id: "south", position: at(0, 2), movable: true },
  ];
  const edges: LayoutEdge[] = [
    { from: "west", to: "east", direction: "East" },
    { from: "east", to: "west", direction: "West" },
    { from: "north", to: "south", direction: "South" },
    { from: "south", to: "north", direction: "North" },
  ];
  const request: IntegralLayoutRequest = { residents, nodes: [], edges, allowExistingMoves: true };
  const standard = planIntegralLayout({ ...request, allowExistingMoves: false });
  assert.equal(standard.quality.linkCrossings, 1);

  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxLayouts: 8,
  });
  assert.equal(repaired.quality.cardinalRayViolations, 0);
  assert.equal(repaired.quality.routingViolations, 0);
  assert.equal(repaired.quality.linkCrossings, 0);
  assert.equal(repaired.constraintRepair?.selected, true);
});

test("constraint compaction separates unrelated rooms from a protected corridor", () => {
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
  const standard = planIntegralLayout({ ...request, allowExistingMoves: false });
  assert.deepEqual(measureLayoutRoutingQuality(standard.positions, edges), {
    routingViolations: 4,
    exitPortViolations: 2,
    reciprocalExitPortViolations: 2,
    roomObstructions: 2,
  });

  const repaired = repairIntegralLayoutConstraints(
    request,
    standard,
    { when: "always", maxDurationMs: 1_000 },
  );
  assert.equal(repaired.constraintRepair?.selected, true);
  assert.equal(repaired.quality.cardinalRayViolations, 0);
  assert.equal(repaired.quality.routingViolations, 0);
  assert.equal(repaired.quality.exitPortViolations, 0);
  assert.equal(repaired.quality.roomObstructions, 0);
});

test("compaction pulls a dangling source flush against its retained relation", () => {
  // Longest-path ranks alone would leave `u` at rank zero of row one — its
  // East relation into `v` carrying two cells of slack — because `v`'s column
  // rank is driven by the unrelated `p` chain in row zero. The per-axis raise
  // pass moves `u` to its tightest outgoing bound instead, and row one is
  // empty there, so the very first built state is already slack-free.
  const positions = new Map<string, GridPosition>([
    ["p0", at(0, 0)],
    ["p1", at(1, 0)],
    ["p2", at(2, 0)],
    ["w", at(3, 0)],
    ["v", at(3, 1)],
    ["u", at(0, 1)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "p0", to: "p1", direction: "East" },
    { from: "p1", to: "p2", direction: "East" },
    { from: "p2", to: "w", direction: "East" },
    { from: "w", to: "v", direction: "South" },
    { from: "u", to: "v", direction: "East" },
  ];
  const compacted = internals.compact(positions, edges);
  assert.equal(compacted.ok, true);
  assert.ok(compacted.ok);
  assert.deepEqual(compacted.status, { completed: true, cancelled: false, exhausted: false });
  assert.equal(compacted.workStats.separatorStates, 1, "the first state needs no separator");
  assert.equal(compacted.quality.cardinalSlack, 0);
  const u = compacted.positions.get("u") as GridPosition;
  const v = compacted.positions.get("v") as GridPosition;
  assert.equal(u.x, v.x - 1, "u sits flush against its only retained relation");
  assert.equal(u.y, v.y);
});

test("compaction cancelled on any inspected state never reports a completed traversal", () => {
  const positions = new Map<string, GridPosition>([
    ["from", at(0, 0)],
    ["to", at(0, -3)],
    ["block-a", at(0, -1)],
    ["block-b", at(0, -2)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "from", to: "to", direction: "North" },
    { from: "to", to: "from", direction: "South" },
    { from: "block-a", to: "block-b", direction: "North" },
    { from: "block-b", to: "block-a", direction: "South" },
  ];
  const reference = internals.compact(positions, edges);
  assert.equal(reference.ok, true);
  assert.deepEqual(reference.status, { completed: true, cancelled: false, exhausted: false });
  const total = reference.incumbents.length;
  assert.ok(total >= 3, "the corridor traversal publishes several incumbents");

  // The first incumbent precedes the root frame (supplied geometry); the
  // second is published while inspecting the first candidate state; the last
  // while inspecting the final candidate state of the deterministic traversal.
  for (const cancelAtIncumbent of [1, 2, total]) {
    const cancelled = internals.compact(positions, edges, { cancelAtIncumbent });
    assert.deepEqual(
      cancelled.status,
      { completed: false, cancelled: true, exhausted: false },
      `cancellation latched at incumbent ${cancelAtIncumbent}`,
    );
    assert.equal(cancelled.incumbents.length, cancelAtIncumbent);
  }
});

test("every cancellation observation point terminates the compaction as cancelled", () => {
  const { request } = softSeparatorFixture();
  const positions = new Map(request.residents.map(({ id, position }) => [id, position]));
  let observations = 0;
  const reference = internals.compact(positions, request.edges, {
    shouldCancel: () => {
      observations += 1;
      return false;
    },
  });
  assert.equal(reference.ok, true);
  assert.deepEqual(reference.status, { completed: true, cancelled: false, exhausted: false });
  assert.ok(observations > 0);

  // A single-observation spike models the worst cooperative-cancellation
  // timing. Whichever sampled check consumes it — coordinate construction,
  // separator admission, a defect scan, or a core frame boundary — the run
  // must terminate as cancelled instead of completing around the cut.
  for (let target = 1; target <= observations; target += 1) {
    let calls = 0;
    const spiked = internals.compact(positions, request.edges, {
      shouldCancel: () => ++calls === target,
    });
    assert.deepEqual(
      spiked.status,
      { completed: false, cancelled: true, exhausted: false },
      `observation ${target} of ${observations}`,
    );
    assert.ok(spiked.incumbents.length <= reference.incumbents.length);
  }
});

test("a repair deadline cutting the extension traversal reports cancellation, never proof", () => {
  const { request, standard } = softSeparatorFixture();
  const options = {
    when: "always" as const,
    maxRestarts: 4,
    maxLayouts: 2,
    maxExtensionStates: 64,
    maxMaskDiversifications: 4,
    maxCrossingWork: 0,
  };
  // A tick-per-call clock lands the deadline on a different deterministic
  // observation point for every budget, covering cuts inside coordinate
  // construction, separator admission, and the first and final DFS frames.
  let cancelledTraversals = 0;
  for (let budget = 2; budget <= 120; budget += 1) {
    let tick = 0;
    const repaired = internals.repairWithClock(
      request,
      standard,
      { ...options, maxDurationMs: budget },
      () => tick++,
    );
    const report = repaired.constraintRepair;
    if (!report) continue;
    if (report.extensionSearch.cancelled) {
      cancelledTraversals += 1;
      assert.equal(report.extensionSearch.completed, false, `budget ${budget}`);
      assert.equal(report.maskDiversification.completed, false, `budget ${budget}`);
      assert.equal(report.cutoff, "time", `budget ${budget}`);
    }
    if (report.cutoff === "time" || report.extensionSearch.cancelled) {
      assert.equal(report.geometricFixedPoint, false, `budget ${budget}`);
    }
  }
  assert.ok(cancelledTraversals > 0, "some budget cut the extension traversal itself");
});

test("protocol validation rejects a fixed point claimed over a cut traversal", () => {
  const { request, standard } = repairFixture();
  const trace: LayoutTraceEvent[] = [];
  repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: Number.POSITIVE_INFINITY,
    maxLayouts: 5,
  }, (event) => trace.push(event));
  const reportEvent = trace.find(
    (event): event is Extract<LayoutTraceEvent, { type: "constraint-repair" }> =>
      event.type === "constraint-repair",
  );
  assert.ok(reportEvent);
  assert.equal(reportEvent.report.geometricFixedPoint, true);

  const message = (report: unknown): unknown => ({
    protocol: LAYOUT_WORKER_PROTOCOL_VERSION,
    id: 1,
    operation: "constraint-repair",
    progress: true,
    event: { type: "constraint-repair", stage: "constraint-repair", report },
  });
  assert.equal(isLayoutWorkerProgress(message(reportEvent.report)), true);
  assert.equal(isLayoutWorkerProgress(message({
    ...reportEvent.report,
    extensionSearch: { completed: false, cancelled: true, exhausted: false },
  })), false, "a cancelled extension frontier cannot carry a fixed point");
  assert.equal(isLayoutWorkerProgress(message({
    ...reportEvent.report,
    extensionSearch: { completed: true, cancelled: true, exhausted: false },
  })), false, "a completed-yet-cancelled frontier is contradictory");
  assert.equal(isLayoutWorkerProgress(message({
    ...reportEvent.report,
    cutoff: "time",
  })), false, "a deadline-cut run cannot carry a fixed point");
});

test("a bounded soft-separator search publishes and returns its hard-valid incumbent", () => {
  const { request, standard } = softSeparatorFixture();
  const { residents, edges } = request;
  const trace: LayoutTraceEvent[] = [];
  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxLayouts: 1,
    maxPolishTournaments: 0,
    maxExtensionStates: 2,
    maxMaskDiversifications: 1,
    maxCrossingWork: 0,
  }, (event) => trace.push(event));

  const report = repaired.constraintRepair;
  assert.ok(report, "a hard-valid state survives the exhausted soft search");
  assert.equal(report.separatorStates, 2);
  assert.equal(report.rawIncumbents, 2, "supplied and first compacted hard-valid states are scored");
  assert.equal(report.distinctLayouts, 2);
  assert.equal(report.softIncumbents, 1);
  assert.equal(report.cutoff, "extensions");
  assert.deepEqual(report.extensionSearch, {
    completed: false,
    cancelled: false,
    exhausted: true,
  });
  assert.equal(report.geometricFixedPoint, false);
  assert.ok(Number.isFinite(report.firstIncumbentMs));
  assert.ok(compareLayoutQuality(repaired.quality, standard.quality) > 0);

  const improvements = trace.filter((event) => event.type === "constraint-improvement");
  assert.ok(improvements.length >= 1);
  assert.deepEqual({
    states: improvements[0].separatorStates,
    branches: improvements[0].separatorBranches,
    cyclePrunes: improvements[0].separatorCyclePrunes,
  }, { states: 2, branches: 1, cyclePrunes: 0 });
  let frontier = standard.quality;
  for (const event of improvements) {
    assert.ok(compareLayoutQuality(event.candidate.quality, frontier) > 0);
    const positions = new Map(event.candidate.positions?.map(({ id, x, y, level }) => [
      id,
      { x, y, level },
    ]));
    assert.equal(positions.size, residents.length);
    assert.equal(new Set([...positions.values()].map((position) =>
      `${position.level}:${position.x}:${position.y}`
    )).size, residents.length);
    assert.equal(directionalViolationEdges(positions, edges).length, 0);
    assert.deepEqual(measureIntegralLayoutQuality(positions, edges), event.candidate.quality);
    frontier = event.candidate.quality;
  }
  assert.ok(
    compareLayoutQuality(repaired.quality, frontier) >= 0,
    "the final plan cannot retract a published hard-valid incumbent",
  );
});

test("an exhausted first geometry mask cannot starve MaxHS certification", () => {
  const soft = softSeparatorFixture();
  const positions = new Map(soft.standard.positions);
  positions.set("cycle-a", at(10, 0));
  positions.set("cycle-b", at(11, 0));
  positions.set("cycle-c", at(12, 0));
  const edges: LayoutEdge[] = [
    ...soft.request.edges,
    { from: "cycle-a", to: "cycle-b", direction: "East" },
    { from: "cycle-b", to: "cycle-c", direction: "East" },
    { from: "cycle-c", to: "cycle-a", direction: "East" },
  ];
  const request: IntegralLayoutRequest = {
    residents: [...positions].map(([id, position]) => ({ id, position, movable: true })),
    nodes: [],
    edges,
    allowExistingMoves: true,
  };
  const standard: IntegralLayoutPlan = {
    positions,
    movedExisting: new Set(),
    quality: measureIntegralLayoutQuality(positions, edges),
  };
  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: 8,
    maxLayouts: 1,
    maxPolishTournaments: 0,
    maxExtensionStates: 2,
    maxMaskDiversifications: 4,
    maxCrossingWork: 0,
  });

  const report = repaired.constraintRepair;
  assert.ok(report);
  assert.ok(report.feasibilityChecks > 1);
  assert.equal(report.lowerBound, 1);
  assert.equal(report.constraintOptimal, true);
  assert.equal(report.cutoff, "extensions");
  assert.equal(report.separatorStates, 2);
});

test("unrestricted polish closes its derived mask before reporting no cutoff", () => {
  const trace: LayoutTraceEvent[] = [];
  const positions = new Map<string, GridPosition>([
    ["r0", at(1, -1)],
    ["r1", at(-2, -5)],
    ["r2", at(1, -5)],
    ["r3", at(-2, 5)],
    ["r4", at(0, 0)],
    ["r5", at(2, 0)],
    ["r6", at(1, -3)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "r0", to: "r1", direction: "East" },
    { from: "r1", to: "r0", direction: "West" },
    { from: "r1", to: "r2", direction: "North" },
    { from: "r0", to: "r3", direction: "East" },
    { from: "r3", to: "r0", direction: "West" },
    { from: "r1", to: "r4", direction: "North" },
    { from: "r4", to: "r1", direction: "South" },
    { from: "r2", to: "r5", direction: "South" },
    { from: "r5", to: "r2", direction: "North" },
    { from: "r4", to: "r6", direction: "East" },
    { from: "r4", to: "r0", direction: "East" },
    { from: "r0", to: "r5", direction: "North" },
    { from: "r5", to: "r6", direction: "North" },
  ];
  const request: IntegralLayoutRequest = {
    residents: [...positions].map(([id, position]) => ({ id, position, movable: true })),
    nodes: [],
    edges,
    allowExistingMoves: true,
  };
  const standard: IntegralLayoutPlan = {
    positions,
    movedExisting: new Set(),
    quality: measureIntegralLayoutQuality(positions, edges),
  };
  const repaired = repairIntegralLayoutConstraints(request, standard, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: 8,
    maxLayouts: Number.POSITIVE_INFINITY,
    maxPolishTournaments: 3,
    maxExtensionStates: 500,
    maxMaskDiversifications: 32,
    maxCrossingWork: 100,
  }, (event) => trace.push(event));

  const report = repaired.constraintRepair;
  assert.ok(report);
  assert.ok(
    report.cutoff !== "none" || report.extensionSearch.completed,
    "a no-cutoff report cannot leave the polish-derived mask unprocessed",
  );
  assert.ok(
    report.extensionSearch.completed || report.extensionSearch.cancelled ||
      report.extensionSearch.exhausted || report.cutoff !== "none",
  );
  assert.equal(
    report.layoutsConsidered,
    report.distinctLayouts,
    "an unbounded layout frontier polishes layouts discovered during mask closure",
  );
  const progressCounts = trace.flatMap((event) =>
    event.type === "constraint-progress" || event.type === "constraint-improvement"
      ? [event.layoutsConsidered]
      : []
  );
  for (let index = 1; index < progressCounts.length; index += 1) {
    assert.ok(
      progressCounts[index] >= progressCounts[index - 1],
      `operation-wide layout work regressed ${progressCounts[index - 1]} -> ${progressCounts[index]}`,
    );
  }
});

test("planar constraint repair never invents levels in bounded progressive or final candidates", () => {
  const positions = new Map<string, GridPosition>([
    ["r0", at(0, 2)],
    ["r1", at(0, 3)],
    ["r2", at(1, 0)],
    ["r3", at(1, 2)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "r2", to: "r1", direction: "East" },
    { from: "r0", to: "r1", direction: "East" },
    { from: "r1", to: "r2", direction: "North" },
  ];
  const request: IntegralLayoutRequest = {
    residents: [...positions].map(([id, position]) => ({ id, position, movable: true })),
    nodes: [],
    edges,
    allowExistingMoves: true,
  };
  const standard: IntegralLayoutPlan = {
    positions,
    movedExisting: new Set(),
    quality: measureIntegralLayoutQuality(positions, edges),
  };

  for (const maxExtensionStates of [60, 128]) {
    const trace: LayoutTraceEvent[] = [];
    const repaired = repairIntegralLayoutConstraints(request, standard, {
      when: "always",
      maxDurationMs: Number.POSITIVE_INFINITY,
      maxRestarts: 8,
      maxLayouts: 1,
      maxPolishTournaments: 0,
      maxExtensionStates,
      maxMaskDiversifications: 8,
      maxCrossingWork: 0,
    }, (event) => trace.push(event));
    const improvements = trace.filter(
      (event): event is Extract<LayoutTraceEvent, { type: "constraint-improvement" }> =>
        event.type === "constraint-improvement",
    );
    assert.ok(improvements.length > 0, `${maxExtensionStates}: expected progressive candidates`);
    for (const [index, improvement] of improvements.entries()) {
      assert.ok(
        improvement.candidate.positions?.every((position) => position.level === 0),
        `${maxExtensionStates}: progressive candidate ${index} left the only authoritative level`,
      );
    }
    assert.ok(
      [...repaired.positions.values()].every((position) => position.level === 0),
      `${maxExtensionStates}: final candidate left the only authoritative level`,
    );
  }
});

test("mixed-level planar-only repairs preserve every input level", () => {
  for (let example = 0; example < 12; example += 1) {
    const activeLevel = example % 5 - 2;
    const quietLevel = 7 - example % 3;
    const isolatedLevel = example - 9;
    const positions = new Map<string, GridPosition>([
      ["from", at(0, 0, activeLevel)],
      ["to", at(0, -3, activeLevel)],
      ["block-a", at(0, -1, activeLevel)],
      ["block-b", at(0, -2, activeLevel)],
      ["quiet-a", at(10, 0, quietLevel)],
      ["quiet-b", at(11, 0, quietLevel)],
      ["isolated", at(20, 20, isolatedLevel)],
    ]);
    const edges: LayoutEdge[] = [
      { from: "from", to: "to", direction: "North" },
      { from: "to", to: "from", direction: "South" },
      { from: "block-a", to: "block-b", direction: "North" },
      { from: "block-b", to: "block-a", direction: "South" },
      { from: "quiet-a", to: "quiet-b", direction: "East" },
    ];
    const request: IntegralLayoutRequest = {
      residents: [...positions].map(([id, position]) => ({ id, position, movable: true })),
      nodes: [],
      edges,
      allowExistingMoves: true,
    };
    const trace: LayoutTraceEvent[] = [];
    const repaired = repairIntegralLayoutConstraints(request, {
      positions,
      movedExisting: new Set(),
      quality: measureIntegralLayoutQuality(positions, edges),
    }, {
      when: "always",
      maxDurationMs: Number.POSITIVE_INFINITY,
      maxRestarts: 8,
      maxLayouts: 1,
      maxPolishTournaments: 0,
      maxExtensionStates: example % 2 === 0 ? 60 : 128,
      maxMaskDiversifications: 8,
      maxCrossingWork: 0,
    }, (event) => trace.push(event));
    const expected = new Map([...positions].map(([id, position]) => [id, position.level]));
    const assertInputLevels = (
      actual: Iterable<readonly [string, GridPosition]>,
      label: string,
    ): void => {
      for (const [id, position] of actual) {
        assert.equal(position.level, expected.get(id), `${example}: ${label} changed ${id}`);
      }
    };
    const improvements = trace.filter(
      (event): event is Extract<LayoutTraceEvent, { type: "constraint-improvement" }> =>
        event.type === "constraint-improvement",
    );
    assert.ok(improvements.length > 0, `${example}: expected a progressive candidate`);
    for (const [index, improvement] of improvements.entries()) {
      assertInputLevels(
        improvement.candidate.positions?.map(({ id, x, y, level }) =>
          [id, { x, y, level }] as const
        ) ?? [],
        `progressive candidate ${index}`,
      );
    }
    assertInputLevels(repaired.positions, "final candidate");
  }
});

test("an anomalous planar edge across input levels falls back without moving either floor", () => {
  const positions = new Map<string, GridPosition>([
    ["a", at(0, 0)],
    ["b", at(1, 0, 3)],
  ]);
  const edges: LayoutEdge[] = [{ from: "a", to: "b", direction: "East" }];
  const request: IntegralLayoutRequest = {
    residents: [...positions].map(([id, position]) => ({ id, position, movable: true })),
    nodes: [],
    edges,
    allowExistingMoves: true,
  };
  const repaired = repairIntegralLayoutConstraints(request, {
    positions,
    movedExisting: new Set(),
    quality: measureIntegralLayoutQuality(positions, edges),
  }, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: 8,
    maxLayouts: 1,
    maxPolishTournaments: 0,
    maxExtensionStates: 128,
    maxMaskDiversifications: 8,
    maxCrossingWork: 0,
  });

  assert.deepEqual(repaired.positions, positions);
});

test("constraint repair preserves legitimate Up/Down stacks and their planar components", () => {
  const positions = new Map<string, GridPosition>([
    ["lower", at(0, 0)],
    ["lower-east", at(1, 0)],
    ["upper", at(0, 0, 1)],
    ["upper-east", at(1, 0, 1)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "lower", to: "upper", direction: "Up" },
    { from: "upper", to: "lower", direction: "Down" },
    { from: "lower", to: "lower-east", direction: "East" },
    { from: "lower-east", to: "lower", direction: "West" },
    { from: "upper", to: "upper-east", direction: "East" },
    { from: "upper-east", to: "upper", direction: "West" },
  ];
  const request: IntegralLayoutRequest = {
    residents: [...positions].map(([id, position]) => ({ id, position, movable: true })),
    nodes: [],
    edges,
    allowExistingMoves: true,
  };
  const repaired = repairIntegralLayoutConstraints(request, {
    positions,
    movedExisting: new Set(),
    quality: measureIntegralLayoutQuality(positions, edges),
  }, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: 8,
    maxLayouts: 1,
    maxPolishTournaments: 0,
    maxExtensionStates: 64,
    maxMaskDiversifications: 8,
    maxCrossingWork: 0,
  });

  const lower = repaired.positions.get("lower") as GridPosition;
  const lowerEast = repaired.positions.get("lower-east") as GridPosition;
  const upper = repaired.positions.get("upper") as GridPosition;
  const upperEast = repaired.positions.get("upper-east") as GridPosition;
  assert.equal(upper.level, lower.level + 1);
  assert.equal(lowerEast.level, lower.level);
  assert.equal(upperEast.level, upper.level);
});

test("gravity compacts only strict raw improvements and safely falls back", () => {
  const { request, standard } = softSeparatorFixture();
  const options = {
    when: "always" as const,
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: 1,
    maxLayouts: 1,
    maxPolishTournaments: 0,
    maxExtensionStates: 2,
    maxMaskDiversifications: 1,
    maxCrossingWork: 0,
  };
  const run = (mode: "identity" | "invalid" | "throw") => {
    let calls = 0;
    const result = internals.repairWithGravity(request, standard, options, (
      _request,
      seed,
      control,
    ) => {
      calls += 1;
      assert.equal(control?.acceptsPositions?.(seed.positions), true);
      if (mode === "throw") throw new Error("synthetic gravity failure");
      if (mode === "invalid") {
        return {
          ...seed,
          positions: new Map([["from", at(0, 0)]]),
        };
      }
      return seed;
    });
    return { result, calls };
  };

  const identity = run("identity");
  assert.ok(identity.result.constraintRepair);
  assert.ok(identity.result.constraintRepair.rawIncumbents > identity.calls);
  assert.equal(identity.calls, identity.result.constraintRepair.softIncumbents);
  assert.ok(identity.calls > 0);

  for (const mode of ["invalid", "throw"] as const) {
    const fallback = run(mode);
    assert.equal(fallback.calls, identity.calls);
    assert.deepEqual(fallback.result.positions, identity.result.positions);
    assert.deepEqual(fallback.result.quality, identity.result.quality);
  }
});

test("soft defects enqueue deterministic equal-primary canonical mask swaps", () => {
  const positions = new Map<string, GridPosition>([
    ["a", at(-2, 0)],
    ["b", at(0, 0)],
    ["c", at(2, 0)],
    ["d", at(0, -2)],
    ["e", at(0, 1)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "a", to: "b", direction: "East" },
    { from: "b", to: "c", direction: "East" },
    { from: "c", to: "a", direction: "East" },
    { from: "d", to: "e", direction: "South" },
  ];
  const request: IntegralLayoutRequest = {
    residents: [...positions].map(([id, position]) => ({ id, position, movable: true })),
    nodes: [],
    edges,
    allowExistingMoves: true,
  };
  const standard: IntegralLayoutPlan = {
    positions,
    movedExisting: new Set(),
    quality: measureIntegralLayoutQuality(positions, edges),
  };
  const searched = internals.search(positions, edges, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: Number.POSITIVE_INFINITY,
    maxMaskDiversifications: 12,
  });
  assert.equal(searched.ok, true);
  assert.equal(searched.ok && searched.masks.length, 1, "master search has only one seed mask");

  const run = (maxMaskDiversifications = 12) => repairIntegralLayoutConstraints(request, standard, {
    when: "always" as const,
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: Number.POSITIVE_INFINITY,
    maxLayouts: 1,
    maxExtensionStates: 100,
    maxMaskDiversifications,
    maxCrossingWork: 0,
  });
  const first = run();
  const second = run();
  const capped = run(2);
  assert.ok((first.constraintRepair?.maskDiversifications ?? 0) > 1);
  assert.equal(capped.constraintRepair?.maskDiversifications, 2);
  assert.equal(capped.constraintRepair?.cutoff, "masks");
  assert.deepEqual(capped.constraintRepair?.maskDiversification, {
    completed: false,
    exhausted: true,
  });
  assert.equal(capped.constraintRepair?.geometricFixedPoint, false);
  assert.equal(
    first.constraintRepair?.maskDiversifications,
    first.constraintRepair?.compactionAttempts,
  );
  assert.equal(first.constraintRepair?.relaxedEdges, 1, "every queued swap keeps primary weight");
  assert.equal(first.constraintRepair?.reciprocalRelaxedEdges, 0);
  const deterministicStats = (plan: IntegralLayoutPlan) => {
    const report = plan.constraintRepair;
    return report && {
      rawIncumbents: report.rawIncumbents,
      softIncumbents: report.softIncumbents,
      distinctLayouts: report.distinctLayouts,
      maskDiversifications: report.maskDiversifications,
      separatorStates: report.separatorStates,
      separatorBranches: report.separatorBranches,
      separatorCyclePrunes: report.separatorCyclePrunes,
      compactionAttempts: report.compactionAttempts,
      relaxedEdges: report.relaxedEdges,
    };
  };
  assert.deepEqual(deterministicStats(first), deterministicStats(second));
  assert.deepEqual(first.quality, second.quality);
  assert.deepEqual([...first.positions], [...second.positions]);
});

test("re-encountered geometries are suppressed without duplicating published improvements", () => {
  const { request, standard } = softSeparatorFixture();
  const run = () => {
    const trace: LayoutTraceEvent[] = [];
    const plan = repairIntegralLayoutConstraints(request, standard, {
      when: "always",
      maxDurationMs: Number.POSITIVE_INFINITY,
      maxRestarts: Number.POSITIVE_INFINITY,
      maxLayouts: 8,
      maxMaskDiversifications: 12,
      maxCrossingWork: 0,
    }, (event) => trace.push(event));
    const improvements = trace
      .filter((event): event is Extract<LayoutTraceEvent, { type: "constraint-improvement" }> =>
        event.type === "constraint-improvement")
      .map((event) => JSON.stringify(event.candidate));
    return { plan, improvements };
  };
  const first = run();
  const second = run();
  const report = first.plan.constraintRepair;
  assert.ok(report);
  assert.ok(
    report.rawIncumbents > report.distinctLayouts,
    "at least one re-encountered geometry was suppressed by the signature window",
  );
  assert.ok(first.improvements.length > 0);
  assert.equal(
    new Set(first.improvements).size,
    first.improvements.length,
    "no improvement is published twice",
  );
  assert.deepEqual(second.improvements, first.improvements);
  const counters = (plan: IntegralLayoutPlan) => {
    const value = plan.constraintRepair;
    return value && {
      rawIncumbents: value.rawIncumbents,
      softIncumbents: value.softIncumbents,
      distinctLayouts: value.distinctLayouts,
      layoutsConsidered: value.layoutsConsidered,
    };
  };
  assert.deepEqual(counters(second.plan), counters(first.plan));
  assert.deepEqual(second.plan.quality, first.plan.quality);
  assert.deepEqual([...second.plan.positions], [...first.plan.positions]);
});

test("deadline and deterministic work cutoffs interrupt lower-bound work", () => {
  const { standard, request } = repairFixture();
  let tick = 0;
  const timed = internals.search(
    standard.positions,
    request.edges,
    { when: "always", maxDurationMs: 2, maxRestarts: 10 },
    { now: () => tick++ },
  );
  assert.equal(timed.ok, true);
  assert.equal(timed.ok && timed.cutoff, "time");
  assert.equal(timed.ok && timed.feasibilityChecks, 1);
  assert.equal(timed.ok && timed.optimal, false);

  const workBounded = internals.search(
    standard.positions,
    request.edges,
    { when: "always", maxDurationMs: 1_000, maxRestarts: 10 },
    { now: () => 0, maximumFeasibilityChecks: 1 },
  );
  assert.equal(workBounded.ok, true);
  assert.equal(workBounded.ok && workBounded.cutoff, "restarts");
  assert.equal(workBounded.ok && workBounded.feasibilityChecks, 1);
  assert.equal(workBounded.ok && workBounded.optimal, false);
});

test("constraint search streams its first feasible mask before lower-bound or restart work", () => {
  const { standard, request } = repairFixture();
  const streamed: { restarts: number; feasibilityChecks: number; elapsedMs: number }[] = [];
  const searched = internals.search(
    standard.positions,
    request.edges,
    {
      when: "always",
      maxDurationMs: Number.POSITIVE_INFINITY,
      maxRestarts: 250_000,
    },
    {
      mask: (_mask, progress) => {
        streamed.push(progress);
        return false;
      },
    },
  );

  assert.equal(searched.ok, true);
  assert.equal(streamed.length, 1);
  assert.equal(streamed[0].restarts, 0);
  assert.equal(streamed[0].feasibilityChecks, 1);
  assert.ok(streamed[0].elapsedMs >= 0);
  assert.equal(searched.ok && searched.restarts, 0);
  assert.equal(searched.ok && searched.feasibilityChecks, 1);
  assert.equal(searched.ok && searched.optimal, false);
});

test("constraint search publishes work progress about every thirty milliseconds", () => {
  const positions = new Map<string, GridPosition>();
  const edges: LayoutEdge[] = [];
  for (let cycle = 0; cycle < 20; cycle += 1) {
    for (let node = 0; node < 3; node += 1) {
      positions.set(`${cycle}-${node}`, at(0, cycle));
    }
    edges.push(
      { from: `${cycle}-0`, to: `${cycle}-1`, direction: "East" },
      { from: `${cycle}-1`, to: `${cycle}-2`, direction: "East" },
      { from: `${cycle}-2`, to: `${cycle}-0`, direction: "East" },
    );
  }
  let clock = 0;
  const progress: { feasibilityChecks: number; elapsedMs: number }[] = [];
  const searched = internals.search(
    positions,
    edges,
    { when: "always", maxDurationMs: Number.POSITIVE_INFINITY, maxRestarts: 1 },
    {
      now: () => clock++,
      progress: (event) => progress.push(event),
    },
  );

  assert.equal(searched.ok, true);
  assert.ok(progress.length >= 3);
  assert.equal(progress[0].feasibilityChecks, 32);
  assert.ok(progress[0].elapsedMs >= 30);
  assert.ok(progress[1].elapsedMs - progress[0].elapsedMs < 75);
});

const AXIS_CASES: readonly {
  positive: LayoutDirection;
  negative: LayoutDirection;
  position: (value: number) => GridPosition;
}[] = [
  { positive: "East", negative: "West", position: (value) => at(value, 0) },
  { positive: "South", negative: "North", position: (value) => at(0, value) },
  { positive: "Up", negative: "Down", position: (value) => at(0, 0, value) },
];
const PERMUTATIONS = [
  [0, 1, 2],
  [0, 2, 1],
  [1, 0, 2],
  [1, 2, 0],
  [2, 0, 1],
  [2, 1, 0],
] as const;

test("190 deterministic cases preserve the minimum reciprocal-aware objective", () => {
  for (let example = 0; example < 190; example += 1) {
    const axis = AXIS_CASES[example % AXIS_CASES.length];
    const order = PERMUTATIONS[(example * 5 + Math.floor(example / 7)) % PERMUTATIONS.length];
    const ids = [
      `a${String(example).padStart(3, "0")}`,
      `b${String(example).padStart(3, "0")}`,
      `c${String(example).padStart(3, "0")}`,
    ];
    const positions = new Map(ids.map((id, index) => [id, axis.position(order[index])]));
    const edges: LayoutEdge[] = [
      { from: ids[0], to: ids[1], direction: axis.positive },
      { from: ids[1], to: ids[0], direction: axis.negative },
      { from: ids[1], to: ids[2], direction: axis.positive },
      { from: ids[2], to: ids[0], direction: axis.positive },
    ];

    let bruteScore: readonly [number, number] | undefined;
    for (let mask = 0; mask < 1 << edges.length; mask += 1) {
      const removed: number[] = [];
      for (let edge = 0; edge < edges.length; edge += 1) {
        if (mask & (1 << edge)) removed.push(edge);
      }
      const analyzed = internals.analyze(positions, edges, removed);
      assert.equal(analyzed.ok, true);
      if (!analyzed.ok || !analyzed.feasible) continue;
      const score: readonly [number, number] = [
        removed.length,
        removed.filter((edge) => edge === 0 || edge === 1).length,
      ];
      if (!bruteScore || score[0] < bruteScore[0] ||
        (score[0] === bruteScore[0] && score[1] < bruteScore[1])) bruteScore = score;
    }

    const searched = internals.search(
      positions,
      edges,
      { when: "always", maxDurationMs: 1_000, maxRestarts: 64 },
    );
    assert.equal(searched.ok, true, `case ${example}`);
    assert.deepEqual(searched.ok && searched.score, bruteScore, `case ${example}`);
    assert.equal(searched.ok && searched.optimal, true, `case ${example}`);
    assert.equal(searched.ok && searched.score[1], 0, `case ${example}`);

    const repeated = internals.search(
      positions,
      edges,
      { when: "always", maxDurationMs: 1_000, maxRestarts: 64 },
    );
    assert.deepEqual(
      repeated.ok && repeated.removedSourceIndexes,
      searched.ok && searched.removedSourceIndexes,
      `case ${example}`,
    );
  }
});

test("grouped reciprocal constraints preserve exhaustive small-graph feasibility", () => {
  const ids = ["a", "b", "c", "d"];
  const positions = new Map(ids.map((id, index) => [id, at(index, index % 2, Math.floor(index / 2))]));
  const pairs = [["a", "b"], ["b", "c"], ["c", "d"], ["a", "c"], ["b", "d"]] as const;
  const directions: LayoutDirection[] = ["East", "West", "South", "North", "Up", "Down"];
  let state = 0x51ced123;
  const random = (): number => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    return (state >>> 0) / 0x1_0000_0000;
  };

  for (let example = 0; example < 48; example += 1) {
    const edges: LayoutEdge[] = [];
    for (let index = 0; index < 7; index += 1) {
      const pair = pairs[Math.floor(random() * pairs.length)];
      edges.push({
        from: pair[0],
        to: pair[1],
        direction: directions[Math.floor(random() * directions.length)],
      });
    }
    for (let mask = 0; mask < 1 << edges.length; mask += 1) {
      const removed = new Set<number>();
      for (let edge = 0; edge < edges.length; edge += 1) {
        if (mask & (1 << edge)) removed.add(edge);
      }
      const expected = referenceConstraintFeasibility(positions, edges, removed);
      const analyzed = internals.analyze(positions, edges, [...removed]);
      assert.equal(analyzed.ok, true, `case ${example}, mask ${mask}`);
      assert.equal(
        analyzed.ok && analyzed.feasible,
        expected,
        `case ${example}, mask ${mask}`,
      );
    }
  }
});

test("grouped source edges still expand to a feasible public removal set", () => {
  const ids = ["a", "b", "c", "d"];
  const positions = new Map(ids.map((id, index) => [id, at(index, index % 2, Math.floor(index / 2))]));
  const pairs = [["a", "b"], ["b", "c"], ["c", "d"], ["a", "c"]] as const;
  const directions = ["East", "West", "South", "North", "Up", "Down"] as const;
  const opposite: Record<(typeof directions)[number], (typeof directions)[number]> = {
    East: "West", West: "East", South: "North", North: "South", Up: "Down", Down: "Up",
  };

  for (let example = 0; example < 24; example += 1) {
    const edges: LayoutEdge[] = [];
    for (let index = 0; index < 6; index += 1) {
      const pair = pairs[(index * 3 + example) % pairs.length];
      edges.push({
        from: pair[0],
        to: pair[1],
        direction: directions[(index * 5 + example * 2) % directions.length],
      });
    }
    const reciprocal = edges.map((edge) => edges.some((other) =>
      other.from === edge.to && other.to === edge.from &&
      other.direction === opposite[edge.direction as keyof typeof opposite]
    ));
    let expected: readonly [number, number] | undefined;
    for (let mask = 0; mask < 1 << edges.length; mask += 1) {
      const removed = new Set<number>();
      for (let edge = 0; edge < edges.length; edge += 1) {
        if (mask & (1 << edge)) removed.add(edge);
      }
      if (!referenceConstraintFeasibility(positions, edges, removed)) continue;
      const score: readonly [number, number] = [
        removed.size,
        [...removed].filter((edge) => reciprocal[edge]).length,
      ];
      if (!expected || score[0] < expected[0] || (score[0] === expected[0] && score[1] < expected[1])) {
        expected = score;
      }
    }
    const searched = internals.search(
      positions,
      edges,
      { when: "always", maxDurationMs: 1_000, maxRestarts: 64 },
    );
    assert.equal(searched.ok, true, `case ${example}`);
    // The hitting-set master strategy certifies every one of these instances,
    // so the exhaustive objective is asserted unconditionally.
    assert.equal(searched.ok && searched.optimal, true, `case ${example} certified`);
    assert.ok(expected, `case ${example} has an exhaustive objective`);
    assert.deepEqual(searched.ok && searched.score, expected, `case ${example} exact objective`);
    const analyzed = searched.ok
      ? internals.analyze(positions, edges, searched.removedSourceIndexes)
      : undefined;
    assert.equal(analyzed?.ok && analyzed.feasible, true, `case ${example} feasible expansion`);
    assert.equal(
      searched.ok && searched.removedSourceIndexes.length,
      searched.ok && searched.score[0],
      `case ${example} public expansion`,
    );
  }
});

test("a reciprocal-positive optimum is certified instead of burning the restart budget", () => {
  // Contradictory reciprocal pairs: a<->b protected both East and West. Every
  // feasible mask removes one whole canonical group, and both groups carry
  // two reciprocal source edges, so the exhaustive optimum is [2, 2] — an
  // objective no zero-reciprocal certificate can ever reach.
  const positions = new Map<string, GridPosition>([
    ["a", at(0, 0)],
    ["b", at(1, 0)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "a", to: "b", direction: "East" },
    { from: "b", to: "a", direction: "West" },
    { from: "a", to: "b", direction: "West" },
    { from: "b", to: "a", direction: "East" },
  ];
  let expected: readonly [number, number] | undefined;
  for (let mask = 1; mask < 1 << edges.length; mask += 1) {
    const removed: number[] = [];
    for (let edge = 0; edge < edges.length; edge += 1) {
      if (mask & (1 << edge)) removed.push(edge);
    }
    const analyzed = internals.analyze(positions, edges, removed);
    assert.equal(analyzed.ok, true);
    if (!analyzed.ok || !analyzed.feasible) continue;
    const score: readonly [number, number] = [removed.length, removed.length];
    if (!expected || score[0] < expected[0]) expected = score;
  }
  assert.deepEqual(expected, [2, 2]);

  const searched = internals.search(
    positions,
    edges,
    { when: "always", maxDurationMs: Number.POSITIVE_INFINITY, maxRestarts: 4 },
  );
  assert.equal(searched.ok, true);
  assert.equal(searched.ok && searched.optimal, true);
  assert.deepEqual(searched.ok && searched.score, [2, 2]);
  assert.equal(searched.ok && searched.cutoff, "none");
  assert.equal(searched.ok && searched.restarts, 0, "no restart was needed for the proof");
  assert.equal(searched.ok && searched.lowerBound, 2);
});

test("an exhausted hitting-set budget falls back to restarts and reports honestly", () => {
  // One of the four-room instances whose optimum the disjoint-conflict bound
  // alone cannot certify. The exact solver certifies it; with the solver's
  // deterministic node budget forced to zero the search must degrade to the
  // seeded randomized restarts, keep a feasible (possibly weaker) result, and
  // never claim optimality it did not prove.
  const ids = ["a", "b", "c", "d"];
  const positions = new Map(ids.map((id, index) => [id, at(index, index % 2, Math.floor(index / 2))]));
  const pairs = [["a", "b"], ["b", "c"], ["c", "d"], ["a", "c"]] as const;
  const directions = ["East", "West", "South", "North", "Up", "Down"] as const;
  const edges: LayoutEdge[] = [];
  for (let index = 0; index < 6; index += 1) {
    const pair = pairs[(index * 3) % pairs.length];
    edges.push({
      from: pair[0],
      to: pair[1],
      direction: directions[(index * 5) % directions.length],
    });
  }
  const options = { when: "always" as const, maxDurationMs: 1_000, maxRestarts: 64 };
  const certified = internals.search(positions, edges, options);
  assert.equal(certified.ok, true);
  assert.equal(certified.ok && certified.optimal, true);
  assert.equal(certified.ok && certified.restarts, 0);

  const fallback = internals.search(positions, edges, options, { maximumHittingSetNodes: 0 });
  assert.equal(fallback.ok, true);
  assert.ok(fallback.ok && fallback.restarts > 0, "the restart fallback actually ran");
  if (certified.ok && fallback.ok) {
    assert.ok(
      fallback.score[0] > certified.score[0] ||
        (fallback.score[0] === certified.score[0] && fallback.score[1] >= certified.score[1]),
      "the fallback cannot beat the certified optimum",
    );
    if (!fallback.optimal) assert.equal(fallback.cutoff, "restarts");
    const expanded = internals.analyze(positions, edges, fallback.removedSourceIndexes);
    assert.equal(expanded.ok && expanded.feasible, true, "the fallback mask stays feasible");
  }
});

test("canonical relation groups preserve duplicate and reciprocal objective weights exactly", () => {
  const positions = new Map<string, GridPosition>([
    ["a", at(0, 0)],
    ["b", at(1, 0)],
    ["c", at(2, 0)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "a", to: "b", direction: "East" },
    { from: "a", to: "b", direction: "East" },
    { from: "b", to: "a", direction: "West" },
    { from: "b", to: "c", direction: "East" },
    { from: "c", to: "a", direction: "East" },
  ];

  assert.equal(internals.analyze(positions, edges).ok, true);
  assert.equal(internals.analyze(positions, edges).feasible, false);
  assert.equal(
    internals.analyze(positions, edges, [0]).feasible,
    false,
    "removing only part of a canonical duplicate group changes no relation",
  );
  assert.equal(internals.analyze(positions, edges, [0, 1, 2]).feasible, true);
  assert.equal(internals.analyze(positions, edges, [3]).feasible, true);
  assert.equal(internals.analyze(positions, edges, [4]).feasible, true);

  const searched = internals.search(positions, edges, {
    when: "always",
    maxDurationMs: Number.POSITIVE_INFINITY,
    maxRestarts: Number.POSITIVE_INFINITY,
    maxMaskDiversifications: Number.POSITIVE_INFINITY,
  });
  assert.equal(searched.ok, true);
  assert.equal(searched.ok && searched.optimal, true);
  assert.deepEqual(searched.ok && searched.score, [1, 0]);
  assert.deepEqual(searched.ok && searched.removedSourceIndexes, [4]);
  assert.equal(
    searched.ok && searched.removed.reduce((total, value) => total + value, 0),
    1,
  );
});

test("consecutive full repairs stay bit-identical across scratch-arena reuse", () => {
  // Feasibility analysis and coordinate construction draw on per-instance
  // scratch arenas. Three back-to-back repairs in one process must agree on
  // every position and every deterministic counter: any state leaking across
  // checks, states, or repair instances would surface here.
  const { request, standard } = softSeparatorFixture();
  const repair = () =>
    repairIntegralLayoutConstraints(request, standard, {
      when: "always",
      maxDurationMs: Number.POSITIVE_INFINITY,
      maxRestarts: 64,
      maxExtensionStates: 256,
      maxLayouts: 2,
      maxPolishTournaments: 1,
    });
  const stable = (plan: IntegralLayoutPlan) => ({
    positions: [...plan.positions].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)),
    movedExisting: [...plan.movedExisting].sort(),
    quality: plan.quality,
    report: plan.constraintRepair && {
      ...plan.constraintRepair,
      firstIncumbentMs: 0,
      searchMs: 0,
      compactionMs: 0,
      polishMs: 0,
      crossingRepair: { ...plan.constraintRepair.crossingRepair, elapsedMs: 0 },
    },
  });
  const first = stable(repair());
  assert.equal(first.report?.selected, true);
  assert.ok((first.report?.feasibilityChecks ?? 0) >= 1, "expected feasibility analysis to run");
  assert.ok((first.report?.separatorStates ?? 0) > 1, "expected repeated separator states");
  assert.deepEqual(stable(repair()), first);
  assert.deepEqual(stable(repair()), first);
});

test("fixed-anchor compaction incumbents repeat exactly under scratch reuse", () => {
  // The coordinate scratch carries fixed-shift and visitation flags across
  // separator states; an incomplete reset would corrupt a later state's
  // anchored component. Two identical runs must publish identical incumbent
  // sequences, work stats, and final geometry.
  const positions = new Map<string, GridPosition>([
    ["from", at(0, 0)],
    ["to", at(0, -3)],
    ["block-a", at(0, -1)],
    ["block-b", at(0, -2)],
  ]);
  const edges: LayoutEdge[] = [
    { from: "from", to: "to", direction: "North" },
    { from: "to", to: "from", direction: "South" },
    { from: "block-a", to: "block-b", direction: "North" },
    { from: "block-b", to: "block-a", direction: "South" },
  ];
  const run = () =>
    internals.compact(positions, edges, { fixedIds: ["from"], maximumStates: 64 });
  const first = run();
  assert.equal(first.ok, true);
  assert.equal(first.status.completed, true);
  assert.ok(first.workStats.separatorStates > 1, "expected a multi-state extension search");
  assert.ok(first.incumbents.length >= 1, "expected at least one published incumbent");
  assert.deepEqual(run(), first);
});
