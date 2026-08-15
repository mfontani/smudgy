import type { GridPosition } from "./layout.ts";

export type RouteSide = "North" | "East" | "South" | "West";

/**
 * Stored geometry for a generated connection route. Generated paths prefer
 * orthogonal turn points, but `Direct` keeps that preference from becoming a
 * persistence invariant when endpoints move or no Manhattan path is found.
 */
export interface PlannedConnectionRoute {
  startSide: RouteSide;
  endSide: RouteSide;
  routing: "Manual" | "Automatic";
  segmentShape: "Direct";
  corner: "Rounded";
  routePoints: { x: number; y: number }[];
}

interface Step {
  x: number;
  y: number;
  side: RouteSide;
}

interface SearchState {
  x: number;
  y: number;
  direction: number;
  cost: number;
  priority: number;
  previous?: string;
}

const STEPS: readonly Step[] = [
  { x: 0, y: -1, side: "North" },
  { x: 1, y: 0, side: "East" },
  { x: 0, y: 1, side: "South" },
  { x: -1, y: 0, side: "West" },
];

const OPPOSITE_SIDE: Record<RouteSide, RouteSide> = {
  North: "South",
  East: "West",
  South: "North",
  West: "East",
};

function pointKey(x: number, y: number): string {
  return `${x}:${y}`;
}

function stateKey(x: number, y: number, direction: number): string {
  return `${x}:${y}:${direction}`;
}

function sameCell(a: GridPosition, b: GridPosition): boolean {
  return Math.round(a.x) === Math.round(b.x) && Math.round(a.y) === Math.round(b.y);
}

function segmentIntersectsRoom(
  from: GridPosition,
  to: GridPosition,
  room: GridPosition,
): boolean {
  if (from.level !== room.level || to.level !== room.level) return false;
  if (sameCell(from, room) || sameCell(to, room)) return false;

  // Smudgy rooms occupy roughly half of one grid interval. Treat a slightly
  // larger central square as blocked so diagonal connections do not graze it.
  const half = 0.32;
  const minX = room.x - half;
  const maxX = room.x + half;
  const minY = room.y - half;
  const maxY = room.y + half;
  const dx = to.x - from.x;
  const dy = to.y - from.y;
  let enter = 0;
  let leave = 1;
  const clips: readonly [number, number][] = [
    [-dx, from.x - minX],
    [dx, maxX - from.x],
    [-dy, from.y - minY],
    [dy, maxY - from.y],
  ];
  for (const [p, q] of clips) {
    if (p === 0) {
      if (q < 0) return false;
      continue;
    }
    const ratio = q / p;
    if (p < 0) {
      enter = Math.max(enter, ratio);
    } else {
      leave = Math.min(leave, ratio);
    }
    if (enter > leave) return false;
  }
  return true;
}

/** Rooms whose rendered cell is crossed by a straight connection. */
export function directRoomObstructions(
  from: GridPosition,
  to: GridPosition,
  rooms: readonly GridPosition[],
): GridPosition[] {
  return rooms.filter((room) => segmentIntersectsRoom(from, to, room));
}

class MinHeap {
  readonly #values: SearchState[] = [];

  get size(): number {
    return this.#values.length;
  }

