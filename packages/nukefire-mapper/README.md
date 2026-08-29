# NukeFire Mapper

`smudgy://kapusniak/nukefire-mapper` reads NukeFire's GMCP data and
automatically builds the map shown in Smudgy's map widget.

As you explore, it adds rooms, exits, terrain, doors, and vertical connections.
A room you reach by going up or down is always mapped one level above or below
the room you left, even when the game charts both on the same plane. It also
follows your current location and displays GPS routes. New maps are saved in a
local `Nukefire` atlas, survive restarts, and are not synced to the cloud.
Automatic mapping only adopts local areas. Existing cloud maps are left
untouched; moving a NukeFire map to cloud storage is always a user action.

Movement into an already mapped room updates the player marker immediately.
New rooms and exits are placed promptly without moving established rooms; an
expensive full reflow is deferred until map updates have been quiet for 350 ms.
A newer update cancels an obsolete full reflow without dropping the distinct
room snapshots still waiting to be mapped.

By default, a quiet full reflow has no wall-clock cutoff. Its dedicated
background Worker instead has deterministic work ceilings which shrink smoothly as the
area grows: each budget scales with the reciprocal of
residents × (residents + edges), so neighboring map sizes always receive
neighboring budgets, with no cliffs. The smallest maps retain up to 4,194,304
separator states; at the largest documented shape every budget rests on its
floor — 32,768 states, 32,768 constraint restarts, two complete polish
layouts, a two-tournament ceiling within three aggregate planner passes, 64
mask variants, and 512 crossing expansions. Small areas receive at most 16
aggregate polish passes, with medium-sized areas receiving smoothly scaled
intermediate caps. The first pass is the bounded early preview and every later
tournament consumes the same budget. These limits bound retained search state
while remaining independent of machine speed.
Every tier stops early at a layout with no directional, route, or crossing
violations. Fresh movement still cancels the obsolete search immediately. Disable
**Keep quiet reflow searching for a violation-free layout** to restore the
short 10-second repair policy.

The mapper stores `nukefire.layout.polish-pending` on an area whenever prompt
topology work or an interrupted/incomplete quiet pass leaves possible polish
work. Returning to that area passively schedules one new quiet attempt for the
visit. Movement that stays inside the area — repeated GMCP chatter or walking
between its rooms — re-arms that attempt rather than forfeiting it, so the
search resumes at the next quiet window; only leaving the area defers the
work. A visit spends at most eight such resumptions without a committed
improvement; every durable improvement, new topology, or re-entry restores
the allowance. The hint remains area-wide because a geometric fixed point is
relative to one entry anchor and its Map.Local chart: another entrance can
expose a different useful reflow basin. Cancellation and package restarts
therefore cannot lose the opportunity to improve the map from a later context.

To avoid repeating expensive deterministic work, the legacy-named
`nukefire.layout.polish-exhausted-fingerprint` property stores a versioned,
bounded set of exact fruitless contexts. A context key includes the resident
geometry, entry anchor, canonical chart, scaled work budgets, perfect-search
mode, and layout-algorithm generation. A completed fixed point or deterministic
work-ceiling stop adds that context; re-entering through the same context skips
the Worker, while a different entrance or chart remains eligible. The newest
32 contexts are retained. Any durable layout improvement or fresh topology
invalidates contexts for the old geometry; a pass which also proves a fixed
point records that proof against its final geometry, while an incomplete
improvement leaves the new geometry eligible. Cancelled, interrupted,
deadline-cut, and failed attempts never add a suppression.

Every strict best-so-far layout is applied while that search continues. Map
writes are serialized and naturally coalesce to the newest pending candidate,
so a slow durable write cannot build an unbounded improvement backlog. The
first improvement of a search applies immediately; later progressive commits
wait at least 1.5 seconds after the previous one, so a fast-improving search
reads as improvement rather than churn, and the final plan always applies
without delay. Each commit revalidates the area snapshot, updates connection
routing, and recenters the MapView if the player's room moved. A newer
movement snapshot cancels any pending obsolete candidate while preserving an
already committed improvement.

The package publishes `layoutState` through
`smudgy:state/kapusniak/nukefire-mapper`. It mirrors map-layout's live phase,
work counters, and current-versus-best quality without exposing mutable
planner internals. The mirror republishes at most five times per second,
retaining only the newest snapshot between publishes, and always ends on the
planner's final state.

Generated connections use orthogonal paths around intervening rooms only when
the path can leave and enter through the walls declared by both exits. They
store those paths as diagonal-tolerant routes so later layout changes cannot
invalidate the map. Their turns are drawn with rounded corners. Generated
routes are stored as solver-produced `Automatic` routing; a route drawn by
hand — `Manual` routing in the map editor — is user-owned and is never
overwritten by route recomputation.

When the layout engine reports that a crossing or obstruction sits between
rooms it is not allowed to move — a user-locked neighborhood, for example —
the mapper adopts the engine's proposed detour and draws that connection
around the problem instead of through it. The detour is stored as an ordinary
`Automatic` route, so it recomputes like any generated route and never touches
a hand-drawn one.

When a one-way arrival shares its destination wall with another connection,
the mapper spreads AutoPinned arrival ports across the room wall so arrowheads
remain distinguishable. Reciprocal midpoint slots and manually positioned
ports stay fixed; the arrangement is deterministic and recenters when the
crowding disappears.

## Setup

Enable the package and connect to NukeFire; mapping starts automatically. For
the complete map and radar interface, also enable
`smudgy://kapusniak/nukefire-scripts`.

Disable `smudgy://official/auto-mapper` while using this package. Running both
mappers can cause conflicting room and exit updates.

For troubleshooting, enable **Log mapping decisions (debug)** in the package
parameters. The JSONL log records every mapper mutation before drafting, after
drafting, and after acknowledgement, including results, durations, failures,
and any partially committed operation IDs.
