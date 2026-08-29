# Map Layout

Reusable integral-grid layout and reflow for Smudgy mappers.

The planner protects cardinal/elevation rays first, then avoids route
violations and crossings, then minimizes link slack and footprint. Route
violations include both rooms lying on a direct connection and occupied cells
which prevent an exit from leaving through its declared cardinal wall. It
never writes to the mapper; every operation returns a declarative patch.

## Stateless area planning

`planAreaChange` is the normal API for mappers which only reflow when topology
grows. It snapshots the Smudgy area into ordinary V8 data immediately before
planning and does not retain that model afterward. Ordinary CPU-intensive
planning runs through one lazily created FIFO Worker shared by this package
instance, so concurrent callers cannot accidentally stack compute-heavy jobs.

```ts
const result = await planAreaChange(areaId, {
  type: "add-room",
  from: currentRoomNumber,
  direction: "North",
  temporaryId: "$new",
});

await mapper.updateRooms(areaId, result.patch.moves.map((move) => [
  move.roomNumber!,
  move.to,
]));
```

An entire existing area can be reflowed without adding observations:

```ts
const result = await planAreaChange(areaId, {
  type: "reflow",
  anchor: currentRoomNumber,
});
```

Manual tools can opt into a bounded thorough search. It repeats each candidate
to a fixed point and compares the requested anchor, an unrestricted reflow,
rooms incident to remaining directional violations, their immediate neighbors,
and high-degree structural rooms. The winning layout is returned as one patch;
locked rooms remain fixed.

```ts
const result = await planAreaChange(areaId, {
  type: "reflow",
  anchor: currentRoomNumber,
}, {
  effort: "thorough",
});
```

`result.search` describes the anchors and planning passes considered. Thorough
search is opt-in so latency-sensitive automatic mapping retains the standard
single-pass behavior.

Because Worker planning introduces a real asynchronous gap, `planAreaChange`
reloads the area before returning. It discards one stale result and retries
once; a second concurrent change rejects with `StaleLayoutSnapshotError`
instead of returning a patch for obsolete coordinates or topology. Room
movability callbacks always run while snapshotting in the caller realm. Trace
events are collected in the Worker and replayed in order after the accepted
result, so callbacks and other non-cloneable values never cross the boundary.

Two existing rooms can be connected while planning the reflow required by the
new topology:

```ts
const result = await planAreaChange(areaId, {
  type: "connect-rooms",
  from: currentRoomNumber,
  to: matchingRoomNumber,
  direction: "East",
});
```

## Retained models

High-frequency consumers may explicitly retain a model instead:

```ts
const workspace = createLayoutWorkspace(loadLayoutModel(areaId));
const result = await workspace.planAsync(change);
await apply(result.patch);
workspace.accept(result);
```

`planLayoutModelAsync` and `planIntegralLayoutAsync` use the same shared Worker
for host-independent models and low-level requests. Their synchronous
counterparts remain available for deterministic tests, decision-log replay,
and explicitly synchronous tools. A retained workspace also keeps its original
`plan` method; an async result cannot be accepted if another result changed the
workspace while its Worker request was in flight.

Async calls accept an optional `AbortSignal` and parent-side `timeoutMs`.
Canceling queued work does not disturb the active job. Canceling active work
terminates that Worker, rejects only that request, and resumes queued work on a
fresh Worker.

## Live progress and deep repair

`layoutPlannerState` is a read-only, subscribable state object for the latest
operation in the current package realm. Its JSON-safe snapshot reports the
phase, elapsed time, input sizes, layouts considered, randomized restarts,
feasibility checks, hard-valid incumbents, distinct layouts and relation masks,
separator states/branches/cycle prunes, crossing work, and the
current/standard/best quality tuples. `firstIncumbentMs` records when geometric
repair first produced a complete hard-valid map.
Subscribers receive the retained value immediately and every subsequent
update.

`planIntegralLayoutAsync` also accepts `onProgress`. The callback receives the
same snapshot and includes an `improvement` plan whenever a new complete
best-so-far layout is found. Progress sinks are isolated from correctness: a
throwing observer cannot fail planning. Live streaming is demand-driven: an
integral job streams per-event telemetry only while a trace sink or
`onProgress` observer is attached, and unobserved jobs build and post no
events at all. Constraint-repair improvements always stream — they are how
anytime results flow.

Whole-layout constraint repair supports separate `maxDurationMs`,
`maxRestarts`, `maxLayouts`, `maxPolishTournaments`, `maxPolishPasses`,
`maxExtensionStates`, `maxMaskDiversifications`, and `maxCrossingWork` controls.
Polish passes are one aggregate deterministic budget shared by the early preview
and all later multi-anchor tournaments; a pass ceiling can stop partway through
a tournament and reports `polishCutoff: "passes"` without claiming a fixed point.
Extension work counts
every deterministic precedence state built across canonical relation masks;
crossing work counts bridge-push macro expansions. Each work limit accepts
`Infinity`, continuing until that frontier exhausts, a perfect map is found, or
the request is cancelled. An infinite duration removes the wall-clock cutoff
while preserving `AbortSignal` cancellation. Complete-layout attempts use a
serialized dedicated-Worker lane, publish every complete strict improvement,
retain only a bounded polish frontier plus compact search bookkeeping, and cap
retained diagnostic coordinates so long searches do not grow an unbounded
trace result.

