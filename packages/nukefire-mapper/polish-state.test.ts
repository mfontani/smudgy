import assert from "node:assert/strict";
import test from "node:test";
import {
  AREA_POLISH_EXHAUSTED_FINGERPRINT_PROPERTY,
  AREA_POLISH_MEMO_SCHEMA_VERSION,
  AREA_POLISH_PENDING_PROPERTY,
  AREA_POLISH_PENDING_VALUE,
  AREA_POLISH_SEARCH_GENERATION,
  AreaPolishEntryTracker,
  areaPolishMemo,
  areaPolishMemoPropertyValue,
  areaPolishPending,
  createAreaPolishPlanningContext,
  equivalentSnapshotPayloads,
  MAX_AREA_POLISH_MEMO_CONTEXTS,
  MAX_FRUITLESS_QUIET_RESUMES,
  polishExhaustedFingerprint,
  polishRetrySuppressed,
  QuietPolishClaims,
  QuietResumeBudget,
  reduceAreaPolishMemo,
  reduceAreaPolishState,
  reportsCeilingExhaustion,
  type AreaPolishEvent,
  type AreaPolishMemo,
  type AreaPolishPlanningContext,
  type AreaPolishReport,
} from "./polish-state.ts";

test("the durable properties have package-owned representations", () => {
  assert.equal(AREA_POLISH_PENDING_PROPERTY, "nukefire.layout.polish-pending");
  assert.equal(AREA_POLISH_PENDING_VALUE, "true");
  assert.equal(
    AREA_POLISH_EXHAUSTED_FINGERPRINT_PROPERTY,
    "nukefire.layout.polish-exhausted-fingerprint",
  );
  assert.equal(areaPolishPending(undefined), false);
  assert.equal(areaPolishPending(""), false);
  assert.equal(areaPolishPending("pending"), false);
  assert.equal(areaPolishPending(" TRUE "), true);
  assert.equal(polishExhaustedFingerprint(undefined), undefined);
  assert.equal(polishExhaustedFingerprint(""), undefined);
  assert.equal(polishExhaustedFingerprint("  "), undefined);
  assert.equal(polishExhaustedFingerprint("[fp]"), "[fp]");
  assert.equal(AREA_POLISH_MEMO_SCHEMA_VERSION, 2);
  assert.equal(AREA_POLISH_SEARCH_GENERATION, 1);
  assert.equal(MAX_AREA_POLISH_MEMO_CONTEXTS, 32);
});

function planningContext(overrides: {
  geometryFingerprint?: string;
  centerId?: string;
  chartX?: number;
  edgeVectorX?: number;
  perfect?: boolean;
  maxLayouts?: number;
  maxPolishPasses?: number;
} = {}): AreaPolishPlanningContext {
  return createAreaPolishPlanningContext({
    geometryFingerprint: overrides.geometryFingerprint ?? "[geometry-1]",
    centerId: overrides.centerId ?? "room:1",
    nodes: [
      { id: "room:2", relative: { x: overrides.chartX ?? 1, y: 0, level: 0 } },
      { id: "room:1", relative: { x: 0, y: 0, level: 0 } },
    ],
    edges: [
      { from: "room:2", to: "room:1", direction: "West" },
      {
        from: "room:1",
        to: "room:2",
        direction: "East",
        ...(overrides.edgeVectorX === undefined
          ? {}
          : { constraintVector: { x: overrides.edgeVectorX, y: 0, level: 0 } }),
      },
    ],
    searchForPerfectLayouts: overrides.perfect ?? true,
    policy: {
      when: "always",
      maxDurationMs: overrides.perfect === false ? 10_000 : Number.POSITIVE_INFINITY,
      maxRestarts: overrides.perfect === false ? undefined : 32_768,
      maxLayouts: overrides.maxLayouts ?? 2,
      maxPolishTournaments: 2,
      maxPolishPasses: overrides.maxPolishPasses ?? 3,
      maxExtensionStates: 32_768,
      maxMaskDiversifications: 64,
      maxCrossingWork: 512,
    },
  });
}

function contextsMemo(
  context: Readonly<AreaPolishPlanningContext>,
  keys: readonly string[] = [context.key],
): AreaPolishMemo {
  return {
    kind: "contexts",
    geometryFingerprint: context.geometryFingerprint,
    contextKeys: keys,
  };
}