  push(value: SearchState): void {
    this.#values.push(value);
    let index = this.#values.length - 1;
    while (index > 0) {
      const parent = Math.floor((index - 1) / 2);
      if (this.#values[parent].priority <= value.priority) break;
      this.#values[index] = this.#values[parent];
      index = parent;
    }
    this.#values[index] = value;
  }

  pop(): SearchState | undefined {
    const first = this.#values[0];
    const last = this.#values.pop();
    if (!first || !last || this.#values.length === 0) return first;
    let index = 0;
    while (true) {
      const left = index * 2 + 1;
      const right = left + 1;
      if (left >= this.#values.length) break;
      const child = right < this.#values.length &&
          this.#values[right].priority < this.#values[left].priority
        ? right
        : left;
      if (this.#values[child].priority >= last.priority) break;
      this.#values[index] = this.#values[child];
      index = child;
    }
    this.#values[index] = last;
    return first;
  }
}

function reconstruct(
  goal: SearchState,
  states: ReadonlyMap<string, SearchState>,
  level: number,
): GridPosition[] {
  const path: GridPosition[] = [];
  let current: SearchState | undefined = goal;
  while (current) {
    path.push({ x: current.x, y: current.y, level });
    current = current.previous ? states.get(current.previous) : undefined;
  }
  return path.reverse();
}

function search(
  from: GridPosition,
  to: GridPosition,
  blocked: ReadonlySet<string>,
  preferredStart: RouteSide | undefined,
  preferredEnd: RouteSide | undefined,
  padding: number,
): GridPosition[] | undefined {
  const startX = Math.round(from.x);
  const startY = Math.round(from.y);
  const endX = Math.round(to.x);
  const endY = Math.round(to.y);
  const minX = Math.min(startX, endX) - padding;
  const maxX = Math.max(startX, endX) + padding;
  const minY = Math.min(startY, endY) - padding;
  const maxY = Math.max(startY, endY) + padding;
  const frontier = new MinHeap();
  const states = new Map<string, SearchState>();
  const bestCost = new Map<string, number>();
  const start: SearchState = {
    x: startX,
    y: startY,
    direction: -1,
    cost: 0,
    priority: Math.abs(endX - startX) * 10 + Math.abs(endY - startY) * 10,
  };
  frontier.push(start);
  states.set(stateKey(startX, startY, -1), start);
  bestCost.set(stateKey(startX, startY, -1), 0);

  while (frontier.size > 0) {
    const current = frontier.pop() as SearchState;
    if (current.x === endX && current.y === endY) return reconstruct(current, states, from.level);
    const currentKey = stateKey(current.x, current.y, current.direction);
    if (current.cost !== bestCost.get(currentKey)) continue;

    for (let direction = 0; direction < STEPS.length; direction += 1) {
      const step = STEPS[direction];
      const x = current.x + step.x;
      const y = current.y + step.y;
      if (x < minX || x > maxX || y < minY || y > maxY) continue;
      const isGoal = x === endX && y === endY;
      if (!isGoal && blocked.has(pointKey(x, y))) continue;

      let cost = current.cost + 10;
      if (current.direction >= 0 && current.direction !== direction) cost += 3;
      if (current.direction < 0 && preferredStart && step.side !== preferredStart) cost += 8;
      if (isGoal && preferredEnd && step.side !== OPPOSITE_SIDE[preferredEnd]) cost += 8;
      const key = stateKey(x, y, direction);
      if ((bestCost.get(key) ?? Number.POSITIVE_INFINITY) <= cost) continue;
      const next: SearchState = {
        x,
        y,
        direction,
        cost,
        priority: cost + (Math.abs(endX - x) + Math.abs(endY - y)) * 10,
        previous: currentKey,
      };
      bestCost.set(key, cost);
      states.set(key, next);
      frontier.push(next);
    }
  }
  return undefined;
}

/**
 * Find an orthogonal centerline through unoccupied integral cells. The endpoint
 * rooms are allowed; every interior room cell is an obstacle.
 */
export function routeAroundRooms(
  from: GridPosition,
  to: GridPosition,
  rooms: readonly GridPosition[],
  preferredStart?: RouteSide,
  preferredEnd?: RouteSide,
): GridPosition[] | undefined {
  if (from.level !== to.level) return undefined;
  const blocked = new Set(
    rooms
      .filter((room) => room.level === from.level && !sameCell(room, from) && !sameCell(room, to))
      .map((room) => pointKey(Math.round(room.x), Math.round(room.y))),
  );
  for (const padding of [3, 6, 12, 24, 48]) {
    const path = search(from, to, blocked, preferredStart, preferredEnd, padding);
    if (path) return path;
  }
  return undefined;
}

/** Keep only orthogonal turn vertices; connection endpoints are not route points. */
export function routeTurnPoints(path: readonly GridPosition[]): { x: number; y: number }[] {
  const result: { x: number; y: number }[] = [];
  for (let index = 1; index < path.length - 1; index += 1) {
    const before = path[index - 1];
    const current = path[index];
    const after = path[index + 1];
    const beforeDirection = `${current.x - before.x}:${current.y - before.y}`;
    const afterDirection = `${after.x - current.x}:${after.y - current.y}`;
    if (beforeDirection !== afterDirection) result.push({ x: current.x, y: current.y });
  }
  return result;
}

export function routeStartSide(path: readonly GridPosition[]): RouteSide | undefined {
  if (path.length < 2) return undefined;
  const dx = path[1].x - path[0].x;
  const dy = path[1].y - path[0].y;
  return STEPS.find((step) => step.x === dx && step.y === dy)?.side;
}

export function routeEndSide(path: readonly GridPosition[]): RouteSide | undefined {
  if (path.length < 2) return undefined;
  const before = path[path.length - 2];
  const end = path[path.length - 1];
  const dx = before.x - end.x;
  const dy = before.y - end.y;
  return STEPS.find((step) => step.x === dx && step.y === dy)?.side;
}

/**
 * Prefer a Manhattan route around occupied rooms without requiring stored
 * segments to remain perfectly axis-aligned. The renderer still follows the
 * generated turn points; `Direct` permits a diagonal fallback or harmless
 * endpoint drift, while `Rounded` fillets every resulting turn.
 */
export function planConnectionRoute(
  from: GridPosition,
  to: GridPosition,
  rooms: readonly GridPosition[],
  preferredStart: RouteSide,
  preferredEnd: RouteSide,
  knownObstructed?: boolean,
): PlannedConnectionRoute {
  const obstructed = knownObstructed ?? directRoomObstructions(from, to, rooms).length > 0;
  if (obstructed) {
    const path = routeAroundRooms(from, to, rooms, preferredStart, preferredEnd);
    if (path) {
      return {
        startSide: routeStartSide(path) ?? preferredStart,
        endSide: routeEndSide(path) ?? preferredEnd,
        routing: "Manual",
        segmentShape: "Direct",
        corner: "Rounded",
        routePoints: routeTurnPoints(path),
      };
    }
  }
  return {
    startSide: preferredStart,
    endSide: preferredEnd,
    routing: "Automatic",
    segmentShape: "Direct",
    corner: "Rounded",
    routePoints: [],
  };
}