Low-level automatic mappers can opt into a whole-layout constraint repair for
settled reflows. The Worker first runs the ordinary planner, then immediately
runs exactly one complete reflow pass anchored at the request center when present so a
strong complete incumbent can publish without letting a multi-anchor tournament
starve exact certification. It next certifies the
minimum-weight set of protected exits to relax with an exact
implicit-hitting-set search over the conflicts feasibility analysis reports.
`optimal: true` is the normal outcome, including optima that relax reciprocal
exits; seeded randomized restarts survive only as a deterministic fallback for
pathological conflict accumulation and report honestly when they run. For each
mask, deterministic complete-extension search branches over legal room and
route separations with cycle pruning, traversing best-first on incumbent
scores so finite state budgets reach better incumbents earlier. Per-state
coordinates minimize retained-relation slack on each axis after longest-path
ranks, so every candidate state is scored at its tightest geometry. Retained
rays, fixed rooms, integral coordinates, and collision freedom are hard
requirements. The extension frontier also has an independent live-node cap;
pruning a subtree reports exhaustion rather than an exact proof, so a generous
total-state budget cannot imply unbounded retained memory. Route obstructions
and crossings are soft quality terms. A collision-free soft incumbent is
freshly scored and can be published immediately instead of being discarded
when further separation is impossible. Geometry-guided equal-primary mask
swaps, bounded distinct-layout planning, multi-anchor fixed-point polish using
the remaining aggregate pass budget, and crossing repair then continue from the
best incumbent without losing it. Master masks are
compacted best-first, and any relaxation mask discovered by final unrestricted
polish is compacted and its newly retained layouts are polished in the same
bounded operation before a geometric fixed point can be reported.

Complete settled layouts run a compaction fixed point before publication. It
removes globally empty rows and columns, then recursively packs mutually
blocking room groups along both planar axes from every directional
orientation, repeating while a probe shows more work. A final aesthetic pass
redistributes unavoidable slack evenly
along maximal straight cardinal series, prioritizing reciprocal connections.
The spacing pass moves attached axis groups atomically and never increases any
public quality field, leaves the original envelope, moves a fixed/player room,
or violates a caller-supplied position constraint.

```ts
const result = await planIntegralLayoutAsync(request, {
  constraintRepair: {
    when: "settled-regression",
    maxDurationMs: 10_000,
  },
});
```

`when: "violation-regression"` also considers newly observed topology and is
appropriate for a mapper's final settled snapshot. `when: "always"` is useful
for explicit whole-area reflows and still runs geometric polish when its input
already has zero violations. `result.constraintRepair` reports constraint
optimality separately from `geometricFixedPoint`, along with constraint and
polish cutoff reasons, first-incumbent latency, geometric work counts, and stage
timings. Constraint optimality describes the weighted relation master; a
finite geometric cutoff remains an anytime result, not a proof that no better
complete extension exists. The duration is a
cooperative search ceiling: one complete polish tournament may finish past a
finite deadline. An infinite duration continues tournaments until geometric
fixed point, the optional deterministic tournament ceiling, or caller
cancellation. Locked layouts and custom multi-axis constraint vectors retain
the ordinary planner.

Constraint repair runs on a second, dedicated persistent Worker after the
ordinary result is available. It therefore cannot hold up the ordinary shared
FIFO queue, while a second FIFO ensures that concurrent callers never stack
CPU-heavy repair Workers. The repair Worker stays warm across successful
repairs, so sequential repairs pay isolate spawn and module compile once. It
is reclaimed — terminated, with the next repair starting a fresh Worker —
whenever active work is abandoned or the transport misbehaves: the hard
deadline's backstop, an abort or caller deadline during active repair, a
startup or postMessage failure, or malformed traffic. Every degraded outcome
still returns the retained ordinary result (or the best validated streamed
improvement); a hard deadline still kills a hung repair.

When a defect is permanent — every room a fix would need to move is immovable
— the planner cannot improve the geometry, so it proposes a `routeAmendments`
entry instead: per-link elbow waypoints that draw the link around the
obstruction or crossing, leaving and entering through the declared walls.
Amendments are advisory. They never move rooms, they never change the quality
tuple (which keeps scoring the straight segments honestly), and consumers
that ignore them behave exactly as before.

Quality still minimizes all directed protected-ray violations first. Among
equal totals, `reciprocalRayViolations` explicitly prefers preserving exact
two-way connections. `routingViolations` then combines direct-link room
obstructions with blocked exit ports; `exitPortViolations` and
`reciprocalExitPortViolations` break ties in favor of routes which leave both
rooms through their declared walls before crossings, slack, and footprint.

## Elevation

Existing U/D links on different levels remain vertical. Same-level U/D links
are projected diagonally; their semantic directions remain Up/Down while each
physical connection receives a NE/NW or SE/SW constraint selected from local
path continuity. A new link may request `auto`, `levels`, or `projected`.