test("planning contexts canonicalize enumeration but include anchor, chart, and policy", () => {
  const base = planningContext();
  const reordered = createAreaPolishPlanningContext({
    geometryFingerprint: base.geometryFingerprint,
    centerId: "room:1",
    nodes: [
      { id: "room:1", relative: { x: 0, y: 0, level: 0 } },
      { id: "room:2", relative: { x: 1, y: 0, level: 0 } },
    ],
    edges: [
      { from: "room:1", to: "room:2", direction: "East" },
      { from: "room:2", to: "room:1", direction: "West" },
    ],
    searchForPerfectLayouts: true,
    policy: {
      when: "always",
      maxDurationMs: Number.POSITIVE_INFINITY,
      maxRestarts: 32_768,
      maxLayouts: 2,
      maxPolishTournaments: 2,
      maxPolishPasses: 3,
      maxExtensionStates: 32_768,
      maxMaskDiversifications: 64,
      maxCrossingWork: 512,
    },
  });
  assert.deepEqual(reordered, base);
  assert.match(base.key, /^[0-9a-f]{32}$/);
  assert.notEqual(planningContext({ centerId: "room:2" }).key, base.key);
  assert.notEqual(planningContext({ chartX: 2 }).key, base.key);
  assert.notEqual(planningContext({ edgeVectorX: 1 }).key, base.key);
  assert.notEqual(planningContext({ perfect: false }).key, base.key);
  assert.notEqual(planningContext({ maxLayouts: 3 }).key, base.key);
  assert.notEqual(planningContext({ maxPolishPasses: 4 }).key, base.key);
  assert.notEqual(
    planningContext({ geometryFingerprint: "[geometry-2]" }).geometryFingerprint,
    base.geometryFingerprint,
  );
});

test("structured memos round-trip while legacy and malformed values remain inert", () => {
  const context = planningContext();
  const memo = contextsMemo(context);
  const property = areaPolishMemoPropertyValue(memo);
  assert.deepEqual(areaPolishMemo(property), memo);
  assert.equal(polishRetrySuppressed(areaPolishMemo(property), context), true);

  const legacy = areaPolishMemo("[legacy-geometry]");
  assert.deepEqual(legacy, { kind: "legacy", propertyValue: "[legacy-geometry]" });
  assert.equal(polishRetrySuppressed(legacy, context), false);
  assert.deepEqual(areaPolishMemo("{broken"), {
    kind: "legacy",
    propertyValue: "{broken",
  });
  const malformedV2 = JSON.stringify({ v: 2, g: "[geometry-1]", c: ["unbounded"] });
  assert.deepEqual(areaPolishMemo(malformedV2), {
    kind: "legacy",
    propertyValue: malformedV2,
  });
  assert.equal(areaPolishMemo(""), undefined);
});

test("memo parsing deduplicates and bounds valid persisted context keys", () => {
  const keys = Array.from(
    { length: MAX_AREA_POLISH_MEMO_CONTEXTS + 8 },
    (_, index) => index.toString(16).padStart(32, "0"),
  );
  assert.deepEqual(
    areaPolishMemo(JSON.stringify({
      v: AREA_POLISH_MEMO_SCHEMA_VERSION,
      g: "[geometry-1]",
      c: [...keys, keys.at(-1)],
    })),
    {
      kind: "contexts",
      geometryFingerprint: "[geometry-1]",
      contextKeys: keys.slice(8),
    },
  );
});

test("deferred topology marks an area pending without redundant writes", () => {
  assert.deepEqual(reduceAreaPolishState(false, { kind: "topology-deferred" }), {
    pending: true,
    propertyValue: "true",
  });
  assert.deepEqual(reduceAreaPolishState(true, { kind: "topology-deferred" }), {
    pending: true,
    propertyValue: undefined,
  });
});

test("starting polish preserves a durable retry", () => {
  const event: AreaPolishEvent = { kind: "polish-started" };
  assert.deepEqual(reduceAreaPolishState(false, event), {
    pending: true,
    propertyValue: "true",
  });
  assert.deepEqual(reduceAreaPolishState(true, event), {
    pending: true,
    propertyValue: undefined,
  });
});

