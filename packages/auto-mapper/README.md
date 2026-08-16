# auto-mapper

Maps as you explore, from the room data your game already sends. Works with GMCP
(`Room.Info` — Aardwolf, IRE games, and most modern MUDs) and MSDP (`ROOM` or the flat
`ROOM_*` variables). Generic `Room.Info` identity fields are read in `num`, `vnum`, then
`id` precedence.

- **Rooms you've mapped are followed** — the map pane tracks your position, and speedwalks
  work over everything you've explored.
- **Rooms you haven't are drawn for you** into durable local maps: one per game zone,
  saved automatically on this device instead of disappearing with the session.
- **`savemap cloud`** moves what you've mapped into cloud storage. Append a zone name to
  move only one zone (`savemap cloud Midgaard`); mapping continues into the acknowledged
  cloud copies. The legacy `savemap`/`savemap local` forms remain safe no-ops for maps that
  are already local.
- **Saved zones are resumed automatically.** On later sessions, a local or cloud zone map
  is picked up by name and mapping continues into it — no duplicate copy. Rename a map if you
  want the auto-mapper to leave it alone.
- **Revisits keep the map honest.** Walking through a mapped room refreshes its title and
  terrain and picks up newly advertised exits. Exits are never removed unless you turn on
  `mapprune` (then compass exits the game stops reporting are pruned from revisited rooms).
- **Every room the game names is on the map.** Unexplored neighbors appear immediately as
  dimmed, unvisited rooms one step from where they were mentioned; walking into one fills
  it in — name, terrain, exits, server coordinates, even the right zone map if the guess
  was wrong. Exits whose destination the game doesn't identify appear as stubs instead.
  Rooms are placed by server coordinates when the game provides them, by your movement
  when it doesn't: the mapper watches the direction commands you send (and speedwalks
  send), so games that don't reveal exit destinations still map correctly. Non-compass
  exits ("enter grate", portals) are kept as special exits traversed by their command.
- **Movement-based maps reflow as topology grows.** The shared `map-layout` planner keeps
  directional links compact and collision-free instead of stretching a new link past an
  occupied cell. Server-coordinate rooms stay fixed. To protect a hand-positioned room
  too, give it the `LAYOUT_LOCKED` tag or set its `layoutLocked` property to `true`.
- **Server neighborhoods are applied in batches.** Consecutive unprocessed snapshots for
  the same center collapse to the newest one; room metadata and same-area topology use one
  durable mutation batch apiece instead of acknowledging every advertised exit separately.
- Mazes and other rooms where the game withholds identity are left alone — the mapper
  never guesses. Overland/continent grids are followed but never drawn.

Development notes (not user docs): this is the first-party reference consumer of the
durable local map tier, `externalId` room identity, and the dual-protocol room-data producers
(`docs/gmcp-mapping.md` §5.3). It runs sandboxed under
`interop:read + mapper:write + automations:aliases + session:echo + gmcp:send`
(movement observation subscribes to `smudgy:events/sys` `send`, which rides
`interop:read`); the e2e coverage lives in `core/tests/auto_mapper_package.rs`.