test("a context-relative geometric fixed point retains area-wide eligibility", () => {
  assert.deepEqual(reduceAreaPolishState(true, {
    kind: "polish-completed",
    report: { geometricFixedPoint: true },
  }), {
    pending: true,
    propertyValue: undefined,
  });
  assert.deepEqual(reduceAreaPolishState(false, {
    kind: "polish-completed",
    report: { geometricFixedPoint: true },
  }), {
    pending: true,
    propertyValue: "true",
  });
});

test("bounded and report-less results remain eligible", () => {
  assert.deepEqual(reduceAreaPolishState(false, {
    kind: "polish-completed",
    report: { geometricFixedPoint: false },
  }), {
    pending: true,
    propertyValue: "true",
  });
  assert.deepEqual(reduceAreaPolishState(true, { kind: "polish-completed" }), {
    pending: true,
    propertyValue: undefined,
  });
});

/** A report whose every stop was a deterministic ceiling, not a deadline. */
function exhaustedReport(overrides: Partial<AreaPolishReport> = {}): AreaPolishReport {
  return {
    geometricFixedPoint: false,
    cutoff: "extensions",
    polishCutoff: "tournaments",
    extensionSearch: { cancelled: false, exhausted: true },
    crossingRepair: { cancelled: false, exhausted: false },
    ...overrides,
  };
}

test("only deterministic ceiling exhaustion qualifies for the memo", () => {
  assert.equal(reportsCeilingExhaustion(undefined), false);
  assert.equal(reportsCeilingExhaustion(exhaustedReport()), true);
  assert.equal(
    reportsCeilingExhaustion(exhaustedReport({
      cutoff: "none",
      polishCutoff: "passes",
      extensionSearch: { cancelled: false, exhausted: false },
    })),
    true,
  );
  assert.equal(
    reportsCeilingExhaustion(exhaustedReport({
      cutoff: "none",
      polishCutoff: "fixed-point",
      extensionSearch: { cancelled: false, exhausted: false },
    })),
    false,
  );
  assert.equal(
    reportsCeilingExhaustion(exhaustedReport({ geometricFixedPoint: true })),
    false,
  );
  assert.equal(reportsCeilingExhaustion(exhaustedReport({ cutoff: "time" })), false);
  assert.equal(
    reportsCeilingExhaustion(exhaustedReport({ polishCutoff: "time" })),
    false,
  );
  assert.equal(
    reportsCeilingExhaustion(exhaustedReport({ polishCutoff: "error" })),
    false,
  );
  assert.equal(
    reportsCeilingExhaustion(exhaustedReport({ extensionSearch: { cancelled: true } })),
    false,
  );
  assert.equal(
    reportsCeilingExhaustion(exhaustedReport({ crossingRepair: { cancelled: true } })),
    false,
  );
});

test("a fruitless deterministic ceiling records its exact context durably", () => {
  const context = planningContext();
  const expectedMemo = contextsMemo(context);
  const first = reduceAreaPolishMemo(undefined, {
    kind: "polish-completed",
    report: exhaustedReport(),
    context,
  });
  assert.deepEqual(first, {
    memo: expectedMemo,
    propertyValue: areaPolishMemoPropertyValue(expectedMemo),
  });
  // Re-recording an identical context needs no durable write.
  assert.deepEqual(
    reduceAreaPolishMemo(first.memo, {
      kind: "polish-completed",
      report: exhaustedReport(),
      context,
    }),
    { memo: expectedMemo, propertyValue: undefined },
  );

  // A different resident geometry starts a fresh bounded context set.
  const changed = planningContext({ geometryFingerprint: "[geometry-2]" });
  const changedMemo = contextsMemo(changed);
  assert.deepEqual(
    reduceAreaPolishMemo(first.memo, {
      kind: "polish-completed",
      report: exhaustedReport(),
      context: changed,
    }),
    {
      memo: changedMemo,
      propertyValue: areaPolishMemoPropertyValue(changedMemo),
    },
  );
  // Without the exact context there is nothing safe to memoize.
  assert.deepEqual(
    reduceAreaPolishMemo(undefined, {
      kind: "polish-completed",
      report: exhaustedReport(),
    }),
    { memo: undefined, propertyValue: undefined },
  );
});

test("fruitless fixed points accumulate by context without retiring the area", () => {
  const first = planningContext({ centerId: "room:1" });
  const second = planningContext({ centerId: "room:2" });
  const firstTransition = reduceAreaPolishMemo(undefined, {
    kind: "polish-completed",
    report: { geometricFixedPoint: true },
    context: first,
  });
  const secondTransition = reduceAreaPolishMemo(firstTransition.memo, {
    kind: "polish-completed",
    report: { geometricFixedPoint: true },
    context: second,
  });
  assert.ok(secondTransition.memo?.kind === "contexts");
  assert.deepEqual(secondTransition.memo.contextKeys, [first.key, second.key]);
  assert.equal(polishRetrySuppressed(secondTransition.memo, first), true);
  assert.equal(polishRetrySuppressed(secondTransition.memo, second), true);
  assert.equal(polishRetrySuppressed(secondTransition.memo, planningContext({ chartX: 2 })), false);
  assert.equal(
    polishRetrySuppressed(
      secondTransition.memo,
      planningContext({ geometryFingerprint: "[geometry-2]", centerId: "room:1" }),
    ),
    false,
  );
});

test("a fruitful incomplete pass clears prior contexts and keeps the new geometry eligible", () => {
  const oldContext = planningContext();
  const oldMemo = contextsMemo(oldContext);
  assert.deepEqual(
    reduceAreaPolishMemo(oldMemo, {
      kind: "polish-completed",
      report: exhaustedReport(),
      context: planningContext({ geometryFingerprint: "[new]" }),
      improved: true,
    }),
    { memo: undefined, propertyValue: "" },
  );
  assert.deepEqual(reduceAreaPolishState(true, {
    kind: "polish-completed",
    report: exhaustedReport(),
    improved: true,
  }), {
    pending: true,
    propertyValue: undefined,
  });
});

test("a fruitful fixed point replaces old contexts with the proven final geometry", () => {
  const oldContext = planningContext();
  const finalContext = planningContext({ geometryFingerprint: "[new]" });
  const transition = reduceAreaPolishMemo(contextsMemo(oldContext), {
    kind: "polish-completed",
    report: { geometricFixedPoint: true },
    context: finalContext,
    improved: true,
  });
  assert.deepEqual(transition.memo, {
    kind: "contexts",
    geometryFingerprint: "[new]",
    contextKeys: [finalContext.key],
  });
  assert.equal(polishRetrySuppressed(transition.memo, oldContext), false);
  assert.equal(polishRetrySuppressed(transition.memo, finalContext), true);
});

test("a fruitful fixed point without its rebased context still clears stale contexts", () => {
  const oldContext = planningContext();
  assert.deepEqual(reduceAreaPolishMemo(contextsMemo(oldContext), {
    kind: "polish-completed",
    report: { geometricFixedPoint: true },
    improved: true,
  }), {
    memo: undefined,
    propertyValue: "",
  });
});

test("the context memo evicts oldest entries deterministically at its bound", () => {
  const geometryFingerprint = JSON.stringify(Array.from({ length: 97 }, (_, index) => ({
    id: `room:${index}`,
    position: { x: index - 48, y: index % 11, level: 0 },
  })));
  const contexts = Array.from(
    { length: MAX_AREA_POLISH_MEMO_CONTEXTS + 2 },
    (_, index) => planningContext({ geometryFingerprint, centerId: `room:${index}` }),
  );
  let memo: AreaPolishMemo | undefined;
  for (const context of contexts) {
    memo = reduceAreaPolishMemo(memo, {
      kind: "polish-completed",
      report: { geometricFixedPoint: true },
      context,
    }).memo;
  }
  assert.ok(memo?.kind === "contexts");
  assert.equal(memo.contextKeys.length, MAX_AREA_POLISH_MEMO_CONTEXTS);
  assert.deepEqual(memo.contextKeys, contexts.slice(2).map((context) => context.key));
  assert.ok(
    areaPolishMemoPropertyValue(memo).length < memo.geometryFingerprint.length * 2 + 1_300,
    "the chart must be represented by compact keys, not repeated canonical JSON",
  );
  assert.equal(polishRetrySuppressed(memo, contexts[0]), false);
  assert.equal(polishRetrySuppressed(memo, contexts.at(-1) as AreaPolishPlanningContext), true);
});

test("fresh growth clears contexts; incomplete, cancelled, and legacy observations do not add any", () => {
  const context = planningContext();
  const memo = contextsMemo(context);
  assert.deepEqual(reduceAreaPolishMemo(memo, { kind: "topology-deferred" }), {
    memo: undefined,
    propertyValue: "",
  });
  assert.deepEqual(reduceAreaPolishMemo(undefined, { kind: "topology-deferred" }), {
    memo: undefined,
    propertyValue: undefined,
  });
  assert.deepEqual(reduceAreaPolishMemo(memo, { kind: "polish-started" }), {
    memo,
    propertyValue: undefined,
  });
  for (const report of [
    exhaustedReport({ cutoff: "time" }),
    exhaustedReport({ polishCutoff: "error" }),
    exhaustedReport({ extensionSearch: { cancelled: true } }),
    exhaustedReport({
      cutoff: "none",
      polishCutoff: "fixed-point",
      extensionSearch: { cancelled: false, exhausted: false },
    }),
  ]) {
    assert.deepEqual(
      reduceAreaPolishMemo(memo, { kind: "polish-completed", report, context }),
      { memo, propertyValue: undefined },
    );
  }

  const legacy = areaPolishMemo("[legacy-geometry]");
  assert.ok(legacy);
  assert.equal(polishRetrySuppressed(legacy, context), false);
  assert.deepEqual(reduceAreaPolishMemo(legacy, { kind: "polish-started" }), {
    memo: legacy,
    propertyValue: undefined,
  });
  const upgraded = reduceAreaPolishMemo(legacy, {
    kind: "polish-completed",
    report: { geometricFixedPoint: true },
    context,
  });
  assert.deepEqual(upgraded.memo, contextsMemo(context));
  assert.equal(upgraded.propertyValue, areaPolishMemoPropertyValue(contextsMemo(context)));
});

test("entry tracking retries once per area visit", () => {
  const tracker = new AreaPolishEntryTracker();

  assert.equal(tracker.currentAreaKey, undefined);
  assert.deepEqual(tracker.observe("area-a", true), {
    entered: true,
    retry: true,
    previousAreaKey: undefined,
  });
  assert.equal(tracker.currentAreaKey, "area-a");
  assert.equal(tracker.retryAreaKey, "area-a");
  assert.equal(tracker.consumeRetry("area-a"), true);
  assert.equal(tracker.retryAreaKey, undefined);
  assert.equal(tracker.consumeRetry("area-a"), false);
  assert.deepEqual(tracker.observe("area-a", true), {
    entered: false,
    retry: false,
    previousAreaKey: undefined,
  });
  assert.deepEqual(tracker.observe("area-a", false), {
    entered: false,
    retry: false,
    previousAreaKey: undefined,
  });
  tracker.markPending("area-a");
  assert.equal(tracker.retryAreaKey, "area-a");
  tracker.markPending("area-b");
  assert.equal(tracker.retryAreaKey, "area-a");
  assert.deepEqual(tracker.observe("area-b", false), {
    entered: true,
    retry: false,
    previousAreaKey: "area-a",
  });
  assert.equal(tracker.retryAreaKey, undefined);
  assert.deepEqual(tracker.observe("area-a", true), {
    entered: true,
    retry: true,
    previousAreaKey: "area-b",
  });
});

test("snapshot payload equivalence is identity or byte-equal clones", () => {
  const snapshot = { center: 5, rooms: [{ vnum: 5, x: 0 }] };
  assert.equal(equivalentSnapshotPayloads(snapshot, snapshot), true);
  assert.equal(
    equivalentSnapshotPayloads(snapshot, JSON.parse(JSON.stringify(snapshot))),
    true,
  );
  assert.equal(
    equivalentSnapshotPayloads(snapshot, { center: 6, rooms: [{ vnum: 5, x: 0 }] }),
    false,
  );
  assert.equal(
    equivalentSnapshotPayloads(snapshot, { center: 5, rooms: [{ vnum: 5, x: 1 }] }),
    false,
  );
});

test("claims settle to a restoration only for an equivalent displacement", () => {
  const claims = new QuietPolishClaims<{ center: number }>();
  const planned = { center: 5 };

  claims.record(planned, "area-1", { retryConsumed: true, deferredRemoved: false });
  claims.record(planned, "area-1", { retryConsumed: false, deferredRemoved: true });
  assert.deepEqual([...claims.settle(planned, { center: 5 })], [
    ["area-1", { retryConsumed: true, deferredRemoved: true }],
  ]);
  // Settling clears the claim even when it was restored.
  assert.equal(claims.settle(planned, { center: 5 }).size, 0);

  claims.record(planned, "area-1", { retryConsumed: true, deferredRemoved: true });
  assert.equal(claims.settle(planned, { center: 6 }).size, 0);
  // A genuine displacement forfeits: the cleared claim cannot restore later.
  assert.equal(claims.settle(planned, { center: 5 }).size, 0);
});

test("nothing-consumed passes and completed polishes leave nothing to restore", () => {
  const claims = new QuietPolishClaims<{ center: number }>();
  const planned = { center: 5 };

  claims.record(planned, "area-1", { retryConsumed: false, deferredRemoved: false });
  assert.equal(claims.settle(planned, { center: 5 }).size, 0);

  claims.record(planned, "area-1", { retryConsumed: true, deferredRemoved: true });
  claims.discharge(planned, "area-1");
  assert.equal(claims.settle(planned, { center: 5 }).size, 0);
});

test("committed progress marks the claim and survives a merged re-record", () => {
  const claims = new QuietPolishClaims<{ center: number }>();
  const planned = { center: 5 };

  // Marking without a recorded claim is inert: nothing exists to restore.
  claims.markProgress(planned, "area-1");
  assert.equal(claims.settle(planned, { center: 5 }).size, 0);

  claims.record(planned, "area-1", { retryConsumed: true, deferredRemoved: false });
  claims.markProgress(planned, "area-1");
  claims.record(planned, "area-1", { retryConsumed: false, deferredRemoved: true });
  assert.deepEqual([...claims.settle(planned, { center: 5 })], [
    ["area-1", { retryConsumed: true, deferredRemoved: true, progressed: true }],
  ]);
});

test("the resume budget allows a bounded run of fruitless resumptions", () => {
  const budget = new QuietResumeBudget();
  for (let resume = 1; resume <= MAX_FRUITLESS_QUIET_RESUMES; resume += 1) {
    assert.equal(budget.allowResume("area-1", false), true, `resumption ${resume}`);
  }
  assert.equal(budget.allowResume("area-1", false), false);
  // Areas are budgeted independently.
  assert.equal(budget.allowResume("area-2", false), true);
});

test("progress, reset, and clear each restart the fruitless allowance", () => {
  const budget = new QuietResumeBudget(2);
  assert.equal(budget.allowResume("area-1", false), true);
  assert.equal(budget.allowResume("area-1", false), true);
  assert.equal(budget.allowResume("area-1", false), false);

  // A fruitful displacement is always resumable and restarts the count.
  assert.equal(budget.allowResume("area-1", true), true);
  assert.equal(budget.allowResume("area-1", false), true);

  budget.reset("area-1");
  assert.equal(budget.allowResume("area-1", false), true);

  assert.equal(budget.allowResume("area-1", false), true);
  assert.equal(budget.allowResume("area-1", false), false);
  budget.clear();
  assert.equal(budget.allowResume("area-1", false), true);
});

test("disabled polishing observes entry without scheduling work", () => {
  const tracker = new AreaPolishEntryTracker();

  assert.deepEqual(tracker.observe("area-a", true, false), {
    entered: true,
    retry: false,
    previousAreaKey: undefined,
  });
  assert.deepEqual(tracker.observe("area-a", true), {
    entered: false,
    retry: false,
    previousAreaKey: undefined,
  });
  tracker.clear();
  assert.equal(tracker.currentAreaKey, undefined);
  assert.equal(tracker.retryAreaKey, undefined);
  assert.deepEqual(tracker.observe("area-a", true), {
    entered: true,
    retry: true,
    previousAreaKey: undefined,
  });
});
