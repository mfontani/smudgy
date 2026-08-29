import {
  compactIntegralLayoutPlan,
  compareLayoutQuality,
  computeIntegralRouteAmendments,
  directionalViolationEdges,
  measureIntegralLayoutQuality,
  measureLayoutRoutingQuality,
  planIntegralLayout,
  repairIntegralLayoutCrossingsDeep,
  withIntegralLayoutCandidateAdmission,
  type ConstraintRepairOptions,
  type ConstraintRepairReport,
  type ConstraintRepairWorkStats,
  type GridPosition,
  type IntegralLayoutPlan,
  type IntegralLayoutRequest,
  type LayoutDirection,
  type LayoutEdge,
  type LayoutQuality,
  type LayoutTraceEvent,
} from "./layout.ts";
import { planLayoutModel, type LayoutModel } from "./model.ts";
import {
  searchConstraintExtensions,
  type ConstraintExtensionAlternative,
  type ConstraintExtensionArc,
  type ConstraintExtensionDefect,
  type ConstraintExtensionInspection,
} from "./constraint-extension-search.ts";

const AXIS_NAMES = ["x", "y", "level"] as const;
const ORTHOGONAL_VECTORS: Partial<Record<LayoutDirection, readonly [number, number, number]>> = {
  North: [0, -1, 0],
  East: [1, 0, 0],
  South: [0, 1, 0],
  West: [-1, 0, 0],
  Up: [0, 0, 1],
  Down: [0, 0, -1],
};
const DEFAULT_MAX_DURATION_MS = 10_000;
const DEFAULT_MAX_RESTARTS = 10_000;
const RANDOM_SEED = 0x5EED1234;
const PROGRESS_INTERVAL_MS = 30;
const SEARCH_PROGRESS_CHECK_MASK = 0x0f;
const COMPACTION_PROGRESS_CHECK_MASK = 0x01;
const DEFAULT_MAX_EXTENSION_STATES = 16_384;
const DEFAULT_MAX_MASK_DIVERSIFICATIONS = 256;
/**
 * Deterministic ceilings for the exact hitting-set master strategy. Each
 * iteration costs one feasibility check; the node budget is shared across all
 * of an operation's exact solves. Both are far above what realistic conflict
 * structures need — hitting either one means conflict accumulation went
 * pathological, and the search falls back to seeded randomized restarts.
 */
const MAX_HITTING_SET_ITERATIONS = 256;
const MAX_HITTING_SET_DFS_NODES = 262_144;
/** Level equality carried by a planar relation is topology, not a removable ray constraint. */
const HARD_LEVEL_EQUALITY = -1;

interface DenseConstraintGraph {
  ids: string[];
  indexById: Map<string, number>;
  positions: readonly Int32Array[];
  sourceEdges: LayoutEdge[];
  sourceIndexes: Int32Array;
  from: Int32Array;
  to: Int32Array;
  axis: Int8Array;
  sign: Int8Array;
  step: Int32Array;
  reciprocal: Uint8Array;
  /** Feasibility-equivalent source edges share one canonical relation. */
  groupOfEdge: Int32Array;
  groupAxis: Int8Array;
  groupFrom: Int32Array;
  groupTo: Int32Array;
  groupStep: Int32Array;
  /** Directed source-edge cost paid when this canonical relation is relaxed. */
  groupSourceCount: Int32Array;
  /** Reciprocal directed source-edge cost paid when this relation is relaxed. */
  groupReciprocalCount: Int32Array;
  /** Nodes whose topological component contains an authoritative level-crossing relation. */
  levelCrossingReachable: Uint8Array;
  groupCount: number;
  nodeCount: number;
  edgeCount: number;
  /** Lazily attached feasibility-check scratch arena; see `AnalysisScratch`. */
  scratch?: AnalysisScratch;
}

interface AxisGraph {
  head: Int32Array;
  to: Int32Array;
  next: Int32Array;
  sourceNode: Int32Array;
  targetNode: Int32Array;
  sourceRoot: Int32Array;
  targetRoot: Int32Array;
  edge: Int32Array;
  step: Int32Array;
  length: number;
}

interface FeasibleState {
  roots: readonly Int32Array[];
  graphs: readonly AxisGraph[];
}

interface AnalysisResult {
  conflict?: number[];
  state?: FeasibleState;
}

type ConstraintFailureReason =
  | "analysis"
  | "compaction"
  | "time"
  | "work";

type ConstraintResult<T> =
  | { ok: true; value: T }
  | { ok: false; reason: ConstraintFailureReason };

interface SearchRuntimeOptions {
  now?: () => number;
  maximumFeasibilityChecks?: number;
  /** Test seam: deterministic node budget shared by the exact hitting-set solves. */
  maximumHittingSetNodes?: number;
  progress?: (progress: ConstraintRepairWorkStats & {
    phase: "search" | "compaction" | "polish";
    restarts: number;
    feasibilityChecks: number;
    layoutsConsidered: number;
    compactionAttempts: number;
    elapsedMs: number;
    bestQuality?: Readonly<import("./layout.ts").LayoutQuality>;
  }) => void;
  /** Streams each feasible canonical mask before randomized exploration ends. */
  mask?: (
    removedGroups: Uint8Array,
    progress: { restarts: number; feasibilityChecks: number; elapsedMs: number },
  ) => boolean | void;
}

interface ConstraintSearchResult {
  /** Canonical relation-group mask for the best weighted master incumbent. */
  removed: Uint8Array;
  /** Best distinct feasible group masks retained for geometric diversification. */
  masks: Uint8Array[];
  score: readonly [number, number];
  lowerBound: number;
  optimal: boolean;
  cutoff: ConstraintRepairReport["cutoff"];
  restarts: number;
  feasibilityChecks: number;
  elapsedMs: number;
}

interface RepairRuntimeOptions extends SearchRuntimeOptions {
  search?: (
    graph: DenseConstraintGraph,
    options: ConstraintRepairOptions,
    standardPositions: ReadonlyMap<string, GridPosition>,
    runtime: SearchRuntimeOptions,
  ) => ConstraintResult<ConstraintSearchResult>;
  compact?: (
    graph: DenseConstraintGraph,
    removed: Uint8Array,
  ) => ConstraintResult<Map<string, GridPosition>>;
  /** Test seam for the compaction-only polish applied to publishable raw incumbents. */
  gravity?: typeof compactIntegralLayoutPlan;
  polish?: typeof polishConstraintLayoutToFixedPoint;
}

class DenseUnionFind {
  parent: Int32Array;
  #rank: Uint8Array;

  constructor(size: number) {
    this.parent = new Int32Array(size);
    this.#rank = new Uint8Array(size);
    for (let index = 0; index < size; index += 1) this.parent[index] = index;
  }

  /** Restores the identity partition over the first `size` entries, growing capacity geometrically when needed. */
  reset(size: number): void {
    if (size > this.parent.length) {
      const capacity = Math.max(size, this.parent.length * 2);
      this.parent = new Int32Array(capacity);
      this.#rank = new Uint8Array(capacity);
    }
    for (let index = 0; index < size; index += 1) this.parent[index] = index;
    this.#rank.fill(0, 0, size);
  }

  find(value: number): number {
    let root = value;
    while (this.parent[root] !== root) root = this.parent[root];
    while (this.parent[value] !== value) {
      const next = this.parent[value];
      this.parent[value] = root;
      value = next;
    }
    return root;
  }

  union(a: number, b: number): void {
    let rootA = this.find(a);
    let rootB = this.find(b);
    if (rootA === rootB) return;
    if (this.#rank[rootA] < this.#rank[rootB]) [rootA, rootB] = [rootB, rootA];
    this.parent[rootB] = rootA;
    if (this.#rank[rootA] === this.#rank[rootB]) this.#rank[rootA] += 1;
  }
}

class DenseMinHeap {
  readonly #values: number[] = [];

  get length(): number {
    return this.#values.length;
  }

  clear(): void {
    this.#values.length = 0;
  }

  push(value: number): void {
    let index = this.#values.length;
    this.#values.push(value);
    while (index > 0) {
      const parent = (index - 1) >>> 1;
      if (this.#values[parent] <= value) break;
      this.#values[index] = this.#values[parent];
      index = parent;
    }
    this.#values[index] = value;
  }

  pop(): number | undefined {
    const first = this.#values[0];
    const last = this.#values.pop();
    if (last === undefined || this.#values.length === 0) return first;
    let index = 0;
    while (true) {
      const left = index * 2 + 1;
      if (left >= this.#values.length) break;
      const right = left + 1;
      const child = right < this.#values.length && this.#values[right] < this.#values[left]
        ? right
        : left;
      if (this.#values[child] >= last) break;
      this.#values[index] = this.#values[child];
      index = child;
    }
    this.#values[index] = last;
    return first;
  }
}

/**
 * Reusable typed-array battery for `analyzeConstraints`.
 *
 * One arena belongs to one compiled graph and lives on it for the duration of
 * a repair, so tens of thousands of feasibility checks share one set of
 * buffers instead of allocating ~50 arrays each. That sharing is sound
 * because analysis is synchronous, single-threaded, and never re-enters
 * itself on the same graph: `shouldStop` predicates observe clocks and
 * counters only, never start another analysis.
 *
 * Ownership rule: nothing handed out by the arena may escape an
 * `analyzeConstraints` call. The one escaping product — the `includeState`
 * feasible-state snapshot — is copied out at its return sites; conflict
 * results are freshly built plain arrays. Buffers are reset per check with
 * prefix fills or write-before-read discipline; capacity only grows,
 * geometrically, and is kept for later checks.
 */
class AnalysisScratch {
  #nodeCapacity = 0;
  #groupCapacity = 0;
  readonly unions = [new DenseUnionFind(0), new DenseUnionFind(0), new DenseUnionFind(0)];
  readonly equalityHead: Int32Array[] = [new Int32Array(0), new Int32Array(0), new Int32Array(0)];
  readonly equalityTo: Int32Array[] = [new Int32Array(0), new Int32Array(0), new Int32Array(0)];
  readonly equalityEdge: Int32Array[] = [new Int32Array(0), new Int32Array(0), new Int32Array(0)];
  readonly equalityNext: Int32Array[] = [new Int32Array(0), new Int32Array(0), new Int32Array(0)];
  readonly equalityLength = new Int32Array(3);
  readonly roots: Int32Array[] = [new Int32Array(0), new Int32Array(0), new Int32Array(0)];
  /** Indexed by canonical relation group; cleared lazily by `uniqueConflict`. */
  conflictMarker = new Uint8Array(0);
  pathQueue = new Int32Array(0);
  pathPreviousNode = new Int32Array(0);
  pathPreviousEdge = new Int32Array(0);
  /**
   * One set of per-axis relation-graph buffers serves all three axes in turn:
   * nothing reads an earlier axis's arrays after its iteration ends, because
   * the escaping state snapshot is sliced out at push time.
   */
  axisHead = new Int32Array(0);
  axisTo = new Int32Array(0);
  axisNext = new Int32Array(0);
  axisSourceNode = new Int32Array(0);
  axisTargetNode = new Int32Array(0);
  axisSourceRoot = new Int32Array(0);
  axisTargetRoot = new Int32Array(0);
  axisEdge = new Int32Array(0);
  axisStep = new Int32Array(0);
  color = new Uint8Array(0);
  parentArc = new Int32Array(0);
  readonly nodeStack: number[] = [];
  readonly arcStack: number[] = [];
  readonly triples = new Map<number, number>();

  ensure(nodeCount: number, groupCount: number): void {
    if (nodeCount > this.#nodeCapacity) {
      const capacity = Math.max(nodeCount, this.#nodeCapacity * 2);
      this.#nodeCapacity = capacity;
      for (let axis = 0; axis < 3; axis += 1) {
        this.equalityHead[axis] = new Int32Array(capacity);
        this.roots[axis] = new Int32Array(capacity);
      }
      this.pathQueue = new Int32Array(capacity);
      this.pathPreviousNode = new Int32Array(capacity);
      this.pathPreviousEdge = new Int32Array(capacity);
      this.axisHead = new Int32Array(capacity);
      this.color = new Uint8Array(capacity);
      this.parentArc = new Int32Array(capacity);
    }
    if (groupCount > this.#groupCapacity) {
      const capacity = Math.max(groupCount, this.#groupCapacity * 2);
      this.#groupCapacity = capacity;
      for (let axis = 0; axis < 3; axis += 1) {
        this.equalityTo[axis] = new Int32Array(capacity * 2);
        this.equalityEdge[axis] = new Int32Array(capacity * 2);
        this.equalityNext[axis] = new Int32Array(capacity * 2);
      }
      this.conflictMarker = new Uint8Array(capacity);
      this.axisTo = new Int32Array(capacity);
      this.axisNext = new Int32Array(capacity);
      this.axisSourceNode = new Int32Array(capacity);
      this.axisTargetNode = new Int32Array(capacity);
      this.axisSourceRoot = new Int32Array(capacity);
      this.axisTargetRoot = new Int32Array(capacity);
      this.axisEdge = new Int32Array(capacity);
      this.axisStep = new Int32Array(capacity);
    }
  }
}

function success<T>(value: T): ConstraintResult<T> {
  return { ok: true, value };
}

function failure(reason: ConstraintFailureReason): ConstraintResult<never> {
  return { ok: false, reason };
}

function integralPosition(position: GridPosition): GridPosition {
  return {
    x: Math.round(position.x),
    y: Math.round(position.y),
    level: Math.round(position.level),
  };
}

function protectedVector(edge: LayoutEdge): readonly [number, number, number] | undefined {
  if (edge.constraintVector) {
    return [
      Math.round(edge.constraintVector.x),
      Math.round(edge.constraintVector.y),
      Math.round(edge.constraintVector.level),
    ];
  }
  return ORTHOGONAL_VECTORS[edge.direction];
}

function vectorKey(from: string, to: string, vector: readonly number[]): string {
  return `${from}\u0000${to}\u0000${vector.join(",")}`;
}

/** Compile only single-axis protected rays. Diagonal custom rays retain the ordinary planner. */
function compileGraph(
  positions: ReadonlyMap<string, GridPosition>,
  edges: readonly LayoutEdge[],
): DenseConstraintGraph | undefined {
  const ids = [...positions.keys()].sort();
  const indexById = new Map(ids.map((id, index) => [id, index]));
  const sourceEdges: LayoutEdge[] = [];
  const sourceIndexes: number[] = [];
  const vectors: (readonly [number, number, number])[] = [];
  for (let sourceIndex = 0; sourceIndex < edges.length; sourceIndex += 1) {
    const edge = edges[sourceIndex];
    if (!indexById.has(edge.from) || !indexById.has(edge.to)) continue;
    const vector = protectedVector(edge);
    if (!vector) continue;
    const nonzero = vector.filter((value) => value !== 0);
    if (nonzero.length !== 1 || Math.abs(nonzero[0]) !== 1) return undefined;
    sourceEdges.push(edge);
    sourceIndexes.push(sourceIndex);
    vectors.push(vector);
  }

  const reciprocalKeys = new Set<string>();
  for (let edge = 0; edge < sourceEdges.length; edge += 1) {
    reciprocalKeys.add(vectorKey(sourceEdges[edge].from, sourceEdges[edge].to, vectors[edge]));
  }
  const edgeCount = sourceEdges.length;
  const from = new Int32Array(edgeCount);
  const to = new Int32Array(edgeCount);
  const axis = new Int8Array(edgeCount);
  const sign = new Int8Array(edgeCount);
  const step = new Int32Array(edgeCount);
  const reciprocal = new Uint8Array(edgeCount);
  const groupKeys = new Map<string, number>();
  const groupIndexes: number[] = [];
  const groupAxes: number[] = [];
  const groupFroms: number[] = [];
  const groupTos: number[] = [];
  const groupSteps: number[] = [];
  const groupSourceCounts: number[] = [];
  const groupReciprocalCounts: number[] = [];
  for (let edge = 0; edge < edgeCount; edge += 1) {
    const vector = vectors[edge];
    const edgeAxis = vector.findIndex((value) => value !== 0);
    from[edge] = indexById.get(sourceEdges[edge].from) as number;
    to[edge] = indexById.get(sourceEdges[edge].to) as number;
    axis[edge] = edgeAxis;
    sign[edge] = Math.sign(vector[edgeAxis]);
    step[edge] = Math.abs(vector[edgeAxis]);
    const low = sign[edge] > 0 ? from[edge] : to[edge];
    const high = sign[edge] > 0 ? to[edge] : from[edge];
    const groupKey = `${edgeAxis}:${low}:${high}:${step[edge]}`;
    let group = groupKeys.get(groupKey);
    if (group === undefined) {
      group = groupKeys.size;
      groupKeys.set(groupKey, group);
      groupAxes.push(edgeAxis);
      groupFroms.push(low);
      groupTos.push(high);
      groupSteps.push(step[edge]);
      groupSourceCounts.push(0);
      groupReciprocalCounts.push(0);
    }
    groupIndexes.push(group);
    reciprocal[edge] = reciprocalKeys.has(vectorKey(
        sourceEdges[edge].to,
        sourceEdges[edge].from,
        vector.map((value) => -value),
      ))
      ? 1
      : 0;
    groupSourceCounts[group] += 1;
    groupReciprocalCounts[group] += reciprocal[edge];
  }
  const densePositions = AXIS_NAMES.map((axisName) =>
    Int32Array.from(ids, (id) => Math.round((positions.get(id) as GridPosition)[axisName]))
  );
  // Constraint repair may adjust levels only in a component which actually
  // contains a level-crossing relation. Planar-only and isolated components
  // keep the authoritative levels established by the standard planner.
  const topology = new DenseUnionFind(ids.length);
  for (let group = 0; group < groupKeys.size; group += 1) {
    topology.union(groupFroms[group], groupTos[group]);
  }
  const levelCrossingRoots = new Uint8Array(ids.length);
  for (let group = 0; group < groupKeys.size; group += 1) {
    if (groupAxes[group] === 2) levelCrossingRoots[topology.find(groupFroms[group])] = 1;
  }
  const levelCrossingReachable = Uint8Array.from(
    ids,
    (_id, node) => levelCrossingRoots[topology.find(node)],
  );
  return {
    ids,
    indexById,
    positions: densePositions,
    sourceEdges,
    sourceIndexes: Int32Array.from(sourceIndexes),
    from,
    to,
    axis,
    sign,
    step,
    reciprocal,
    groupOfEdge: Int32Array.from(groupIndexes),
    groupAxis: Int8Array.from(groupAxes),
    groupFrom: Int32Array.from(groupFroms),
    groupTo: Int32Array.from(groupTos),
    groupStep: Int32Array.from(groupSteps),
    groupSourceCount: Int32Array.from(groupSourceCounts),
    groupReciprocalCount: Int32Array.from(groupReciprocalCounts),
    levelCrossingReachable,
    groupCount: groupKeys.size,
    nodeCount: ids.length,
    edgeCount,
  };
}

function analyzeConstraints(
  graph: DenseConstraintGraph,
  removedGroups: Uint8Array,
  includeState = false,
  shouldStop?: () => boolean,
): ConstraintResult<AnalysisResult> {
  const { nodeCount, groupCount } = graph;
  const scratch = graph.scratch ??= new AnalysisScratch();
  scratch.ensure(nodeCount, groupCount);
  let work = 0;
  const interrupted = (): boolean => {
    work += 1;
    return (work & 0x3ff) === 0 && shouldStop?.() === true;
  };
  const unions = scratch.unions;
  const equalityHead = scratch.equalityHead;
  for (let axis = 0; axis < 3; axis += 1) {
    unions[axis].reset(nodeCount);
    equalityHead[axis].fill(-1, 0, nodeCount);
  }
  const equalityTo = scratch.equalityTo;
  const equalityEdge = scratch.equalityEdge;
  const equalityNext = scratch.equalityNext;
  const equalityLength = scratch.equalityLength;
  equalityLength.fill(0);
  const addEquality = (axis: number, from: number, to: number, edge: number): void => {
    unions[axis].union(from, to);
    let cursor = equalityLength[axis]++;
    equalityTo[axis][cursor] = to;
    equalityEdge[axis][cursor] = edge;
    equalityNext[axis][cursor] = equalityHead[axis][from];
    equalityHead[axis][from] = cursor;
    cursor = equalityLength[axis]++;
    equalityTo[axis][cursor] = from;
    equalityEdge[axis][cursor] = edge;
    equalityNext[axis][cursor] = equalityHead[axis][to];
    equalityHead[axis][to] = cursor;
  };

  // A planar relation always says that its endpoints share one level. Relaxing
  // its directional x/y ray must not turn z into another collision-avoidance
  // axis. These hard equalities deliberately have no removable group in a
  // conflict explanation; only the still-active ray relations around them can
  // be selected by the hitting-set master.
  for (let group = 0; group < graph.groupCount; group += 1) {
    if (interrupted()) return failure("time");
    if (graph.groupAxis[group] !== 2) {
      addEquality(2, graph.groupFrom[group], graph.groupTo[group], HARD_LEVEL_EQUALITY);
    }
  }

  // Search and analysis otherwise operate only on canonical relation groups.
  // Source-edge multiplicity survives exclusively as the exact objective weight.
  for (let group = 0; group < graph.groupCount; group += 1) {
    if (interrupted()) return failure("time");
    if (removedGroups[group]) continue;
    for (let axis = 0; axis < 3; axis += 1) {
      if (graph.groupAxis[group] === axis) continue;
      if (axis === 2 && graph.groupAxis[group] !== 2) continue;
      addEquality(axis, graph.groupFrom[group], graph.groupTo[group], group);
    }
  }

  const roots = scratch.roots;
  for (let axis = 0; axis < 3; axis += 1) {
    for (let node = 0; node < nodeCount; node += 1) {
      if (interrupted()) return failure("time");
      roots[axis][node] = unions[axis].find(node);
    }
  }
  const marker = scratch.conflictMarker;
  const uniqueConflict = (edges: readonly number[]): number[] => {
    // Conflict members are canonical relation groups, and at most one conflict
    // is assembled per check, so the marker is cleared lazily here.
    marker.fill(0, 0, groupCount);
    const result: number[] = [];
    for (const edge of edges) {
      if (marker[edge]) continue;
      marker[edge] = 1;
      result.push(edge);
    }
    return result;
  };
  const equalityPath = (
    axis: number,
    start: number,
    finish: number,
  ): ConstraintResult<number[]> => {
    if (start === finish) return success([]);
    const queue = scratch.pathQueue;
    const previousNode = scratch.pathPreviousNode;
    // `queue` and `previousEdge` follow write-before-read discipline: entries
    // are read only for nodes discovered in this traversal.
    const previousEdge = scratch.pathPreviousEdge;
    previousNode.fill(-2, 0, nodeCount);
    let read = 0;
    let write = 0;
    queue[write++] = start;
    previousNode[start] = -1;
    while (read < write) {
      if (interrupted()) return failure("time");
      const node = queue[read++];
      for (let cursor = equalityHead[axis][node]; cursor !== -1; cursor = equalityNext[axis][cursor]) {
        const next = equalityTo[axis][cursor];
        if (previousNode[next] !== -2) continue;
        previousNode[next] = node;
        previousEdge[next] = equalityEdge[axis][cursor];
        if (next === finish) {
          const result: number[] = [];
          let current = finish;
          while (current !== start) {
            if (previousEdge[current] !== HARD_LEVEL_EQUALITY) {
              result.push(previousEdge[current]);
            }
            current = previousNode[current];
          }
          return success(result);
        }
        queue[write++] = next;
      }
    }
    return failure("analysis");
  };

  const axisGraphs: AxisGraph[] = [];
  for (let axis = 0; axis < 3; axis += 1) {
    const head = scratch.axisHead;
    const to = scratch.axisTo;
    const next = scratch.axisNext;
    const sourceNode = scratch.axisSourceNode;
    const targetNode = scratch.axisTargetNode;
    const sourceRoot = scratch.axisSourceRoot;
    const targetRoot = scratch.axisTargetRoot;
    const sourceEdge = scratch.axisEdge;
    const step = scratch.axisStep;
    head.fill(-1, 0, nodeCount);
    let length = 0;
    for (let group = 0; group < graph.groupCount; group += 1) {
      if (interrupted()) return failure("time");
      if (removedGroups[group] || graph.groupAxis[group] !== axis) continue;
      const lowNode = graph.groupFrom[group];
      const highNode = graph.groupTo[group];
      const lowRoot = roots[axis][lowNode];
      const highRoot = roots[axis][highNode];
      if (lowRoot === highRoot) {
        const path = equalityPath(axis, lowNode, highNode);
        if (!path.ok) return path;
        return success({ conflict: uniqueConflict([group, ...path.value]) });
      }
      to[length] = highRoot;
      sourceNode[length] = lowNode;
      targetNode[length] = highNode;
      sourceRoot[length] = lowRoot;
      targetRoot[length] = highRoot;
      sourceEdge[length] = group;
      step[length] = graph.groupStep[group];
      next[length] = head[lowRoot];
      head[lowRoot] = length++;
    }

    const color = scratch.color;
    const parentArc = scratch.parentArc;
    color.fill(0, 0, nodeCount);
    parentArc.fill(-1, 0, nodeCount);
    const nodeStack = scratch.nodeStack;
    const arcStack = scratch.arcStack;
    let cycle: number[] | undefined;
    for (let root = 0; root < nodeCount && !cycle; root += 1) {
      if (roots[axis][root] !== root || color[root] !== 0) continue;
      nodeStack.length = 0;
      arcStack.length = 0;
      nodeStack.push(root);
      arcStack.push(head[root]);
      color[root] = 1;
      while (nodeStack.length > 0 && !cycle) {
        if (interrupted()) return failure("time");
        const depth = nodeStack.length - 1;
        const node = nodeStack[depth];
        const arc = arcStack[depth];
        if (arc === -1) {
          color[node] = 2;
          nodeStack.pop();
          arcStack.pop();
          continue;
        }
        arcStack[depth] = next[arc];
        const target = to[arc];
        if (color[target] === 0) {
          parentArc[target] = arc;
          color[target] = 1;
          nodeStack.push(target);
          arcStack.push(head[target]);
        } else if (color[target] === 1) {
          const path: number[] = [];
          let cursor = node;
          while (cursor !== target) {
            const parent = parentArc[cursor];
            if (parent === -1) return failure("analysis");
            path.push(parent);
            cursor = sourceRoot[parent];
          }
          path.reverse();
          path.push(arc);
          cycle = path;
        }
      }
    }
    if (cycle) {
      const conflict = cycle.map((arc) => sourceEdge[arc]);
      for (let index = 0; index < cycle.length; index += 1) {
        const before = cycle[index];
        const after = cycle[(index + 1) % cycle.length];
        const path = equalityPath(axis, targetNode[before], sourceNode[after]);
        if (!path.ok) return path;
        conflict.push(...path.value);
      }
      return success({ conflict: uniqueConflict(conflict) });
    }
    // Escape point: the feasible-state snapshot outlives this check (it seeds
    // an entire extension search), so it is copied out of the arena here. The
    // copies match the historical exact allocation sizes.
    if (includeState) {
      axisGraphs.push({
        head: head.slice(0, nodeCount),
        to: to.slice(0, groupCount),
        next: next.slice(0, groupCount),
        sourceNode: sourceNode.slice(0, groupCount),
        targetNode: targetNode.slice(0, groupCount),
        sourceRoot: sourceRoot.slice(0, groupCount),
        targetRoot: targetRoot.slice(0, groupCount),
        edge: sourceEdge.slice(0, groupCount),
        step: step.slice(0, groupCount),
        length,
      });
    }
  }

  const triples = scratch.triples;
  triples.clear();
  for (let node = 0; node < nodeCount; node += 1) {
    if (interrupted()) return failure("time");
    const key = roots[0][node] + nodeCount * (roots[1][node] + nodeCount * roots[2][node]);
    const previous = triples.get(key);
    if (previous !== undefined) {
      const conflict: number[] = [];
      for (let axis = 0; axis < 3; axis += 1) {
        const path = equalityPath(axis, previous, node);
        if (!path.ok) return path;
        conflict.push(...path.value);
      }
      return success({ conflict: uniqueConflict(conflict) });
    }
    triples.set(key, node);
  }
  if (!includeState) return success({});
  // Escape point: the retained state snapshot must survive later checks, so
  // the roots leave the arena as exact-size copies alongside the axis graphs.
  return success({
    state: {
      roots: [
        roots[0].slice(0, nodeCount),
        roots[1].slice(0, nodeCount),
        roots[2].slice(0, nodeCount),
      ],
      graphs: axisGraphs,
    },
  });
}

/**
 * Collect pairwise-disjoint conflict cores by repeatedly removing every group
 * of each discovered conflict and re-checking. Each core is a certificate that
 * at least one of its groups must be removed by any feasible mask, so the sum
 * of per-core minimum weights is a valid objective lower bound — and the cores
 * themselves seed the exact hitting-set strategy, which subsumes this bound.
 */
function collectDisjointConflictCores(
  graph: DenseConstraintGraph,
  check: (removed: Uint8Array, includeState?: boolean) => ConstraintResult<AnalysisResult>,
): ConstraintResult<{ cores: number[][]; lowerBound: number; complete: boolean }> {
  const excluded = new Uint8Array(graph.groupCount);
  const cores: number[][] = [];
  let lowerBound = 0;
  for (;;) {
    const checked = check(excluded);
    if (!checked.ok) {
      if (checked.reason === "time" || checked.reason === "work") {
        return success({ cores, lowerBound, complete: false });
      }
      return checked;
    }
    const conflict = checked.value.conflict;
    if (!conflict) return success({ cores, lowerBound, complete: true });
    if (conflict.length === 0) return failure("analysis");
    let minimumWeight = Number.POSITIVE_INFINITY;
    let changed = false;
    for (const group of conflict) {
      minimumWeight = Math.min(minimumWeight, graph.groupSourceCount[group]);
      if (!excluded[group]) changed = true;
      excluded[group] = 1;
    }
    if (!Number.isFinite(minimumWeight)) return failure("analysis");
    cores.push(conflict);
    lowerBound += minimumWeight;
    if (!changed) return failure("analysis");
  }
}

interface HittingSetSolution {
  /** Exact only when true; false means the node budget cut the solve. */
  complete: boolean;
  /** Present exactly when a hitting set strictly better than the incumbent exists. */
  mask?: Uint8Array;
  score?: readonly [number, number];
}

/**
 * Exact minimum-weight hitting set over the accumulated conflict cores, via
 * depth-first branch and bound under a deterministic shared node budget.
 * Weights are the master objective — primary directed source edges, secondary
 * reciprocal source edges, compared lexicographically. The incumbent to beat
 * is the best known feasible mask, itself a hitting set of every core (a mask
 * hitting no group of a core keeps that core's contradiction intact). The
 * solve therefore ends in one of two proofs: a strictly better hitting set
 * whose score is the exact minimum over the cores, or the certificate that no
 * hitting set beats the incumbent — which, because every feasible mask is a
 * hitting set, certifies the incumbent as weight-optimal, reciprocal-positive
 * optima included.
 */
function solveMinimumHittingSet(
  graph: DenseConstraintGraph,
  cores: readonly (readonly number[])[],
  incumbent: readonly [number, number],
  budget: { nodes: number },
): HittingSetSolution {
  const chosen = new Uint8Array(graph.groupCount);
  const banned = new Uint8Array(graph.groupCount);
  const marker = new Uint8Array(graph.groupCount);
  let bestScore: readonly [number, number] | undefined;
  let bestMask: Uint8Array | undefined;
  let exhausted = false;

  // Admissible primary-weight bound on completing the current partial set:
  // greedily count still-unhit cores whose selectable groups are pairwise
  // disjoint, each contributing its cheapest selectable group. A core with no
  // selectable group at all is unhittable in this subtree.
  const remainingBound = (): number | undefined => {
    let bound = 0;
    marker.fill(0);
    for (const core of cores) {
      let hit = false;
      let overlaps = false;
      let minimum = Number.POSITIVE_INFINITY;
      for (const group of core) {
        if (chosen[group]) {
          hit = true;
          break;
        }
        if (banned[group]) continue;
        if (marker[group]) overlaps = true;
        minimum = Math.min(minimum, graph.groupSourceCount[group]);
      }
      if (hit) continue;
      if (!Number.isFinite(minimum)) return undefined;
      if (overlaps) continue;
      bound += minimum;
      for (const group of core) {
        if (!banned[group]) marker[group] = 1;
      }
    }
    return bound;
  };

  const descend = (primary: number, secondary: number): void => {
    if (exhausted) return;
    if (budget.nodes <= 0) {
      exhausted = true;
      return;
    }
    budget.nodes -= 1;
    const bound = remainingBound();
    if (bound === undefined) return;
    const target = bestScore ?? incumbent;
    if (primary + bound > target[0] ||
      (primary + bound === target[0] && secondary >= target[1])) return;
    let branchCore: readonly number[] | undefined;
    for (const core of cores) {
      let hit = false;
      for (const group of core) {
        if (chosen[group]) {
          hit = true;
          break;
        }
      }
      if (!hit) {
        branchCore = core;
        break;
      }
    }
    if (!branchCore) {
      // Every core is hit, and the entry prune already guaranteed strict
      // lexicographic improvement over the running target.
      bestScore = [primary, secondary];
      bestMask = chosen.slice();
      return;
    }
    // Standard symmetry breaking: after the branch containing a group is
    // exhausted, later branches of this core exclude it, so no group subset is
    // enumerated twice.
    const bannedHere: number[] = [];
    for (const group of branchCore) {
      if (banned[group]) continue;
      chosen[group] = 1;
      descend(
        primary + graph.groupSourceCount[group],
        secondary + graph.groupReciprocalCount[group],
      );
      chosen[group] = 0;
      if (exhausted) break;
      // Every completion below this node scores at least [primary + bound,
      // secondary]; once the running best sits exactly on that floor, no
      // sibling branch can strictly beat it. On wide cores this collapses the
      // whole fan to its first satisfying branch.
      if (bestScore && bestScore[0] === primary + bound && bestScore[1] === secondary) break;
      banned[group] = 1;
      bannedHere.push(group);
    }
    for (const group of bannedHere) banned[group] = 0;
  };
  descend(0, 0);
  if (exhausted) return { complete: false };
  if (!bestMask || !bestScore) return { complete: true };
  return { complete: true, mask: bestMask, score: bestScore };
}

function randomGenerator(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    return (state >>> 0) / 0x1_0000_0000;
  };
}

function removedScore(graph: DenseConstraintGraph, removedGroups: Uint8Array): readonly [number, number] {
  let total = 0;
  let reciprocal = 0;
  for (let group = 0; group < graph.groupCount; group += 1) {
    if (!removedGroups[group]) continue;
    total += graph.groupSourceCount[group];
    reciprocal += graph.groupReciprocalCount[group];
  }
  return [total, reciprocal];
}

function constraintMaskHash(mask: Uint8Array): number {
  let hash = 0x811c9dc5;
  for (let group = 0; group < mask.length; group += 1) {
    if (mask[group]) hash = Math.imul(hash ^ group, 0x01000193) >>> 0;
  }
  return hash;
}

function sameConstraintMask(left: Uint8Array, right: Uint8Array): boolean {
  if (left.length !== right.length) return false;
  for (let group = 0; group < left.length; group += 1) {
    if (left[group] !== right[group]) return false;
  }
  return true;
}

function expandGroupMask(graph: DenseConstraintGraph, removedGroups: Uint8Array): Uint8Array {
  return Uint8Array.from(graph.groupOfEdge, (group) => removedGroups[group] ? 1 : 0);
}

function sourceMaskToGroupMask(graph: DenseConstraintGraph, removedSources: Uint8Array): Uint8Array {
  const removedCounts = new Int32Array(graph.groupCount);
  for (let edge = 0; edge < graph.edgeCount; edge += 1) {
    if (removedSources[edge]) removedCounts[graph.groupOfEdge[edge]] += 1;
  }
  return Uint8Array.from(removedCounts, (count, group) =>
    count === graph.groupSourceCount[group] ? 1 : 0
  );
}

function sourceIndexesForGroups(
  graph: DenseConstraintGraph,
  groups: readonly number[],
): number[] {
  const selected = new Uint8Array(graph.groupCount);
  for (const group of groups) selected[group] = 1;
  const result: number[] = [];
  for (let edge = 0; edge < graph.edgeCount; edge += 1) {
    if (selected[graph.groupOfEdge[edge]]) result.push(graph.sourceIndexes[edge]);
  }
  return result;
}

/**
 * Infer the least canonical relaxation mask which admits the supplied rays.
 * Grouping makes reciprocal/duplicate source edges atomic, so one violated
 * member relaxes the complete feasibility-equivalent relation.
 */
function removedGroupsForPositions(
  graph: DenseConstraintGraph,
  positions: ReadonlyMap<string, GridPosition>,
): Uint8Array {
  const violated = new Set(directionalViolationEdges(positions, graph.sourceEdges));
  const removed = new Uint8Array(graph.groupCount);
  for (let edge = 0; edge < graph.edgeCount; edge += 1) {
    if (violated.has(graph.sourceEdges[edge])) removed[graph.groupOfEdge[edge]] = 1;
  }
  return removed;
}

function betterScore(a: readonly number[], b: readonly number[] | undefined): boolean {
  return !b || a[0] < b[0] || (a[0] === b[0] && a[1] < b[1]);
}

function emptyConstraintWorkStats(): ConstraintRepairWorkStats {
  return {
    rawIncumbents: 0,
    softIncumbents: 0,
    distinctLayouts: 0,
    maskDiversifications: 0,
    separatorStates: 0,
    separatorBranches: 0,
    separatorCyclePrunes: 0,
  };
}

function constraintSearch(
  graph: DenseConstraintGraph,
  options: ConstraintRepairOptions,
  standardPositions: ReadonlyMap<string, GridPosition>,
  runtime: SearchRuntimeOptions = {},
): ConstraintResult<ConstraintSearchResult> {
  const now = runtime.now ?? (() => performance.now());
  const started = now();
  const requestedDuration = options.maxDurationMs ?? DEFAULT_MAX_DURATION_MS;
  const duration = Number.isFinite(requestedDuration)
    ? Math.max(0, requestedDuration)
    : requestedDuration === Number.POSITIVE_INFINITY
    ? Number.POSITIVE_INFINITY
    : 0;
  const deadline = started + duration;
  const requestedRestarts = Math.floor(options.maxRestarts ?? DEFAULT_MAX_RESTARTS);
  const maximumRestarts = Number.isFinite(requestedRestarts)
    ? Math.max(1, requestedRestarts)
    : requestedRestarts === Number.POSITIVE_INFINITY
    ? Number.POSITIVE_INFINITY
    : DEFAULT_MAX_RESTARTS;
  const requestedMasks = Math.floor(options.maxMaskDiversifications ?? DEFAULT_MAX_MASK_DIVERSIFICATIONS);
  const maximumMasks = Number.isFinite(requestedMasks)
    ? Math.max(1, requestedMasks)
    : requestedMasks === Number.POSITIVE_INFINITY
    ? Number.POSITIVE_INFINITY
    : DEFAULT_MAX_MASK_DIVERSIFICATIONS;
  const sourceEdgeIndex = new Map(graph.sourceEdges.map((edge, index) => [edge, index]));
  const initial = new Uint8Array(graph.groupCount);
  for (const edge of directionalViolationEdges(standardPositions, graph.sourceEdges)) {
    const index = sourceEdgeIndex.get(edge);
    if (index === undefined) return failure("analysis");
    initial[graph.groupOfEdge[index]] = 1;
  }
  const initialScore = removedScore(graph, initial);
  const initialRemovedGroups = initial.reduce((total, value) => total + (value ? 1 : 0), 0);
  const maximumPerRestart = 2 * Math.min(graph.groupCount, initialRemovedGroups + 1) + 3;
  const defaultMaximumChecks = Math.min(
    Number.MAX_SAFE_INTEGER,
    1 + graph.groupCount + 1 + initialRemovedGroups + maximumRestarts * maximumPerRestart,
  );
  const maximumFeasibilityChecks = runtime.maximumFeasibilityChecks === undefined
    ? defaultMaximumChecks
    : Math.max(1, Math.floor(runtime.maximumFeasibilityChecks));
  let feasibilityChecks = 0;
  let attemptedRestarts = 0;
  let lastProgressAt = started;
  let budgetCutoff: ConstraintRepairReport["cutoff"] | undefined;
  let stopRequested = false;
  const publishMask = (removed: Uint8Array): void => {
    if (runtime.mask?.(removed.slice(), {
      restarts: attemptedRestarts,
      feasibilityChecks,
      elapsedMs: Math.max(0, now() - started),
    }) === false) stopRequested = true;
  };
  const timeExpired = (): boolean => {
    if (now() < deadline) return false;
    budgetCutoff = "time";
    return true;
  };
  const check = (
    removed: Uint8Array,
    includeState = false,
  ): ConstraintResult<AnalysisResult> => {
    if (timeExpired()) return failure("time");
    if (feasibilityChecks >= maximumFeasibilityChecks) {
      budgetCutoff ??= "restarts";
      return failure("work");
    }
    feasibilityChecks += 1;
    if ((feasibilityChecks & SEARCH_PROGRESS_CHECK_MASK) === 0) {
      const progressAt = now();
      if (progressAt - lastProgressAt >= PROGRESS_INTERVAL_MS) {
        lastProgressAt = progressAt;
        runtime.progress?.({
          ...emptyConstraintWorkStats(),
          phase: "search",
          restarts: attemptedRestarts,
          feasibilityChecks,
          layoutsConsidered: 0,
          compactionAttempts: 0,
          elapsedMs: Math.max(0, progressAt - started),
        });
      }
    }
    const analyzed = analyzeConstraints(graph, removed, includeState, timeExpired);
    if (!analyzed.ok && analyzed.reason === "time") budgetCutoff = "time";
    return analyzed;
  };

  const initialCheck = check(initial);
  if (!initialCheck.ok) return initialCheck;
  if (initialCheck.value.conflict) return failure("analysis");
  publishMask(initial);
  let best = initial.slice();
  let bestScore = initialScore;
  interface RetainedMask {
    hash: number;
    removed: Uint8Array;
    score: readonly [number, number];
    ordinal: number;
  }
  const retainedMasks: RetainedMask[] = [];
  const retainedMaskBuckets = new Map<number, RetainedMask[]>();
  let nextMaskOrdinal = 0;
  const rememberMask = (removed: Uint8Array): void => {
    const hash = constraintMaskHash(removed);
    if (retainedMaskBuckets.get(hash)?.some((entry) =>
      sameConstraintMask(entry.removed, removed)
    )) return;
    const entry: RetainedMask = {
      hash,
      removed: removed.slice(),
      score: removedScore(graph, removed),
      ordinal: nextMaskOrdinal++,
    };
    const addEntry = (): void => {
      retainedMasks.push(entry);
      const bucket = retainedMaskBuckets.get(hash);
      if (bucket) bucket.push(entry);
      else retainedMaskBuckets.set(hash, [entry]);
    };
    if (retainedMasks.length < maximumMasks) {
      addEntry();
      return;
    }
    let worstIndex = -1;
    for (let index = 0; index < retainedMasks.length; index += 1) {
      const candidate = retainedMasks[index];
      const worst = worstIndex < 0 ? undefined : retainedMasks[worstIndex];
      if (!worst || candidate.score[0] > worst.score[0] ||
        candidate.score[0] === worst.score[0] && candidate.score[1] > worst.score[1] ||
        candidate.score[0] === worst.score[0] && candidate.score[1] === worst.score[1] &&
          candidate.ordinal > worst.ordinal) worstIndex = index;
    }
    const worst = worstIndex < 0 ? undefined : retainedMasks[worstIndex];
    if (worst && (entry.score[0] < worst.score[0] ||
      entry.score[0] === worst.score[0] && entry.score[1] < worst.score[1])) {
      const worstBucket = retainedMaskBuckets.get(worst.hash) as RetainedMask[];
      const bucketIndex = worstBucket.indexOf(worst);
      if (bucketIndex >= 0) worstBucket.splice(bucketIndex, 1);
      if (worstBucket.length === 0) retainedMaskBuckets.delete(worst.hash);
      retainedMasks[worstIndex] = entry;
      const entryBucket = retainedMaskBuckets.get(hash);
      if (entryBucket) entryBucket.push(entry);
      else retainedMaskBuckets.set(hash, [entry]);
    }
  };
  rememberMask(initial);
  let lowerBound = 0;
  const finish = (optimal: boolean): ConstraintResult<ConstraintSearchResult> => {
    rememberMask(best);
    const masks = [...retainedMasks]
      .sort((a, b) =>
        (sameConstraintMask(a.removed, best) ? -1 : sameConstraintMask(b.removed, best) ? 1 : 0) ||
        a.score[0] - b.score[0] || a.score[1] - b.score[1] || a.ordinal - b.ordinal
      )
      .map((entry) => entry.removed);
    return success({
      removed: best,
      masks,
      score: bestScore,
      lowerBound,
      optimal,
      cutoff: optimal ? "none" : budgetCutoff ?? "restarts",
      restarts: attemptedRestarts,
      feasibilityChecks,
      elapsedMs: Math.max(0, now() - started),
    });
  };
  // A feasible empty relaxation cannot be beaten: certify it before spending
  // any bound work, and independently of any downstream stop request — the
  // proof is already in hand.
  if (bestScore[0] === 0 && bestScore[1] === 0) return finish(true);
  // A streamed mask callback may already have found a perfect geometric
  // incumbent (or exhausted a caller-owned downstream budget). Do not spend
  // certification or restart work after it asks the master search to stop.
  if (stopRequested) return finish(false);
  const seeded = collectDisjointConflictCores(graph, check);
  if (!seeded.ok) {
    if (seeded.reason === "time" || seeded.reason === "work") return finish(false);
    return seeded;
  }
  const cores = seeded.value.cores;
  lowerBound = seeded.value.lowerBound;
  if (stopRequested) return finish(false);
  if (bestScore[0] === lowerBound && bestScore[1] === 0) return finish(true);
  if (!seeded.value.complete) return finish(false);

  // The initial feasible state removes every directionally-invalid canonical
  // relation. Restore each removed group once in a stable objective-aware
  // order before the exact hitting-set loop: the cheaper incumbent tightens
  // its branch and bound, and the anytime geometry lane receives a better
  // early mask. A failed restore is reverted; the incumbent remains feasible
  // throughout.
  const restorationOrder = Array.from(initial, (_value, group) => group)
    .filter((group) => initial[group] !== 0)
    .sort((a, b) => graph.groupReciprocalCount[b] - graph.groupReciprocalCount[a] || a - b);
  for (const group of restorationOrder) {
    initial[group] = 0;
    const restored = check(initial);
    if (!restored.ok) {
      initial[group] = 1;
      if (restored.reason === "time" || restored.reason === "work") return finish(false);
      return restored;
    }
    if (restored.value.conflict) initial[group] = 1;
  }
  rememberMask(initial);
  publishMask(initial);
  const restoredScore = removedScore(graph, initial);
  if (betterScore(restoredScore, bestScore)) {
    best = initial.slice();
    bestScore = restoredScore;
    lastProgressAt = now();
    runtime.progress?.({
      ...emptyConstraintWorkStats(),
      phase: "search",
      restarts: attemptedRestarts,
      feasibilityChecks,
      layoutsConsidered: 0,
      compactionAttempts: 0,
      elapsedMs: Math.max(0, lastProgressAt - started),
      bestQuality: undefined,
    });
  }
  if (stopRequested) return finish(false);
  if (bestScore[0] === lowerBound && bestScore[1] === 0) return finish(true);

  // Primary strategy: MaxHS-style implicit hitting set. Alternate an exact
  // min-weight hitting set over the accumulated cores with one feasibility
  // check of that set as a removal mask. A feasible exact minimum is optimal
  // outright; an infeasible one contributes a new core (none of whose groups
  // it removed) and the iteration repeats. Termination within the ceilings is
  // the normal outcome; hitting a ceiling falls back to randomized restarts
  // with whatever bound the completed solves proved.
  const hittingSetBudget = {
    nodes: runtime.maximumHittingSetNodes === undefined
      ? MAX_HITTING_SET_DFS_NODES
      : Math.max(0, Math.floor(runtime.maximumHittingSetNodes)),
  };
  for (let iteration = 0; iteration < MAX_HITTING_SET_ITERATIONS; iteration += 1) {
    const solved = solveMinimumHittingSet(graph, cores, bestScore, hittingSetBudget);
    if (!solved.complete) break;
    if (!solved.mask || !solved.score) {
      // No hitting set beats the incumbent, and every feasible mask must hit
      // every accumulated core: the incumbent is weight-optimal.
      lowerBound = Math.max(lowerBound, bestScore[0]);
      return finish(true);
    }
    lowerBound = Math.max(lowerBound, solved.score[0]);
    const checked = check(solved.mask);
    if (!checked.ok) {
      if (checked.reason === "time" || checked.reason === "work") return finish(false);
      return checked;
    }
    const conflict = checked.value.conflict;
    if (!conflict) {
      best = solved.mask.slice();
      bestScore = solved.score;
      rememberMask(best);
      publishMask(best);
      lastProgressAt = now();
      runtime.progress?.({
        ...emptyConstraintWorkStats(),
        phase: "search",
        restarts: attemptedRestarts,
        feasibilityChecks,
        layoutsConsidered: 0,
        compactionAttempts: 0,
        elapsedMs: Math.max(0, lastProgressAt - started),
      });
      // The exact hitting-set minimum is itself feasible: weight-optimal
      // regardless of any downstream stop request.
      return finish(true);
    }
    if (conflict.length === 0) return failure("analysis");
    cores.push(conflict);
  }

  // Fallback: seeded randomized conflict-driven restarts, reachable only when
  // conflict accumulation exhausted a hitting-set ceiling.
  const random = randomGenerator(RANDOM_SEED);
  for (let restarts = 0; restarts < maximumRestarts; restarts += 1) {
    if (timeExpired()) return finish(false);
    attemptedRestarts += 1;
    const removed = new Uint8Array(graph.groupCount);
    let removedWeight = 0;
    let restartWork = 0;
    const maximumRestartWork = graph.groupCount * 6 + 16;
    const continueRestart = (): boolean => {
      restartWork += 1;
      if (restartWork > maximumRestartWork) {
        budgetCutoff ??= "restarts";
        return false;
      }
      return (restartWork & 0x3ff) !== 0 || !timeExpired();
    };
    for (;;) {
      if (!continueRestart()) return finish(false);
      const checked = check(removed);
      if (!checked.ok) {
        if (checked.reason === "time" || checked.reason === "work") return finish(false);
        return checked;
      }
      const conflict = checked.value.conflict;
      if (!conflict) break;
      if (conflict.length === 0) return failure("analysis");
      const oneWay = conflict.filter((group) => graph.groupReciprocalCount[group] === 0);
      const choices = oneWay.length > 0 && random() < 0.85 ? oneWay : conflict;
      const selected = choices[Math.floor(random() * choices.length)];
      if (!removed[selected]) {
        removed[selected] = 1;
        removedWeight += graph.groupSourceCount[selected];
      }
      if (removedWeight > bestScore[0]) break;
    }
    const order: number[] = [];
    for (let group = 0; group < graph.groupCount; group += 1) {
      if (!continueRestart()) return finish(false);
      if (removed[group]) order.push(group);
    }
    for (let index = order.length - 1; index > 0; index -= 1) {
      if (!continueRestart()) return finish(false);
      const swap = Math.floor(random() * (index + 1));
      [order[index], order[swap]] = [order[swap], order[index]];
    }
    for (const group of order) {
      if (!continueRestart()) return finish(false);
      removed[group] = 0;
      const checked = check(removed);
      if (!checked.ok) {
        removed[group] = 1;
        if (checked.reason === "time" || checked.reason === "work") return finish(false);
        return checked;
      }
      if (checked.value.conflict) removed[group] = 1;
    }
    const finalCheck = check(removed);
    if (!finalCheck.ok) {
      if (finalCheck.reason === "time" || finalCheck.reason === "work") return finish(false);
      return finalCheck;
    }
    if (finalCheck.value.conflict) continue;
    rememberMask(removed);
    publishMask(removed);
    const score = removedScore(graph, removed);
    if (betterScore(score, bestScore)) {
      best = removed.slice();
      bestScore = score;
      lastProgressAt = now();
      runtime.progress?.({
        ...emptyConstraintWorkStats(),
        phase: "search",
        restarts: attemptedRestarts,
        feasibilityChecks,
        layoutsConsidered: 0,
        compactionAttempts: 0,
        elapsedMs: Math.max(0, now() - started),
      });
    }
    if (stopRequested) return finish(false);
    if (bestScore[0] === lowerBound && bestScore[1] === 0) break;
  }
  const optimal = bestScore[0] === lowerBound && bestScore[1] === 0;
  if (!optimal && timeExpired()) budgetCutoff = "time";
  return finish(optimal);
}

function median(values: number[]): number {
  values.sort((a, b) => a - b);
  return values[Math.floor(values.length / 2)];
}

interface ConstraintCompactionRuntime {
  maximumStates?: number;
  shouldCancel?: () => boolean;
  score?: (positions: ReadonlyMap<string, GridPosition>) => LayoutQuality;
  onIncumbent?: (
    positions: Map<string, GridPosition>,
    quality: Readonly<LayoutQuality>,
  ) => void;
  onProgress?: () => void;
  /** Soft-defect relation groups eligible for an equal-primary mask swap. */
  onDiversification?: (relationGroups: readonly number[]) => void;
  onFinish?: (status: {
    completed: boolean;
    cancelled: boolean;
    exhausted: boolean;
  }) => void;
  workStats?: ConstraintRepairWorkStats;
}

function hardValidCompactionPositions(
  graph: DenseConstraintGraph,
  removedGroups: Uint8Array,
  positions: readonly (readonly [number, number, number])[],
  fixedIds: ReadonlySet<string>,
): boolean {
  for (let node = 0; node < graph.nodeCount; node += 1) {
    if (!graph.levelCrossingReachable[node] && positions[node][2] !== graph.positions[2][node]) {
      return false;
    }
  }
  for (const id of fixedIds) {
    const node = graph.indexById.get(id);
    if (node === undefined) continue;
    if (positions[node][0] !== graph.positions[0][node] ||
      positions[node][1] !== graph.positions[1][node] ||
      positions[node][2] !== graph.positions[2][node]) return false;
  }
  for (let group = 0; group < graph.groupCount; group += 1) {
    const axis = graph.groupAxis[group];
    const low = positions[graph.groupFrom[group]];
    const high = positions[graph.groupTo[group]];
    // A relaxed planar ray may move or reorder its endpoints in x/y, but it
    // never permits either endpoint to leave their shared map level.
    if (axis !== 2 && low[2] !== high[2]) return false;
    if (removedGroups[group]) continue;
    if (high[axis] - low[axis] < graph.groupStep[group]) return false;
    for (let perpendicular = 0; perpendicular < 3; perpendicular += 1) {
      if (perpendicular !== axis && low[perpendicular] !== high[perpendicular]) return false;
    }
  }
  return true;
}

function hardValidLayoutPositions(
  graph: DenseConstraintGraph,
  removedGroups: Uint8Array,
  positions: ReadonlyMap<string, GridPosition>,
  fixedIds: ReadonlySet<string>,
): boolean {
  if (positions.size !== graph.nodeCount) return false;
  const occupied = new Set<string>();
  const dense: [number, number, number][] = [];
  for (let node = 0; node < graph.nodeCount; node += 1) {
    const position = positions.get(graph.ids[node]);
    if (!position || !Number.isSafeInteger(position.x) ||
      !Number.isSafeInteger(position.y) || !Number.isSafeInteger(position.level)) return false;
    const integral: [number, number, number] = [
      position.x,
      position.y,
      position.level,
    ];
    const key = integral.join(",");
    if (occupied.has(key)) return false;
    occupied.add(key);
    dense.push(integral);
  }
  return hardValidCompactionPositions(graph, removedGroups, dense, fixedIds);
}

/** Undefined reports a cancellation observed mid-check, never an admission verdict. */
function createConstraintSeparatorAdmission(nodeCount: number): (
  alternatives: readonly ConstraintExtensionAlternative[],
  outgoing: readonly (readonly (readonly number[])[])[],
  shouldCancel: () => boolean,
) => boolean | undefined {
  const reachSeen = new Int32Array(nodeCount);
  const reachStack: number[] = [];
  let reachStamp = 0;
  const pathExists = (
    outgoing: readonly (readonly number[])[],
    from: number,
    target: number,
    shouldCancel: () => boolean,
  ): boolean | undefined => {
    if (from === target) return true;
    reachStamp += 1;
    if (reachStamp === 0x7fffffff) {
      reachSeen.fill(0);
      reachStamp = 1;
    }
    const stamp = reachStamp;
    reachStack.length = 0;
    reachStack.push(from);
    reachSeen[from] = stamp;
    let work = 0;
    while (reachStack.length > 0) {
      if ((work++ & 0x3ff) === 0 && shouldCancel()) return undefined;
      const node = reachStack.pop() as number;
      for (const next of outgoing[node]) {
        if (next === target) return true;
        if (reachSeen[next] === stamp) continue;
        reachSeen[next] = stamp;
        reachStack.push(next);
      }
    }
    reachStack.length = 0;
    return false;
  };
  return (alternatives, outgoing, shouldCancel): boolean | undefined => {
    for (const alternative of alternatives) {
      // Production geometric defects currently use one atomic precedence arc.
      // Retain the generic loop so later multi-arc defects remain conservative.
      let changed = false;
      let admissible = true;
      for (const arc of alternative.arcs) {
        const reverse = pathExists(outgoing[arc.axis], arc.to, arc.from, shouldCancel);
        if (reverse === undefined) return undefined;
        if (reverse) {
          admissible = false;
          break;
        }
        const implied = pathExists(outgoing[arc.axis], arc.from, arc.to, shouldCancel);
        if (implied === undefined) return undefined;
        if (!implied) changed = true;
      }
      if (admissible && changed) return true;
    }
    return false;
  };
}

/** Distinct from an inadmissible defect: a cancellation observed during admission. */
const DEFECT_SCAN_CANCELLED = "cancelled";

function firstAdmissibleConstraintDefect<T>(
  candidates: Iterable<T>,
  admit: (candidate: T) => ConstraintExtensionDefect | typeof DEFECT_SCAN_CANCELLED | undefined,
): ConstraintExtensionDefect | typeof DEFECT_SCAN_CANCELLED | undefined {
  for (const candidate of candidates) {
    const defect = admit(candidate);
    if (defect) return defect;
  }
  return undefined;
}

function compactConstraints(
  graph: DenseConstraintGraph,
  removed: Uint8Array,
  fixedIds: ReadonlySet<string> = new Set(),
  runtime: ConstraintCompactionRuntime = {},
): ConstraintResult<Map<string, GridPosition>> {
  const analyzed = analyzeConstraints(graph, removed, true);
  if (!analyzed.ok) return analyzed;
  const state = analyzed.value.state;
  if (!state) return failure("compaction");

  // Difference-constraint ranks intentionally minimize slack. Multiple fixed
  // anchors can require different offsets inside one retained component (for
  // example A=0, B=5 with A→B). Preserve the supplied complete geometry as a
  // hard-valid incumbent before attempting that optional compaction.
  const suppliedPositions = new Map(graph.ids.map((id, node) => [id, {
    x: graph.positions[0][node],
    y: graph.positions[1][node],
    level: graph.positions[2][node],
  }]));
  const suppliedHardValid = hardValidLayoutPositions(
    graph,
    removed,
    suppliedPositions,
    fixedIds,
  );
  if (suppliedHardValid) {
    const suppliedQuality = runtime.score
      ? runtime.score(suppliedPositions)
      : measureIntegralLayoutQuality(suppliedPositions, graph.sourceEdges);
    runtime.onIncumbent?.(suppliedPositions, suppliedQuality);
  }

  // Scratch reused by every `buildCoordinates` call across the separator
  // states of this one extension search (single-threaded and synchronous, so
  // exactly one state is ever in flight). Ownership rule: nothing built from
  // these buffers may outlive the `inspect` call that received it — the next
  // state overwrites everything. Whatever escapes into a published candidate
  // is copied at the escape point: candidate positions are copied scalar-by-
  // scalar into a fresh Map, and defect/alternative objects carry only
  // numbers. The graph's node count is fixed for the life of this search, so
  // these capacities never grow.
  const scratchNodeCount = graph.nodeCount;
  const emptyNodeLists = (): number[][] =>
    Array.from({ length: scratchNodeCount }, () => [] as number[]);
  const resetNodeLists = (lists: number[][]): number[][] => {
    for (let node = 0; node < scratchNodeCount; node += 1) lists[node].length = 0;
    return lists;
  };
  const scratchOutgoing: number[][][] = [emptyNodeLists(), emptyNodeLists(), emptyNodeLists()];
  const scratchIncoming = emptyNodeLists();
  const scratchExtensionOutgoing = emptyNodeLists();
  const scratchUndirected = emptyNodeLists();
  const scratchTargetValues = emptyNodeLists();
  // `indegree`, `target`, `coordinate`, and `fixedShift` follow write-before-
  // read discipline (roots are fully seeded before any read); `rank` needs its
  // zero fill, and the two flag arrays need their clears.
  const scratchIndegree = new Int32Array(scratchNodeCount);
  const scratchRank = new Int32Array(scratchNodeCount);
  const scratchTarget = new Int32Array(scratchNodeCount);
  const scratchCoordinate = [
    new Int32Array(scratchNodeCount),
    new Int32Array(scratchNodeCount),
    new Int32Array(scratchNodeCount),
  ];
  const scratchFixedShift = new Int32Array(scratchNodeCount);
  const scratchHasFixedShift = new Uint8Array(scratchNodeCount);
  // Slack-raise battery: in/out weights follow write-before-read discipline
  // (zeroed per live root before accumulation); the fixed-root flags need
  // their per-axis clear.
  const scratchInWeight = new Int32Array(scratchNodeCount);
  const scratchOutWeight = new Int32Array(scratchNodeCount);
  const scratchFixedRoot = new Uint8Array(scratchNodeCount);
  const scratchEntered = new Uint8Array(scratchNodeCount);
  const scratchReady = new DenseMinHeap();
  const scratchRootList: number[] = [];
  const scratchOrder: number[] = [];
  const scratchComponent: number[] = [];
  const scratchComponentQueue: number[] = [];
  const scratchShifts: number[] = [];
  const scratchPositions: [number, number, number][] = Array.from(
    { length: scratchNodeCount },
    () => [0, 0, 0],
  );

  const buildCoordinates = (
    extensionArcs: readonly ConstraintExtensionArc[],
  ): ConstraintResult<{
    positions: [number, number, number][];
    outgoing: number[][][];
  }> => {
    const extraArcs: [number, number][][] = [[], [], []];
    for (const arc of extensionArcs) extraArcs[arc.axis].push([arc.from, arc.to]);
    let work = 0;
    const stopped = (): boolean =>
      (work++ & 0x3ff) === 0 && runtime.shouldCancel?.() === true;
    for (let axis = 0; axis < 3; axis += 1) {
      const roots: Int32Array = state.roots[axis];
      const base: AxisGraph = state.graphs[axis];
      const outgoing = resetNodeLists(scratchOutgoing[axis]);
      const incoming = resetNodeLists(scratchIncoming);
      const extensionOutgoing = resetNodeLists(scratchExtensionOutgoing);
      for (let arc = 0; arc < base.length; arc += 1) {
        if (stopped()) return failure("time");
        const from = base.sourceRoot[arc];
        const to = base.targetRoot[arc];
        outgoing[from].push(to);
        incoming[to].push(from);
      }
      for (const [from, to] of extraArcs[axis]) {
        outgoing[from].push(to);
        incoming[to].push(from);
        extensionOutgoing[from].push(to);
      }
      const rootList = scratchRootList;
      rootList.length = 0;
      for (let node = 0; node < graph.nodeCount; node += 1) {
        if (roots[node] === node) rootList.push(node);
      }
      const indegree = scratchIndegree;
      for (const root of rootList) indegree[root] = incoming[root].length;
      const ready = scratchReady;
      ready.clear();
      for (const root of rootList) if (indegree[root] === 0) ready.push(root);
      const order = scratchOrder;
      order.length = 0;
      while (ready.length > 0) {
        if (stopped()) return failure("time");
        const root = ready.pop() as number;
        order.push(root);
        for (const target of outgoing[root]) {
          indegree[target] -= 1;
          if (indegree[target] === 0) ready.push(target);
        }
      }
      if (order.length !== rootList.length) return failure("compaction");

      const rank = scratchRank;
      rank.fill(0);
      for (const root of order) {
        if (stopped()) return failure("time");
        for (let arc = base.head[root]; arc !== -1; arc = base.next[arc]) {
          rank[base.to[arc]] = Math.max(rank[base.to[arc]], rank[root] + base.step[arc]);
        }
        for (const to of extensionOutgoing[root]) {
          rank[to] = Math.max(rank[to], rank[root] + 1);
        }
      }
      // Longest-path ranks pin every root at its earliest feasible coordinate,
      // which strands slack on retained relations whose source chain has no
      // other support: a neighborhood attached only from behind sits flush
      // against zero while its successors are pushed ahead by longer chains.
      // One reverse-topological raise pass compresses exactly that slack: a
      // root whose weighted in-degree does not exceed its weighted out-degree
      // rises to its tightest outgoing bound. Weights are retained source-edge
      // multiplicities — extension separators weigh nothing but still bound
      // the move — so each move changes weighted retained slack by
      // delta * (in - out), never positive. Bounds only relax for roots still
      // to be processed (a raise loosens only its predecessors' bounds), so
      // one sweep reaches the per-root fixed point. Roots holding fixed rooms
      // never move: their ranks anchor the component shifts, and moving one
      // could break multi-anchor shift agreement that longest-path ranks
      // satisfied.
      const inWeight = scratchInWeight;
      const outWeight = scratchOutWeight;
      for (const root of rootList) {
        inWeight[root] = 0;
        outWeight[root] = 0;
      }
      for (let arc = 0; arc < base.length; arc += 1) {
        const weight = graph.groupSourceCount[base.edge[arc]];
        outWeight[base.sourceRoot[arc]] += weight;
        inWeight[base.targetRoot[arc]] += weight;
      }
      const fixedRoot = scratchFixedRoot;
      fixedRoot.fill(0, 0, graph.nodeCount);
      for (const id of fixedIds) {
        const node = graph.indexById.get(id);
        if (node !== undefined) fixedRoot[roots[node]] = 1;
      }
      for (let index = order.length - 1; index >= 0; index -= 1) {
        if (stopped()) return failure("time");
        const root = order[index];
        if (fixedRoot[root] || inWeight[root] > outWeight[root]) continue;
        let bound = Number.MAX_SAFE_INTEGER;
        for (let arc = base.head[root]; arc !== -1; arc = base.next[arc]) {
          bound = Math.min(bound, rank[base.to[arc]] - base.step[arc]);
        }
        for (const to of extensionOutgoing[root]) {
          bound = Math.min(bound, rank[to] - 1);
        }
        if (bound !== Number.MAX_SAFE_INTEGER && bound > rank[root]) rank[root] = bound;
      }
      const undirected = resetNodeLists(scratchUndirected);
      for (const root of rootList) {
        for (const target of outgoing[root]) {
          undirected[root].push(target);
          undirected[target].push(root);
        }
      }
      const targetValues = resetNodeLists(scratchTargetValues);
      for (let node = 0; node < graph.nodeCount; node += 1) {
        targetValues[roots[node]].push(graph.positions[axis][node]);
      }
      const target = scratchTarget;
      for (const root of rootList) target[root] = median(targetValues[root]);
      const coordinate = scratchCoordinate[axis];
      const fixedShift = scratchFixedShift;
      const hasFixedShift = scratchHasFixedShift;
      hasFixedShift.fill(0);
      for (let node = 0; node < graph.nodeCount; node += 1) {
        if (!fixedIds.has(graph.ids[node])) continue;
        const root = roots[node];
        const shift = graph.positions[axis][node] - rank[root];
        if (hasFixedShift[root] && fixedShift[root] !== shift) return failure("compaction");
        hasFixedShift[root] = 1;
        fixedShift[root] = shift;
      }
      const entered = scratchEntered;
      entered.fill(0);
      const component = scratchComponent;
      const queue = scratchComponentQueue;
      for (const start of rootList) {
        if (entered[start]) continue;
        component.length = 0;
        queue.length = 0;
        queue.push(start);
        let queueIndex = 0;
        entered[start] = 1;
        while (queueIndex < queue.length) {
          if (stopped()) return failure("time");
          const root = queue[queueIndex++];
          component.push(root);
          for (const neighbor of undirected[root]) {
            if (entered[neighbor]) continue;
            entered[neighbor] = 1;
            queue.push(neighbor);
          }
        }
        let anchoredShift: number | undefined;
        for (const root of component) {
          if (!hasFixedShift[root]) continue;
          if (anchoredShift !== undefined && anchoredShift !== fixedShift[root]) {
            return failure("compaction");
          }
          anchoredShift = fixedShift[root];
        }
        let shift: number;
        if (anchoredShift === undefined) {
          const shifts = scratchShifts;
          shifts.length = 0;
          for (const root of component) shifts.push(target[root] - rank[root]);
          shift = median(shifts);
        } else {
          shift = anchoredShift;
        }
        for (const root of component) coordinate[root] = rank[root] + shift;
      }
    }
    const positions = scratchPositions;
    for (let node = 0; node < graph.nodeCount; node += 1) {
      const triple = positions[node];
      triple[0] = scratchCoordinate[0][state.roots[0][node]];
      triple[1] = scratchCoordinate[1][state.roots[1][node]];
      triple[2] = scratchCoordinate[2][state.roots[2][node]];
    }
    return success({ positions, outgoing: scratchOutgoing });
  };

  const physicalEdges: { group: number; from: number; to: number; axis: number }[] = [];
  const physicalKeys = new Set<string>();
  for (let group = 0; group < graph.groupCount; group += 1) {
    if (removed[group] || graph.groupAxis[group] > 1) continue;
    const from = graph.groupFrom[group];
    const to = graph.groupTo[group];
    const low = Math.min(from, to);
    const high = Math.max(from, to);
    const key = `${low}:${high}:${graph.groupAxis[group]}`;
    if (physicalKeys.has(key)) continue;
    physicalKeys.add(key);
    physicalEdges.push({ group, from, to, axis: graph.groupAxis[group] });
  }

  const singleArc = (
    axis: number,
    from: number,
    to: number,
  ): ConstraintExtensionAlternative => ({
    arcs: [{ axis: axis as 0 | 1 | 2, from, to }],
  });
  const MAX_SOFT_DEFECT_SIGNATURES = 4_096;
  const seenSoftDefects = new Set<string>();
  const softDefectOrder: string[] = [];
  let nextSoftDefectEviction = 0;
  const softDefectSignature = (
    kind: number,
    values: readonly number[],
    extensionArcs: readonly ConstraintExtensionArc[],
  ): string => {
    let first = 0x811c9dc5;
    let second = 0x9e3779b9;
    const feed = (value: number): void => {
      first = Math.imul(first ^ value, 0x01000193) >>> 0;
      second = Math.imul(second ^ (value + 0x7f4a7c15), 0x85ebca6b) >>> 0;
    };
    feed(kind);
    for (const value of values) feed(value);
    const canonicalArcs = [...extensionArcs].sort((a, b) =>
      a.axis - b.axis || a.from - b.from || a.to - b.to
    );
    feed(canonicalArcs.length);
    for (const arc of canonicalArcs) {
      feed(arc.axis);
      feed(arc.from);
      feed(arc.to);
    }
    return `${first.toString(16).padStart(8, "0")}${second.toString(16).padStart(8, "0")}`;
  };
  const rememberSoftDefect = (signature: string): boolean => {
    if (seenSoftDefects.has(signature)) return false;
    seenSoftDefects.add(signature);
    if (softDefectOrder.length < MAX_SOFT_DEFECT_SIGNATURES) {
      softDefectOrder.push(signature);
    } else {
      seenSoftDefects.delete(softDefectOrder[nextSoftDefectEviction]);
      softDefectOrder[nextSoftDefectEviction] = signature;
      nextSoftDefectEviction = (nextSoftDefectEviction + 1) % MAX_SOFT_DEFECT_SIGNATURES;
    }
    return true;
  };
  const hasAdmissibleSeparator = createConstraintSeparatorAdmission(graph.nodeCount);
  // Per-state scan scratch, reset at each use inside `inspect` and never
  // escaping it: published alternatives are mapped into fresh objects at the
  // escape point, and blocker records hold only numbers.
  const scratchOccupied = new Map<string, number>();
  const scratchWeightedAlternatives: {
    penalty: number;
    axis: number;
    from: number;
    to: number;
  }[] = [];
  const scratchBlockers: { edge: number; node: number; atPort: boolean }[] = [];
  const inspect = ({
    extensionArcs,
    shouldCancel,
  }: {
    extensionArcs: readonly ConstraintExtensionArc[];
    shouldCancel: () => boolean;
  }): ConstraintExtensionInspection<Map<string, GridPosition>, LayoutQuality> => {
    const builtResult = buildCoordinates(extensionArcs);
    if (!builtResult.ok) {
      // A deadline observed while building geometry says nothing about this
      // state. Fabricating a conflict here would let a cut traversal drain the
      // stack and masquerade as an exhaustively completed search.
      if (builtResult.reason === "time") return { type: "cancelled" };
      return {
        type: "hard-conflict",
        conflict: { kind: builtResult.reason, alternatives: [] },
      };
    }
    const built = builtResult.value;
    const occupied = scratchOccupied;
    occupied.clear();
    let collision: readonly [number, number] | undefined;
    for (let node = 0; node < graph.nodeCount; node += 1) {
      const key = built.positions[node].join(",");
      const previous = occupied.get(key);
      if (previous !== undefined) {
        collision = [previous, node];
        break;
      }
      occupied.set(key, node);
    }
    const weightedAlternatives = scratchWeightedAlternatives;
    weightedAlternatives.length = 0;
    if (collision) {
      // Levels express map topology. Ordinary room collisions must be healed
      // in the visible x/y plane rather than inventing a new floor.
      for (let axis = 0; axis < 2; axis += 1) {
        const roots = state.roots[axis];
        const a = roots[collision[0]];
        const b = roots[collision[1]];
        if (a === b) continue;
        const preferred: readonly [number, number] = graph.positions[axis][collision[0]] <=
            graph.positions[axis][collision[1]]
          ? [a, b]
          : [b, a];
        for (const [from, to] of [preferred, [preferred[1], preferred[0]]] as const) {
          weightedAlternatives.push({
            axis,
            from,
            to,
            penalty: graph.positions[axis][collision[0]] === graph.positions[axis][collision[1]] ? 1 : 0,
          });
        }
      }
      weightedAlternatives.sort((a, b) =>
        a.penalty - b.penalty || a.axis - b.axis || a.from - b.from || a.to - b.to
      );
      return {
        type: "hard-conflict",
        conflict: {
          kind: "collision",
          alternatives: weightedAlternatives.map(({ axis, from, to }) => singleArc(axis, from, to)),
        },
      };
    }
    if (!hardValidCompactionPositions(graph, removed, built.positions, fixedIds)) {
      return { type: "hard-conflict", conflict: { kind: "hard-validity", alternatives: [] } };
    }
    // Escape point: the candidate must outlive this state (the extension core
    // retains its best incumbent), so scratch coordinates are copied scalar-by-
    // scalar into a fresh Map here.
    const positions = new Map<string, GridPosition>();
    for (let node = 0; node < graph.nodeCount; node += 1) {
      positions.set(graph.ids[node], {
        x: built.positions[node][0],
        y: built.positions[node][1],
        level: built.positions[node][2],
      });
    }
    const quality = runtime.score
      ? runtime.score(positions)
      : measureIntegralLayoutQuality(positions, graph.sourceEdges);
    runtime.onIncumbent?.(positions, quality);
    // The incumbent above is already published; a cancellation observed at any
    // later point of this inspection must surface as a cancellation so the
    // truncated defect scan can never pass for a fully explored state.
    if (shouldCancel()) return { type: "cancelled" };

    const admissibleDefect = (
      defect: ConstraintExtensionDefect,
      signatureValues: readonly number[],
      kind: number,
    ): ConstraintExtensionDefect | typeof DEFECT_SCAN_CANCELLED | undefined => {
      const admissible = hasAdmissibleSeparator(defect.alternatives, built.outgoing, shouldCancel);
      if (admissible === undefined) return DEFECT_SCAN_CANCELLED;
      if (!admissible) return undefined;
      const signature = softDefectSignature(kind, signatureValues, extensionArcs);
      return rememberSoftDefect(signature) ? defect : undefined;
    };
    const obstructionDefect = (
      obstruction: { edge: number; node: number; atPort: boolean },
    ): ConstraintExtensionDefect => {
      weightedAlternatives.length = 0;
      const edgeAxis = graph.groupAxis[obstruction.edge];
      const perpendicular = edgeAxis === 0 ? 1 : 0;
      const edgeRoot = state.roots[perpendicular][graph.groupFrom[obstruction.edge]];
      const blockerRoot = state.roots[perpendicular][obstruction.node];
      if (edgeRoot !== blockerRoot) {
        const edgeOriginal = graph.positions[perpendicular][graph.groupFrom[obstruction.edge]];
        const blockerOriginal = graph.positions[perpendicular][obstruction.node];
        const preferred: readonly [number, number] = edgeOriginal <= blockerOriginal
          ? [edgeRoot, blockerRoot]
          : [blockerRoot, edgeRoot];
        for (const [from, to] of [preferred, [preferred[1], preferred[0]]] as const) {
          weightedAlternatives.push({
            axis: perpendicular,
            from,
            to,
            penalty: edgeOriginal === blockerOriginal ? 1 : 0,
          });
        }
      }
      const roots = state.roots[edgeAxis];
      const blocker = roots[obstruction.node];
      const fromNode = graph.groupFrom[obstruction.edge];
      const toNode = graph.groupTo[obstruction.edge];
      const lowNode = built.positions[fromNode][edgeAxis] <= built.positions[toNode][edgeAxis]
        ? fromNode
        : toNode;
      const highNode = lowNode === fromNode ? toNode : fromNode;
      for (const [from, to] of [
        [blocker, roots[lowNode]],
        [roots[highNode], blocker],
      ] as const) {
        weightedAlternatives.push({ axis: edgeAxis, from, to, penalty: 2 });
      }
      weightedAlternatives.sort((a, b) =>
        a.penalty - b.penalty || a.axis - b.axis || a.from - b.from || a.to - b.to
      );
      return {
        kind: "obstruction",
        relationGroups: [obstruction.edge],
        alternatives: weightedAlternatives.map(({ axis, from, to }) => singleArc(axis, from, to)),
      };
    };

    // Scan every stable obstruction candidate until one has a separator which
    // is neither already implied nor cycle-closing. An unrepairable first
    // obstruction must not hide a later repairable defect in the same layout.
    for (let group = 0; group < graph.groupCount; group += 1) {
      if ((group & 0x3f) === 0 && shouldCancel()) return { type: "cancelled" };
      if (removed[group] || graph.groupAxis[group] > 1) continue;
      const fromNode = graph.groupFrom[group];
      const toNode = graph.groupTo[group];
      const from = built.positions[fromNode];
      const to = built.positions[toNode];
      if (from[2] !== to[2]) continue;
      const axis = graph.groupAxis[group];
      const perpendicular = axis === 0 ? 1 : 0;
      const minimum = Math.min(from[axis], to[axis]);
      const maximum = Math.max(from[axis], to[axis]);
      const blockers = scratchBlockers;
      blockers.length = 0;
      for (let node = 0; node < graph.nodeCount; node += 1) {
        if (node === fromNode || node === toNode) continue;
        const position = built.positions[node];
        if (position[2] !== from[2] || position[perpendicular] !== from[perpendicular] ||
          position[axis] <= minimum || position[axis] >= maximum) continue;
        blockers.push({
          edge: group,
          node,
          atPort: Math.abs(position[axis] - from[axis]) === 1 ||
            Math.abs(position[axis] - to[axis]) === 1,
        });
      }
      blockers.sort((a, b) => Number(b.atPort) - Number(a.atPort) ||
        (a.atPort ? a.node - b.node : b.node - a.node));
      const defect = firstAdmissibleConstraintDefect(blockers, (blocker) =>
        admissibleDefect(
          obstructionDefect(blocker),
          [blocker.edge, blocker.node, blocker.atPort ? 1 : 0],
          1,
        )
      );
      if (defect === DEFECT_SCAN_CANCELLED) return { type: "cancelled" };
      if (defect) {
        return { type: "candidate", candidate: positions, score: quality, softDefect: defect };
      }
    }

    for (let first = 0; first < physicalEdges.length; first += 1) {
      if ((first & 0x3f) === 0 && shouldCancel()) return { type: "cancelled" };
      for (let second = first + 1; second < physicalEdges.length; second += 1) {
        const a = physicalEdges[first];
        const b = physicalEdges[second];
        if (a.axis === b.axis || a.from === b.from || a.from === b.to ||
          a.to === b.from || a.to === b.to) continue;
        const horizontal = a.axis === 0 ? a : b;
        const vertical = a.axis === 1 ? a : b;
        const hFrom = built.positions[horizontal.from];
        const hTo = built.positions[horizontal.to];
        const vFrom = built.positions[vertical.from];
        const vTo = built.positions[vertical.to];
        if (hFrom[2] !== hTo[2] || hFrom[2] !== vFrom[2] || hFrom[2] !== vTo[2]) continue;
        const minimumX = Math.min(hFrom[0], hTo[0]);
        const maximumX = Math.max(hFrom[0], hTo[0]);
        const minimumY = Math.min(vFrom[1], vTo[1]);
        const maximumY = Math.max(vFrom[1], vTo[1]);
        if (vFrom[0] <= minimumX || vFrom[0] >= maximumX ||
          hFrom[1] <= minimumY || hFrom[1] >= maximumY) continue;
        weightedAlternatives.length = 0;
        const horizontalY = state.roots[1][horizontal.from];
        const verticalFromY = state.roots[1][vertical.from];
        const verticalToY = state.roots[1][vertical.to];
        const topY = built.positions[vertical.from][1] <= built.positions[vertical.to][1]
          ? verticalFromY
          : verticalToY;
        const bottomY = topY === verticalFromY ? verticalToY : verticalFromY;
        const verticalX = state.roots[0][vertical.from];
        const horizontalFromX = state.roots[0][horizontal.from];
        const horizontalToX = state.roots[0][horizontal.to];
        const leftX = built.positions[horizontal.from][0] <= built.positions[horizontal.to][0]
          ? horizontalFromX
          : horizontalToX;
        const rightX = leftX === horizontalFromX ? horizontalToX : horizontalFromX;
        for (const [axis, from, to] of [
          [1, horizontalY, topY],
          [1, bottomY, horizontalY],
          [0, verticalX, leftX],
          [0, rightX, verticalX],
        ] as const) {
          weightedAlternatives.push({ axis, from, to, penalty: 3 });
        }
        const defect = admissibleDefect({
          kind: "crossing",
          relationGroups: [horizontal.group, vertical.group],
          alternatives: weightedAlternatives.map(({ axis, from, to }) => singleArc(axis, from, to)),
        }, [horizontal.group, vertical.group], 2);
        if (defect === DEFECT_SCAN_CANCELLED) return { type: "cancelled" };
        if (defect) {
          return { type: "candidate", candidate: positions, score: quality, softDefect: defect };
        }
      }
    }
    return { type: "candidate", candidate: positions, score: quality };
  };

  const baseArcs: ConstraintExtensionArc[] = [];
  for (let axis = 0; axis < 3; axis += 1) {
    const base = state.graphs[axis];
    for (let arc = 0; arc < base.length; arc += 1) {
      baseArcs.push({
        axis: axis as 0 | 1 | 2,
        from: base.sourceRoot[arc],
        to: base.targetRoot[arc],
      });
    }
  }
  const initialStats = runtime.workStats ? { ...runtime.workStats } : undefined;
  const updateWorkStats = (stats: {
    states: number;
    branches: number;
    cyclePrunes: number;
  }): void => {
    if (!runtime.workStats || !initialStats) return;
    runtime.workStats.separatorStates = initialStats.separatorStates + stats.states;
    runtime.workStats.separatorBranches = initialStats.separatorBranches + stats.branches;
    runtime.workStats.separatorCyclePrunes = initialStats.separatorCyclePrunes + stats.cyclePrunes;
  };
  const result = searchConstraintExtensions<Map<string, GridPosition>, LayoutQuality>({
    axisNodeCounts: [graph.nodeCount, graph.nodeCount, graph.nodeCount],
    baseArcs,
    inspect: (context) => {
      if (runtime.workStats && initialStats) {
        runtime.workStats.separatorStates = initialStats.separatorStates + context.state;
        runtime.workStats.separatorBranches = initialStats.separatorBranches + context.branches;
        runtime.workStats.separatorCyclePrunes = initialStats.separatorCyclePrunes +
          context.cyclePrunes;
      }
      return inspect(context);
    },
    compareScores: compareLayoutQuality,
    maxExtensionStates: runtime.maximumStates,
    shouldCancel: runtime.shouldCancel,
    progressIntervalStates: 16,
    onProgress: (stats) => {
      updateWorkStats(stats);
      runtime.onProgress?.();
    },
    onEqualPrimaryDiversification: ({ reason, defect }) => {
      // Soft defects are heuristic geometry guidance: they may diversify the
      // next complete mask but can never prune this fixed-mask search or prove
      // infeasibility. Hard explanations remain confined to the generic core's
      // exhaustive root-conflict contract.
      if (reason === "soft-defect" && defect.relationGroups?.length) {
        runtime.onDiversification?.(defect.relationGroups);
      }
    },
  });
  updateWorkStats(result);
  runtime.onFinish?.({
    completed: result.completed,
    cancelled: result.cancelled,
    exhausted: result.exhausted,
  });
  if (result.best) return success(new Map(result.best));
  if (suppliedHardValid) return success(suppliedPositions);
  return failure(result.cancelled ? "time" : "compaction");
}

function samePosition(a: GridPosition, b: GridPosition): boolean {
  return a.x === b.x && a.y === b.y && a.level === b.level;
}

function recomputeMovedExisting(
  request: IntegralLayoutRequest,
  positions: ReadonlyMap<string, GridPosition>,
): ReadonlySet<string> {
  const result = new Set<string>();
  for (const resident of request.residents) {
    const after = positions.get(resident.id);
    if (after && !samePosition(integralPosition(resident.position), after)) result.add(resident.id);
  }
  return result;
}

/**
 * Two independent 32-bit FNV-style lanes fingerprint a complete position
 * assignment, the same technique the soft-defect signatures use. Coordinates
 * travel with the signature only for exact comparison against the retained
 * full-position holders (the winner and the polish frontier); the dedup
 * window stores nothing but the combined 64-bit key.
 */
interface ConstraintPositionSignature {
  hash: number;
  second: number;
  coordinates: Float64Array;
}

function constraintPositionSignature(
  graph: DenseConstraintGraph,
  positions: ReadonlyMap<string, GridPosition>,
): ConstraintPositionSignature | undefined {
  if (positions.size !== graph.nodeCount) return undefined;
  const coordinates = new Float64Array(graph.nodeCount * 3);
  let hash = 0x811c9dc5;
  let second = 0x9e3779b9;
  let cursor = 0;
  for (const id of graph.ids) {
    const position = positions.get(id);
    if (!position || !Number.isSafeInteger(position.x) ||
      !Number.isSafeInteger(position.y) || !Number.isSafeInteger(position.level)) return undefined;
    for (const value of [position.x, position.y, position.level]) {
      coordinates[cursor++] = value;
      const low = value | 0;
      const high = Math.floor(value / 0x1_0000_0000);
      hash = Math.imul(hash ^ low, 0x01000193) >>> 0;
      hash = Math.imul(hash ^ high, 0x01000193) >>> 0;
      second = Math.imul(second ^ (low + 0x7f4a7c15), 0x85ebca6b) >>> 0;
      second = Math.imul(second ^ (high + 0x7f4a7c15), 0x85ebca6b) >>> 0;
    }
  }
  return { hash, second, coordinates };
}

function constraintPositionSignatureKey(signature: ConstraintPositionSignature): string {
  return `${signature.hash.toString(16).padStart(8, "0")}${
    signature.second.toString(16).padStart(8, "0")}`;
}

function sameConstraintPositionSignature(
  left: ConstraintPositionSignature,
  right: ConstraintPositionSignature,
): boolean {
  if (left.hash !== right.hash || left.second !== right.second ||
    left.coordinates.length !== right.coordinates.length) return false;
  for (let index = 0; index < left.coordinates.length; index += 1) {
    if (left.coordinates[index] !== right.coordinates[index]) return false;
  }
  return true;
}

function beforeViolationCounts(
  request: IntegralLayoutRequest,
  standard: IntegralLayoutPlan,
): {
  beforeViolations: number;
  beforeRoutingViolations: number;
  beforeSettledViolations: number;
  standardSettledViolations: number;
  beforeSettledRoutingViolations: number;
  standardSettledRoutingViolations: number;
} {
  const residentIds = new Set(request.residents.map((resident) => resident.id));
  const settledEdges = request.edges.filter((edge) => residentIds.has(edge.from) && residentIds.has(edge.to));
  const before = new Map(request.residents.map((resident) => [resident.id, integralPosition(resident.position)]));
  return {
    beforeViolations: directionalViolationEdges(before, request.edges).length,
    beforeRoutingViolations: measureLayoutRoutingQuality(before, request.edges).routingViolations,
    beforeSettledViolations: directionalViolationEdges(before, settledEdges).length,
    standardSettledViolations: directionalViolationEdges(standard.positions, settledEdges).length,
    beforeSettledRoutingViolations: measureLayoutRoutingQuality(before, settledEdges).routingViolations,
    standardSettledRoutingViolations: measureLayoutRoutingQuality(
      standard.positions,
      settledEdges,
    ).routingViolations,
  };
}

interface ConstraintPolishProgress {
  repairStarted: number;
  deadline: number;
  maximumTournaments: number;
  maximumPasses: number;
  constraintLayoutsConsidered: number;
  compactionAttempts: number;
  restarts: number;
  feasibilityChecks: number;
  workStats: ConstraintRepairWorkStats;
  /** Reject optional polish maps which no longer satisfy the chosen hard mask. */
  acceptsPositions?: (positions: ReadonlyMap<string, GridPosition>) => boolean;
}

interface ConstraintPolishResult {
  plan: IntegralLayoutPlan;
  tournaments: number;
  passes: number;
  anchorsTried: number;
  improvements: number;
  fixedPoint: boolean;
  cutoff: "fixed-point" | "time" | "tournaments" | "passes" | "error";
  elapsedMs: number;
}

/** Private control-flow marker used to stop a finite nested planner cleanly. */
const POLISH_DEADLINE = Symbol("map-layout-polish-deadline");

/** Code-unit id order keeps model and trace ordering identical across ICU locales. */
function compareRoomIds(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function layoutModelFromPlan(
  request: IntegralLayoutRequest,
  plan: IntegralLayoutPlan,
): LayoutModel {
  const movableById = new Map(request.residents.map((resident) => [resident.id, resident.movable]));
  return {
    rooms: [...plan.positions]
      .sort(([a], [b]) => compareRoomIds(a, b))
      .map(([id, position]) => ({
        id,
        position,
        movable: movableById.get(id) ?? true,
      })),
    edges: request.edges,
  };
}

/**
 * Apply the same multi-anchor fixed-point tournament as `nf reflow` to the
 * constraint winner. A complete tournament which finds no strict public
 * quality improvement is the geometric fixed-point proof for this heuristic.
 */
function polishConstraintLayoutToFixedPoint(
  request: IntegralLayoutRequest,
  seed: IntegralLayoutPlan,
  trace: ((event: LayoutTraceEvent) => void) | undefined,
  progress: ConstraintPolishProgress,
  now: () => number = () => performance.now(),
): ConstraintPolishResult {
  const started = now();
  let winner = seed;
  let tournaments = 0;
  let passes = 0;
  let anchorsTried = 0;
  let improvements = 0;

  const publishProgress = (): void => trace?.({
    type: "constraint-progress",
    stage: "constraint-repair",
    phase: "polish",
    restarts: progress.restarts,
    feasibilityChecks: progress.feasibilityChecks,
    layoutsConsidered: progress.constraintLayoutsConsidered + passes,
    compactionAttempts: progress.compactionAttempts,
    elapsedMs: Math.max(0, now() - progress.repairStarted),
    bestQuality: winner.quality,
    ...progress.workStats,
  });
  const finish = (
    cutoff: ConstraintPolishResult["cutoff"],
    fixedPoint: boolean,
  ): ConstraintPolishResult => ({
    plan: winner,
    tournaments,
    passes,
    anchorsTried,
    improvements,
    fixedPoint,
    cutoff,
    elapsedMs: Math.max(0, now() - started),
  });
  const adopt = (
    _quality: IntegralLayoutPlan["quality"],
    positions: ReadonlyMap<string, GridPosition>,
  ): void => {
    if (progress.acceptsPositions && !progress.acceptsPositions(positions)) return;
    const quality = measureIntegralLayoutQuality(positions, request.edges);
    if (compareLayoutQuality(quality, winner.quality) <= 0) return;
    winner = {
      positions: new Map(positions),
      movedExisting: recomputeMovedExisting(request, positions),
      quality: { ...quality },
    };
    improvements += 1;
    trace?.({
      type: "constraint-improvement",
      stage: "constraint-repair",
      restarts: progress.restarts,
      feasibilityChecks: progress.feasibilityChecks,
      layoutsConsidered: progress.constraintLayoutsConsidered + passes,
      compactionAttempts: progress.compactionAttempts,
      ...progress.workStats,
      candidate: {
        quality: { ...winner.quality },
        movedExisting: [...winner.movedExisting].sort(),
        positions: [...winner.positions]
          .sort(([a], [b]) => compareRoomIds(a, b))
          .map(([id, position]) => ({ id, ...position })),
      },
    });
  };

  try {
    if (Number.isFinite(progress.deadline) && now() >= progress.deadline) {
      publishProgress();
      return finish("time", false);
    }
    for (;;) {
      // Charge only complete deterministic tournaments. Check before entering
      // the nested planner so a limit of N never begins tournament N + 1 and
      // the last complete best-so-far plan remains publishable.
      if (tournaments >= progress.maximumTournaments) {
        publishProgress();
        return finish("tournaments", false);
      }
      if (passes >= progress.maximumPasses) {
        publishProgress();
        return finish("passes", false);
      }
      const beforeTournament = winner;
      const passesBeforeTournament = passes;
      const planTournament = (): ReturnType<typeof planLayoutModel> => planLayoutModel(
        layoutModelFromPlan(request, beforeTournament),
        { type: "reflow", anchor: request.centerId },
        {
          effort: "thorough",
          allowExistingMoves: true,
          maxPlanningPasses: Number.isFinite(progress.maximumPasses)
            ? Math.max(1, progress.maximumPasses - passes)
            : Number.POSITIVE_INFINITY,
          trace: (event) => {
            // The nested planner only exposes synchronous trace callbacks, so
            // throw before inspecting/filtering an event once a finite polish
            // deadline has elapsed. The outer catch retains `winner`, which is
            // always a complete plan adopted before this sentinel was raised.
            if (Number.isFinite(progress.deadline) && now() >= progress.deadline) {
              throw POLISH_DEADLINE;
            }
            if (event.type !== "selection" || event.stage !== "final-selection") return;
            passes += 1;
            if (event.selected.positions) {
              adopt(
                event.selected.quality,
                new Map(event.selected.positions.map(({ id, x, y, level }) => [
                  id,
                  { x, y, level },
                ])),
              );
            }
            publishProgress();
          },
        },
      );
      const planned = progress.acceptsPositions
        ? withIntegralLayoutCandidateAdmission(progress.acceptsPositions, planTournament)
        : planTournament();
      if (Number.isFinite(progress.deadline) && now() >= progress.deadline) {
        throw POLISH_DEADLINE;
      }
      anchorsTried += planned.search?.anchorsTried.length ?? 1;
      const reportedPasses = planned.search?.planningPasses ?? 1;
      // Selection events normally charge every completed nested pass live.
      // Reconcile defensively in case a future planner omits those diagnostics.
      passes = Math.max(passes, passesBeforeTournament + reportedPasses);
      const plannedQuality = measureIntegralLayoutQuality(planned.positions, request.edges);
      // The returned tournament winner also carries its deterministic
      // movement-count tie-break, so prefer it when public quality ties the
      // best progressive candidate observed inside the tournament.
      if ((!progress.acceptsPositions || progress.acceptsPositions(planned.positions)) &&
        compareLayoutQuality(plannedQuality, winner.quality) >= 0) {
        winner = {
          positions: new Map(planned.positions),
          movedExisting: recomputeMovedExisting(request, planned.positions),
          quality: plannedQuality,
        };
      }
      if (planned.search?.completed === false) {
        publishProgress();
        return finish("passes", false);
      }
      tournaments += 1;
      if (compareLayoutQuality(winner.quality, beforeTournament.quality) <= 0) {
        publishProgress();
        return {
          plan: winner,
          tournaments,
          passes,
          anchorsTried,
          improvements,
          fixedPoint: true,
          cutoff: "fixed-point",
          elapsedMs: Math.max(0, now() - started),
        };
      }
      // Infinity is the NukeFire deep-search policy. Finite callers poll
      // cooperatively before nested trace events and at planner boundaries,
      // retaining the last complete winner when their deadline elapses.
      if (now() >= progress.deadline) {
        publishProgress();
        return finish("time", false);
      }
    }
  } catch (error) {
    if (error === POLISH_DEADLINE) {
      try {
        publishProgress();
      } catch {
        // Progress observers are request-local and cannot change the cutoff.
      }
      return finish("time", false);
    }
    // Constraint repair already produced a valid complete layout. A failure in
    // optional geometric polish must not discard that accepted best-so-far.
    return {
      plan: winner,
      tournaments,
      passes,
      anchorsTried,
      improvements,
      fixedPoint: false,
      cutoff: "error",
      elapsedMs: Math.max(0, now() - started),
    };
  }
}

function repairIntegralLayoutConstraintsWithRuntime(
  request: IntegralLayoutRequest,
  standard: IntegralLayoutPlan,
  options: ConstraintRepairOptions,
  trace?: (event: LayoutTraceEvent) => void,
  runtime: RepairRuntimeOptions = {},
): IntegralLayoutPlan {
  if (request.allowExistingMoves === false) {
    return standard;
  }
  const {
    beforeViolations,
    beforeRoutingViolations,
    beforeSettledViolations,
    standardSettledViolations,
    beforeSettledRoutingViolations,
    standardSettledRoutingViolations,
  } = beforeViolationCounts(request, standard);
  if (options.when === "settled-regression") {
    const directionalRegression = standardSettledViolations > beforeSettledViolations;
    const routingRegression = standardSettledViolations === beforeSettledViolations &&
      standardSettledRoutingViolations > beforeSettledRoutingViolations;
    if (!directionalRegression && !routingRegression) return standard;
  }
  if (options.when === "violation-regression") {
    const directionalRegression = standard.quality.cardinalRayViolations > beforeViolations;
    const routingRegression = standard.quality.cardinalRayViolations === beforeViolations &&
      standard.quality.routingViolations > beforeRoutingViolations;
    if (!directionalRegression && !routingRegression) return standard;
  }
  if (options.when !== "always" && standard.quality.cardinalRayViolations === 0 &&
    standard.quality.routingViolations === 0 && standard.quality.linkCrossings === 0) {
    return standard;
  }
  const graph = compileGraph(standard.positions, request.edges);
  if (!graph || graph.edgeCount === 0 || (options.maxDurationMs ?? DEFAULT_MAX_DURATION_MS) <= 0) {
    return standard;
  }
  const now = runtime.now ?? (() => performance.now());
  const repairStarted = now();
  const requestedDuration = options.maxDurationMs ?? DEFAULT_MAX_DURATION_MS;
  const duration = Number.isFinite(requestedDuration)
    ? Math.max(0, requestedDuration)
    : requestedDuration === Number.POSITIVE_INFINITY
    ? Number.POSITIVE_INFINITY
    : 0;
  const deadline = repairStarted + duration;
  const workStats = emptyConstraintWorkStats();
  // `layoutsConsidered` in progress is an operation-wide work counter. Keep
  // completed unrestricted-polish passes as an explicit offset so returning
  // to mask compaction after a dynamic-mask discovery cannot make it regress.
  let progressPolishPasses = 0;
  const emitProgress = (progress: Parameters<NonNullable<SearchRuntimeOptions["progress"]>>[0]): void => {
    const enriched = {
      ...progress,
      ...workStats,
      layoutsConsidered: progress.layoutsConsidered + progressPolishPasses,
      bestQuality: progress.bestQuality ?? winner.quality,
    };
    runtime.progress?.(enriched);
    trace?.({ type: "constraint-progress", stage: "constraint-repair", ...enriched });
  };
  const requestedLayouts = Math.floor(options.maxLayouts ?? 1);
  const maximumLayouts = Number.isFinite(requestedLayouts)
    ? Math.max(1, requestedLayouts)
    : requestedLayouts === Number.POSITIVE_INFINITY
    ? Number.POSITIVE_INFINITY
    : 1;
  const requestedPolishTournaments = Math.floor(
    options.maxPolishTournaments ?? Number.POSITIVE_INFINITY,
  );
  const maximumPolishTournaments = Number.isFinite(requestedPolishTournaments)
    ? Math.max(0, requestedPolishTournaments)
    : Number.POSITIVE_INFINITY;
  const requestedPolishPasses = Math.floor(
    options.maxPolishPasses ?? Number.POSITIVE_INFINITY,
  );
  const maximumPolishPasses = Number.isFinite(requestedPolishPasses)
    ? Math.max(0, requestedPolishPasses)
    : requestedPolishPasses === Number.POSITIVE_INFINITY
    ? Number.POSITIVE_INFINITY
    : 0;
  const requestedExtensionStates = Math.floor(
    options.maxExtensionStates ?? DEFAULT_MAX_EXTENSION_STATES,
  );
  const maximumExtensionStates = Number.isFinite(requestedExtensionStates)
    ? Math.max(0, requestedExtensionStates)
    : requestedExtensionStates === Number.POSITIVE_INFINITY
    ? Number.POSITIVE_INFINITY
    : DEFAULT_MAX_EXTENSION_STATES;
  const requestedMaskDiversifications = Math.floor(
    options.maxMaskDiversifications ?? DEFAULT_MAX_MASK_DIVERSIFICATIONS,
  );
  const maximumMaskDiversifications = Number.isFinite(requestedMaskDiversifications)
    ? Math.max(1, requestedMaskDiversifications)
    : requestedMaskDiversifications === Number.POSITIVE_INFINITY
    ? Number.POSITIVE_INFINITY
    : DEFAULT_MAX_MASK_DIVERSIFICATIONS;
  let winner = standard;
  let winnerRemovedGroups = removedGroupsForPositions(graph, standard.positions);
  let winnerRelaxedScore: readonly [number, number] = removedScore(graph, winnerRemovedGroups);
  let winnerSignature = constraintPositionSignature(graph, winner.positions) as ConstraintPositionSignature;
  let searchRestarts = 0;
  let searchFeasibilityChecks = 0;
  const movableById = new Map(request.residents.map((resident) => [resident.id, resident.movable]));
  const fixedIds = new Set(
    request.residents.filter((resident) => !resident.movable).map((resident) => resident.id),
  );
  let layoutsConsidered = 0;
  let layoutFrontierTruncated = false;
  let compactionAttempts = 0;
  let lastCompactionProgressAt = repairStarted;
  let compactionMs = 0;
  let timedOut = false;
  const perfect = (plan: IntegralLayoutPlan): boolean =>
    plan.quality.cardinalRayViolations === 0 &&
    plan.quality.routingViolations === 0 &&
    plan.quality.linkCrossings === 0;
  interface DistinctLayout {
    signature: ConstraintPositionSignature;
    plan: IntegralLayoutPlan;
    relaxedScore: readonly [number, number];
    removedGroups: Uint8Array;
    ordinal: number;
    polished: boolean;
  }
  const polishFrontier: DistinctLayout[] = [];
  const maximumSeenSignatures = Number.isFinite(maximumExtensionStates)
    ? Math.max(64, Math.min(2_048, maximumExtensionStates))
    : 2_048;
  // The dedup window retains only 64-bit signature keys, never coordinates.
  // Complete positions survive solely on the retained holders — the winner and
  // the polish frontier — and candidates are exact-compared against those
  // holders before this window is consulted. Every remembered key passed the
  // full admission pipeline at first encounter, including the strict-improvement
  // comparison against the winner's quality, and the winner's quality only
  // strictly improves — so a genuine re-encounter can never beat the winner. A
  // key collision therefore suppresses at most the re-publication of a
  // non-improving duplicate; hard-validity and quality guarantees are untouched.
  const seenSignatureKeys = new Set<string>();
  const seenSignatureOrder: string[] = [];
  let nextSignatureEviction = 0;
  let nextLayoutOrdinal = 0;
  const rememberPositionSignature = (signature: ConstraintPositionSignature): boolean => {
    const key = constraintPositionSignatureKey(signature);
    if (seenSignatureKeys.has(key)) return false;
    seenSignatureKeys.add(key);
    if (seenSignatureOrder.length < maximumSeenSignatures) {
      seenSignatureOrder.push(key);
    } else {
      seenSignatureKeys.delete(seenSignatureOrder[nextSignatureEviction]);
      seenSignatureOrder[nextSignatureEviction] = key;
      nextSignatureEviction = (nextSignatureEviction + 1) % maximumSeenSignatures;
    }
    return true;
  };
  const publishImprovement = (plan: IntegralLayoutPlan): void => trace?.({
    type: "constraint-improvement",
    stage: "constraint-repair",
    restarts: searchRestarts,
    feasibilityChecks: searchFeasibilityChecks,
    layoutsConsidered: layoutsConsidered + progressPolishPasses,
    compactionAttempts,
    ...workStats,
    candidate: {
      quality: { ...plan.quality },
      movedExisting: [...plan.movedExisting].sort(),
      positions: [...plan.positions]
        .sort(([a], [b]) => compareRoomIds(a, b))
        .map(([id, position]) => ({ id, ...position })),
    },
  });
  const retainForPolish = (entry: DistinctLayout): void => {
    polishFrontier.push(entry);
    polishFrontier.sort((a, b) =>
      compareLayoutQuality(b.plan.quality, a.plan.quality) || a.ordinal - b.ordinal
    );
    if (polishFrontier.length <= maximumLayouts) return;
    polishFrontier.pop();
    layoutFrontierTruncated = true;
  };
  const considerRawIncumbent = (
    positions: ReadonlyMap<string, GridPosition>,
    relaxedScore: readonly [number, number],
    removedGroups: Uint8Array,
    measuredQuality?: Readonly<LayoutQuality>,
  ): void => {
    if (!hardValidLayoutPositions(graph, removedGroups, positions, fixedIds)) return;
    workStats.rawIncumbents += 1;
    workStats.firstIncumbentMs ??= Math.max(0, now() - repairStarted);
    const signature = constraintPositionSignature(graph, positions);
    if (!signature) return;
    if (sameConstraintPositionSignature(signature, winnerSignature) &&
      betterScore(relaxedScore, winnerRelaxedScore)) {
      winnerRelaxedScore = relaxedScore;
      winnerRemovedGroups = removedGroups.slice();
    }
    const retained = polishFrontier.find((entry) =>
      sameConstraintPositionSignature(entry.signature, signature)
    );
    if (retained) {
      if (betterScore(relaxedScore, retained.relaxedScore)) {
        retained.relaxedScore = relaxedScore;
        retained.removedGroups = removedGroups.slice();
      }
      return;
    }
    if (!rememberPositionSignature(signature)) return;
    // Only admitted, distinct layouts pay for full Map retention, movement
    // reconstruction, and any stable trace ordering.
    const copied = new Map(positions);
    const plan: IntegralLayoutPlan = {
      positions: copied,
      movedExisting: recomputeMovedExisting(request, copied),
      // The production compactor supplies a freshly measured complete public
      // score. Test seams and alternate compactors are scored here instead;
      // incremental/private scores never cross this boundary.
      quality: measuredQuality
        ? { ...measuredQuality }
        : measureIntegralLayoutQuality(copied, request.edges),
    };
    workStats.distinctLayouts += 1;
    retainForPolish({
      signature,
      plan,
      relaxedScore,
      removedGroups: removedGroups.slice(),
      ordinal: nextLayoutOrdinal++,
      polished: false,
    });
    if (compareLayoutQuality(plan.quality, winner.quality) > 0) {
      let published = plan;
      try {
        const acceptsCompactedPositions = (positions: ReadonlyMap<string, GridPosition>): boolean =>
          hardValidLayoutPositions(graph, removedGroups, positions, fixedIds);
        const compacted = (runtime.gravity ?? compactIntegralLayoutPlan)({
          ...request,
          trace: undefined,
        }, plan, {
          acceptsPositions: acceptsCompactedPositions,
          shouldCancel: () => Number.isFinite(deadline) && now() >= deadline,
        });
        const compactedQuality = measureIntegralLayoutQuality(compacted.positions, request.edges);
        if (acceptsCompactedPositions(compacted.positions) &&
          compareLayoutQuality(compactedQuality, plan.quality) >= 0) {
          published = {
            positions: new Map(compacted.positions),
            movedExisting: recomputeMovedExisting(request, compacted.positions),
            quality: compactedQuality,
          };
        }
      } catch {
        // A compaction-only aesthetic pass must never retract a freshly scored,
        // hard-valid raw improvement. The ordinary frontier can still polish it.
      }
      winner = published;
      winnerRelaxedScore = relaxedScore;
      winnerRemovedGroups = removedGroups.slice();
      winnerSignature = constraintPositionSignature(graph, winner.positions) as ConstraintPositionSignature;
      workStats.softIncumbents += 1;
      publishImprovement(winner);
    }
  };

  interface MaskQueueNode {
    mask: Uint8Array;
    next?: MaskQueueNode;
  }
  let geometryHead: MaskQueueNode | undefined;
  let geometryTail: MaskQueueNode | undefined;
  let searchHead: MaskQueueNode | undefined;
  let searchTail: MaskQueueNode | undefined;
  let pendingMasks = 0;
  let knownMaskCount = 0;
  const knownMaskBuckets = new Map<number, Uint8Array[]>();
  const processedMaskBuckets = new Map<number, Uint8Array[]>();
  const rememberProcessedMask = (mask: Uint8Array): void => {
    const hash = constraintMaskHash(mask);
    const bucket = processedMaskBuckets.get(hash);
    if (bucket) bucket.push(mask.slice());
    else processedMaskBuckets.set(hash, [mask.slice()]);
  };
  const wasProcessedMask = (mask: Uint8Array): boolean => {
    const hash = constraintMaskHash(mask);
    return processedMaskBuckets.get(hash)?.some((known) => sameConstraintMask(known, mask)) === true;
  };
  let extensionCompleted = true;
  let extensionExhausted = false;
  let extensionCancelled = false;
  let maskExhausted = false;
  let fatalCompactionFailure = false;
  const stageMask = (
    mask: Uint8Array,
    geometry: boolean,
    restageKnownUnprocessed = false,
  ): boolean => {
    const hash = constraintMaskHash(mask);
    const known = knownMaskBuckets.get(hash)?.some((candidate) =>
      sameConstraintMask(candidate, mask)
    ) === true;
    if (known) {
      // A perfect interim winner can clear queued lower-priority masks. If a
      // later unrestricted polish selects one of those masks, closure must be
      // able to put that already-counted mask back on the geometry queue.
      if (!restageKnownUnprocessed || wasProcessedMask(mask)) return true;
    } else {
      if (knownMaskCount >= maximumMaskDiversifications) {
        maskExhausted = true;
        return false;
      }
      const copied = mask.slice();
      const bucket = knownMaskBuckets.get(hash);
      if (bucket) bucket.push(copied);
      else knownMaskBuckets.set(hash, [copied]);
      knownMaskCount += 1;
    }
    const copied = mask.slice();
    const node: MaskQueueNode = { mask: copied };
    if (geometry) {
      if (geometryTail) geometryTail.next = node;
      else geometryHead = node;
      geometryTail = node;
    } else {
      if (searchTail) searchTail.next = node;
      else searchHead = node;
      searchTail = node;
    }
    pendingMasks += 1;
    return true;
  };
  const takeMask = (): Uint8Array | undefined => {
    const node = geometryHead ?? searchHead;
    if (!node) return undefined;
    if (geometryHead) {
      geometryHead = node.next;
      if (!geometryHead) geometryTail = undefined;
    } else {
      searchHead = node.next;
      if (!searchHead) searchTail = undefined;
    }
    pendingMasks -= 1;
    return node.mask;
  };
  const drainMasks = (): boolean => {
    while (pendingMasks > 0 && !fatalCompactionFailure) {
      // Even a zero state budget evaluates the supplied hard-valid geometry
      // for the first mask. The extension core then reports exhaustion without
      // building a separator state, preserving a truthful incumbent/report.
      if (workStats.separatorStates >= maximumExtensionStates && compactionAttempts > 0) {
        extensionCompleted = false;
        extensionExhausted = true;
        return false;
      }
      if (now() >= deadline) {
        timedOut = true;
        extensionCompleted = false;
        extensionCancelled = true;
        return false;
      }
      const removedGroups = takeMask() as Uint8Array;
      rememberProcessedMask(removedGroups);
      const relaxedScore = removedScore(graph, removedGroups);
      compactionAttempts += 1;
      workStats.maskDiversifications += 1;
      const compactStarted = now();
      const rawBefore = workStats.rawIncumbents;
      const enqueueEqualPrimarySwaps = (relationGroups: readonly number[]): void => {
        const defectGroups = [...new Set(relationGroups)]
          .filter((group) => group >= 0 && group < graph.groupCount && !removedGroups[group])
          .sort((a, b) => a - b);
        const restoredGroups = Array.from(removedGroups, (_removed, group) => group)
          .filter((group) => removedGroups[group] !== 0);
        for (const defect of defectGroups) {
          for (const restored of restoredGroups) {
            if (graph.groupSourceCount[defect] !== graph.groupSourceCount[restored] ||
              graph.groupReciprocalCount[defect] !== graph.groupReciprocalCount[restored]) continue;
            const mask = removedGroups.slice();
            mask[defect] = 1;
            mask[restored] = 0;
            stageMask(mask, true);
          }
        }
      };
      let compactionStatus: { completed: boolean; cancelled: boolean; exhausted: boolean } = {
        completed: true,
        cancelled: false,
        exhausted: false,
      };
      const compactedResult = runtime.compact
        ? runtime.compact(graph, removedGroups)
        : compactConstraints(graph, removedGroups, fixedIds, {
          maximumStates: maximumExtensionStates - workStats.separatorStates,
          shouldCancel: () => Number.isFinite(deadline) && now() >= deadline,
          score: (positions) => measureIntegralLayoutQuality(positions, request.edges),
          workStats,
          onIncumbent: (positions, quality) =>
            considerRawIncumbent(positions, relaxedScore, removedGroups, quality),
          onDiversification: enqueueEqualPrimarySwaps,
          onFinish: (status) => {
            compactionStatus = status;
          },
          onProgress: () => {
            const progressAt = now();
            if (progressAt - lastCompactionProgressAt < PROGRESS_INTERVAL_MS) return;
            lastCompactionProgressAt = progressAt;
            emitProgress({
              ...workStats,
              phase: "compaction",
              restarts: searchRestarts,
              feasibilityChecks: searchFeasibilityChecks,
              layoutsConsidered,
              compactionAttempts,
              elapsedMs: Math.max(0, progressAt - repairStarted),
              bestQuality: winner.quality,
            });
          },
        });
      compactionMs += Math.max(0, now() - compactStarted);
      extensionCompleted &&= compactionStatus.completed;
      extensionExhausted ||= compactionStatus.exhausted;
      extensionCancelled ||= compactionStatus.cancelled;
      if (compactionStatus.cancelled && Number.isFinite(deadline) && now() >= deadline) {
        timedOut = true;
      }
      if (runtime.compact && compactedResult.ok) {
        considerRawIncumbent(compactedResult.value, relaxedScore, removedGroups);
      }
      if (!compactedResult.ok && runtime.compact) {
        fatalCompactionFailure = true;
        extensionCompleted = false;
      }
      const progressAt = now();
      if ((compactionAttempts & COMPACTION_PROGRESS_CHECK_MASK) === 0 &&
        (workStats.rawIncumbents !== rawBefore ||
          progressAt - lastCompactionProgressAt >= PROGRESS_INTERVAL_MS)) {
        lastCompactionProgressAt = progressAt;
        emitProgress({
          ...workStats,
          phase: "compaction",
          restarts: searchRestarts,
          feasibilityChecks: searchFeasibilityChecks,
          layoutsConsidered,
          compactionAttempts,
          elapsedMs: Math.max(0, progressAt - repairStarted),
          bestQuality: winner.quality,
        });
      }
      if (perfect(winner)) {
        geometryHead = geometryTail = searchHead = searchTail = undefined;
        pendingMasks = 0;
        return false;
      }
    }
    return !fatalCompactionFailure && !extensionExhausted && !extensionCancelled && !maskExhausted &&
      !perfect(winner);
  };

  const acceptsAnyHardValidPositions = (
    positions: ReadonlyMap<string, GridPosition>,
  ): boolean => {
    const removedGroups = removedGroupsForPositions(graph, positions);
    return hardValidLayoutPositions(graph, removedGroups, positions, fixedIds);
  };
  let earlyPolished: ConstraintPolishResult = {
    plan: winner,
    tournaments: 0,
    passes: 0,
    anchorsTried: 0,
    improvements: 0,
    fixedPoint: false,
    cutoff: "tournaments",
    elapsedMs: 0,
  };
  let earlyWinnerRetained = false;
  let earlyWinnerMask: Uint8Array | undefined;
  let earlyFixedPointSignature: ConstraintPositionSignature | undefined;
  // One anchored baseline reflow gives the anytime lane a useful incumbent
  // before the exact master search spends minutes certifying masks. Its single
  // complete pass cannot expand into the multi-anchor tournament and starve
  // MaxHS; every later tournament shares the same aggregate pass ceiling.
  // Custom search seams retain their historical phase isolation in tests.
  if (!runtime.search && !runtime.compact && !runtime.polish &&
    maximumPolishTournaments > 0 && maximumPolishPasses > 0) {
    earlyPolished = (runtime.polish ?? polishConstraintLayoutToFixedPoint)(
      request,
      winner,
      trace,
      {
        repairStarted,
        deadline,
        maximumTournaments: 1,
        maximumPasses: 1,
        constraintLayoutsConsidered: 0,
        compactionAttempts,
        restarts: 0,
        feasibilityChecks: 0,
        workStats,
        acceptsPositions: acceptsAnyHardValidPositions,
      },
      now,
    );
    const earlyQuality = measureIntegralLayoutQuality(earlyPolished.plan.positions, request.edges);
    const earlyMask = removedGroupsForPositions(graph, earlyPolished.plan.positions);
    if (hardValidLayoutPositions(graph, earlyMask, earlyPolished.plan.positions, fixedIds) &&
      compareLayoutQuality(earlyQuality, winner.quality) > 0) {
      winner = {
        positions: new Map(earlyPolished.plan.positions),
        movedExisting: recomputeMovedExisting(request, earlyPolished.plan.positions),
        quality: earlyQuality,
      };
      winnerRemovedGroups = earlyMask;
      winnerRelaxedScore = removedScore(graph, earlyMask);
      winnerSignature = constraintPositionSignature(graph, winner.positions) as ConstraintPositionSignature;
      earlyWinnerRetained = true;
      earlyWinnerMask = earlyMask;
    }
    progressPolishPasses = earlyPolished.passes;
    const earlySignature = constraintPositionSignature(graph, earlyPolished.plan.positions);
    if (earlyPolished.fixedPoint && earlySignature &&
      sameConstraintPositionSignature(earlySignature, winnerSignature)) {
      earlyFixedPointSignature = earlySignature;
    }
    if (earlyPolished.cutoff === "time") timedOut = true;
  }

  const remainingSearchDuration = Number.isFinite(deadline)
    ? Math.max(0, deadline - now())
    : Number.POSITIVE_INFINITY;
  const searchOptions = remainingSearchDuration === options.maxDurationMs
    ? options
    : { ...options, maxDurationMs: remainingSearchDuration };
  const searchStarted = now();
  const searchedResult = (runtime.search ?? constraintSearch)(graph, searchOptions, standard.positions, {
    ...runtime,
    progress: (progress) => {
      searchRestarts = progress.restarts;
      searchFeasibilityChecks = progress.feasibilityChecks;
      emitProgress(progress);
    },
    mask: (mask, progress) => {
      searchRestarts = progress.restarts;
      searchFeasibilityChecks = progress.feasibilityChecks;
      // Master search owns proof/certification. Geometry is deliberately
      // deferred until it returns, so a difficult crude mask cannot consume
      // the entire extension budget before MaxHS discovers its best mask.
      return true;
    },
  });
  if (!searchedResult.ok && searchedResult.reason === "time") timedOut = true;
  // Production search converts time/work limits to a partial success, but an
  // unexpected late analysis failure must not retract a complete improvement
  // that a streamed feasible mask already published. Retain it with an
  // explicitly non-optimal search termination; only a pre-incumbent failure
  // retains the historical exact-standard fallback.
  if (!searchedResult.ok && !earlyWinnerRetained) return standard;
  const searched: ConstraintSearchResult = searchedResult.ok
    ? searchedResult.value
    : {
      removed: winnerRemovedGroups.slice(),
      masks: [],
      score: winnerRelaxedScore,
      lowerBound: 0,
      optimal: false,
      cutoff: timedOut ? "time" : "restarts",
      restarts: searchRestarts,
      feasibilityChecks: searchFeasibilityChecks,
      elapsedMs: Math.max(0, now() - searchStarted),
    };
  searchRestarts = searched.restarts;
  searchFeasibilityChecks = searched.feasibilityChecks;
  // `constraintSearch` returns masks best-first. Stage that certified order
  // before the early heuristic mask; encounter-order callbacks are intentionally
  // not retained, so the crude initial mask cannot starve the MaxHS incumbent.
  for (const mask of searched.masks) stageMask(mask, false);
  if (earlyWinnerMask) stageMask(earlyWinnerMask, false);
  drainMasks();
  if (workStats.distinctLayouts === 0 && !earlyWinnerRetained) return standard;

  const polishRetainedLayouts = (): void => {
    for (;;) {
      const entry = polishFrontier.find((candidate) => !candidate.polished);
      if (!entry) return;
      const reachedLayoutLimit = layoutsConsidered >= maximumLayouts;
      const reachedDeadline = now() >= deadline;
      if (reachedLayoutLimit || reachedDeadline) {
        // Set only while an actual retained entry remains unprocessed;
        // merely consuming exactly maxLayouts entries is complete.
        layoutFrontierTruncated ||= reachedLayoutLimit;
        if (reachedDeadline) timedOut = true;
        return;
      }
      // Mark before planning because candidate publication can re-sort the
      // retained frontier. The object survives that stable reordering.
      entry.polished = true;
      layoutsConsidered += 1;
      lastCompactionProgressAt = now();
      emitProgress({
        ...workStats,
        phase: "polish",
        restarts: searched.restarts,
        feasibilityChecks: searched.feasibilityChecks,
        layoutsConsidered,
        compactionAttempts,
        elapsedMs: Math.max(0, lastCompactionProgressAt - repairStarted),
        bestQuality: winner.quality,
      });
      const acceptsPolishedPositions = (positions: ReadonlyMap<string, GridPosition>): boolean =>
        hardValidLayoutPositions(graph, entry.removedGroups, positions, fixedIds);
      const ordinaryPolishTrace = layoutsConsidered === 1 && trace
        ? (event: LayoutTraceEvent): void => {
          // Quick crossing events are relative to this one local planner seed,
          // not the operation-wide public frontier. The completed plan is
          // reconsidered below and, if globally better, published once as a
          // constraint improvement. Telemetry-only crossing progress is safe.
          if (event.type !== "crossing-repair") trace(event);
        }
        : undefined;
      const candidate = planIntegralLayout({
        residents: graph.ids.map((id) => ({
          id,
          position: entry.plan.positions.get(id) as GridPosition,
          movable: movableById.get(id) ?? true,
        })),
        nodes: [],
        edges: request.edges,
        centerId: request.centerId,
        allowExistingMoves: true,
        trace: ordinaryPolishTrace,
      }, { acceptsPositions: acceptsPolishedPositions });
      const candidateQuality = measureIntegralLayoutQuality(candidate.positions, request.edges);
      if (acceptsPolishedPositions(candidate.positions) &&
        compareLayoutQuality(candidateQuality, winner.quality) > 0) {
        winner = {
          ...candidate,
          movedExisting: recomputeMovedExisting(request, candidate.positions),
          quality: candidateQuality,
        };
        winnerRelaxedScore = entry.relaxedScore;
        winnerRemovedGroups = entry.removedGroups.slice();
        winnerSignature = constraintPositionSignature(graph, winner.positions) as ConstraintPositionSignature;
        publishImprovement(winner);
      }
    }
  };
  polishRetainedLayouts();
  const polished: ConstraintPolishResult = {
    plan: winner,
    tournaments: earlyPolished.tournaments,
    passes: earlyPolished.passes,
    anchorsTried: earlyPolished.anchorsTried,
    improvements: earlyPolished.improvements,
    fixedPoint: false,
    cutoff: earlyPolished.cutoff,
    elapsedMs: earlyPolished.elapsedMs,
  };
  // Close over masks discovered by unrestricted polish in the same bounded
  // operation. A newly selected mask is compacted before a fixed-point claim;
  // if that compaction advances the winner, another tournament starts from the
  // new basin while sharing the one configured tournament budget.
  for (;;) {
    if (earlyFixedPointSignature &&
      sameConstraintPositionSignature(earlyFixedPointSignature, winnerSignature) &&
      wasProcessedMask(winnerRemovedGroups)) {
      polished.fixedPoint = true;
      polished.cutoff = "fixed-point";
      break;
    }
    const polishSeed = winner;
    const remainingPolishTournaments = Number.isFinite(maximumPolishTournaments)
      ? Math.max(0, maximumPolishTournaments - polished.tournaments)
      : Number.POSITIVE_INFINITY;
    const remainingPolishPasses = Number.isFinite(maximumPolishPasses)
      ? Math.max(0, maximumPolishPasses - polished.passes)
      : Number.POSITIVE_INFINITY;
    const phase = (runtime.polish ?? polishConstraintLayoutToFixedPoint)(
      request,
      winner,
      trace,
      {
        repairStarted,
        deadline,
        maximumTournaments: remainingPolishTournaments,
        maximumPasses: remainingPolishPasses,
        constraintLayoutsConsidered: layoutsConsidered + progressPolishPasses,
        compactionAttempts,
        restarts: searched.restarts,
        feasibilityChecks: searched.feasibilityChecks,
        workStats,
        acceptsPositions: acceptsAnyHardValidPositions,
      },
      now,
    );
    polished.tournaments += phase.tournaments;
    polished.passes += phase.passes;
    polished.anchorsTried += phase.anchorsTried;
    polished.improvements += phase.improvements;
    polished.elapsedMs += phase.elapsedMs;
    polished.cutoff = phase.cutoff;
    polished.fixedPoint = phase.fixedPoint;
    progressPolishPasses += phase.passes;

    const phaseQuality = measureIntegralLayoutQuality(phase.plan.positions, request.edges);
    const phaseMask = removedGroupsForPositions(graph, phase.plan.positions);
    if (hardValidLayoutPositions(graph, phaseMask, phase.plan.positions, fixedIds) &&
      compareLayoutQuality(phaseQuality, polishSeed.quality) >= 0) {
      winner = {
        positions: new Map(phase.plan.positions),
        movedExisting: recomputeMovedExisting(request, phase.plan.positions),
        quality: phaseQuality,
      };
      winnerRemovedGroups = phaseMask;
      winnerRelaxedScore = removedScore(graph, phaseMask);
    } else {
      winner = polishSeed;
      winnerRemovedGroups = removedGroupsForPositions(graph, winner.positions);
      winnerRelaxedScore = removedScore(graph, winnerRemovedGroups);
    }
    winnerSignature = constraintPositionSignature(graph, winner.positions) as ConstraintPositionSignature;

    if (wasProcessedMask(winnerRemovedGroups)) break;
    const beforeDrain = winnerSignature;
    if (!stageMask(winnerRemovedGroups, true, true)) {
      polished.fixedPoint = false;
      break;
    }
    drainMasks();
    // Mask closure can discover complete layouts after the initial frontier
    // sweep. Polish every newly retained basin before claiming a fixed point;
    // a finite layout ceiling is recorded as an explicit truncation instead.
    polishRetainedLayouts();
    if (!wasProcessedMask(winnerRemovedGroups)) {
      polished.fixedPoint = false;
      break;
    }
    const winnerChanged = !sameConstraintPositionSignature(beforeDrain, winnerSignature);
    if (!winnerChanged) break;
    if (Number.isFinite(deadline) && now() >= deadline) {
      timedOut = true;
      polished.fixedPoint = false;
      polished.cutoff = "time";
      break;
    }
    if (Number.isFinite(maximumPolishTournaments) &&
      polished.tournaments >= maximumPolishTournaments) {
      polished.fixedPoint = false;
      polished.cutoff = "tournaments";
      break;
    }
    if (Number.isFinite(maximumPolishPasses) && polished.passes >= maximumPolishPasses) {
      polished.fixedPoint = false;
      polished.cutoff = "passes";
      break;
    }
  }
  polished.plan = winner;
  // Nested polish can hit the same finite repair deadline while it is inside a
  // synchronous planner callback. Carry that cooperative cutoff into the public
  // repair report even when crossing repair has no work left to cancel.
  if (earlyPolished.cutoff === "time" || polished.cutoff === "time") timedOut = true;
  const crossingStarted = now();
  const downstreamTrace = request.trace ?? trace;
  const beforeCrossing = winner;
  let publishedCrossingWinner = winner;
  const crossingTrace = downstreamTrace
    ? (event: LayoutTraceEvent): void => {
      if (event.type === "crossing-repair") {
        const positions = event.after.positions && new Map(
          event.after.positions.map(({ id, x, y, level }) => [id, { x, y, level }]),
        );
        if (!positions || !hardValidLayoutPositions(
          graph,
          winnerRemovedGroups,
          positions,
          fixedIds,
        )) return;
        const quality = measureIntegralLayoutQuality(positions, request.edges);
        if (compareLayoutQuality(quality, event.after.quality) !== 0 ||
          compareLayoutQuality(quality, publishedCrossingWinner.quality) <= 0) return;
        publishedCrossingWinner = {
          positions,
          movedExisting: recomputeMovedExisting(request, positions),
          quality,
        };
      }
      downstreamTrace(event);
    }
    : undefined;
  const crossing = repairIntegralLayoutCrossingsDeep(
    crossingTrace ? { ...request, trace: crossingTrace } : request,
    winner,
    {
      maximumWork: options.maxCrossingWork,
      shouldCancel: () => Number.isFinite(deadline) && now() >= deadline,
      acceptsPositions: (positions) =>
        hardValidLayoutPositions(graph, winnerRemovedGroups, positions, fixedIds),
    },
  );
  const crossingMs = Math.max(0, now() - crossingStarted);
  const crossingHardValid = hardValidLayoutPositions(
    graph,
    winnerRemovedGroups,
    crossing.plan.positions,
    fixedIds,
  );
  const crossingQuality = measureIntegralLayoutQuality(crossing.plan.positions, request.edges);
  if (crossingHardValid && compareLayoutQuality(crossingQuality, winner.quality) >= 0) {
    winner = {
      ...crossing.plan,
      movedExisting: recomputeMovedExisting(request, crossing.plan.positions),
      quality: crossingQuality,
    };
    winnerSignature = constraintPositionSignature(graph, winner.positions) as ConstraintPositionSignature;
  }
  // Defensive monotonic fallback: a future crossing implementation must not
  // return below a complete hard-valid candidate it already published.
  if (compareLayoutQuality(publishedCrossingWinner.quality, winner.quality) > 0) {
    winner = publishedCrossingWinner;
    winnerSignature = constraintPositionSignature(graph, winner.positions) as ConstraintPositionSignature;
  }
  const crossingImproved = compareLayoutQuality(winner.quality, beforeCrossing.quality) > 0;
  if (crossing.cancelled && Number.isFinite(deadline) && now() >= deadline) timedOut = true;
  const selected = compareLayoutQuality(winner.quality, standard.quality) > 0;
  const winnerConstraintOptimal = searched.optimal &&
    winnerRelaxedScore[0] === searched.score[0] &&
    winnerRelaxedScore[1] === searched.score[1];
  const extensionSearch = {
    completed: extensionCompleted && !extensionCancelled && !extensionExhausted && pendingMasks === 0,
    cancelled: extensionCancelled,
    exhausted: extensionExhausted,
  };
  const maskDiversification = {
    // A cancelled extension traversal may have withheld equal-primary swaps,
    // so a cut run can never claim the diversification frontier was drained.
    completed: !maskExhausted && !extensionCancelled && pendingMasks === 0,
    exhausted: maskExhausted,
  };
  const report: ConstraintRepairReport = {
    ...workStats,
    trigger: options.when,
    selected,
    constraintOptimal: winnerConstraintOptimal,
    optimal: winnerConstraintOptimal,
    cutoff: timedOut
      ? "time"
      : extensionSearch.exhausted
      ? "extensions"
      : maskDiversification.exhausted
      ? "masks"
      : layoutFrontierTruncated
      ? "layouts"
      : perfect(winner)
      ? "none"
      : searched.cutoff,
    lowerBound: searched.lowerBound,
    relaxedEdges: winnerRelaxedScore[0],
    reciprocalRelaxedEdges: winnerRelaxedScore[1],
    standardViolations: standard.quality.cardinalRayViolations,
    finalViolations: winner.quality.cardinalRayViolations,
    beforeViolations,
    standardRoutingViolations: standard.quality.routingViolations,
    finalRoutingViolations: winner.quality.routingViolations,
    beforeRoutingViolations,
    beforeSettledViolations,
    standardSettledViolations,
    beforeSettledRoutingViolations,
    standardSettledRoutingViolations,
    restarts: searched.restarts,
    feasibilityChecks: searched.feasibilityChecks,
    layoutsConsidered,
    compactionAttempts,
    extensionSearch,
    maskDiversification,
    searchMs: searched.elapsedMs,
    compactionMs,
    polishTournaments: polished.tournaments,
    polishPasses: polished.passes,
    polishAnchorsTried: polished.anchorsTried,
    polishImprovements: polished.improvements,
    // Never a proof for a run the deadline cut at any point: callers retire
    // durable retry state on this flag, so a false claim is unrecoverable.
    geometricFixedPoint: !timedOut && (searched.cutoff === "none" || perfect(winner)) &&
      !layoutFrontierTruncated && polished.fixedPoint && !crossingImproved &&
      extensionSearch.completed && maskDiversification.completed &&
      crossing.completed && !crossing.cancelled && !crossing.exhausted,
    polishCutoff: polished.cutoff,
    polishMs: polished.elapsedMs,
    crossingRepair: {
      completed: crossing.completed,
      cancelled: crossing.cancelled,
      exhausted: crossing.exhausted,
      elapsedMs: crossingMs,
      ...crossing.stats,
    },
  };
  trace?.({ type: "constraint-repair", stage: "constraint-repair", report });
  // Polish and crossing phases rebuild the winner without the standard plan's
  // route amendments, and permanent all-immovable defects are exactly the case
  // repair cannot move rooms for — recompute amendments for the final geometry.
  const routeAmendments = computeIntegralRouteAmendments(request, winner);
  const { routeAmendments: _stale, ...base } = winner;
  return {
    ...base,
    ...(routeAmendments ? { routeAmendments } : {}),
    constraintRepair: report,
  };
}

/** Whole-layout constraint repair. Called only in the Worker after the standard plan. */
export function repairIntegralLayoutConstraints(
  request: IntegralLayoutRequest,
  standard: IntegralLayoutPlan,
  options: ConstraintRepairOptions,
  trace?: (event: LayoutTraceEvent) => void,
): IntegralLayoutPlan {
  return repairIntegralLayoutConstraintsWithRuntime(request, standard, options, trace);
}

/** Direct-only seams for deterministic stress and failure-path tests; not re-exported by the package entry. */
export const constraintLayoutInternalsForTesting = {
  polish(
    request: IntegralLayoutRequest,
    seed: IntegralLayoutPlan,
    trace?: (event: LayoutTraceEvent) => void,
    options: {
      now?: () => number;
      deadline?: number;
      maximumTournaments?: number;
      maximumPasses?: number;
    } = {},
  ) {
    const now = options.now ?? (() => performance.now());
    const repairStarted = now();
    return polishConstraintLayoutToFixedPoint(request, seed, trace, {
      repairStarted,
      deadline: options.deadline ?? Number.POSITIVE_INFINITY,
      maximumTournaments: options.maximumTournaments ?? Number.POSITIVE_INFINITY,
      maximumPasses: options.maximumPasses ?? Number.POSITIVE_INFINITY,
      constraintLayoutsConsidered: 0,
      compactionAttempts: 0,
      restarts: 0,
      feasibilityChecks: 0,
      workStats: emptyConstraintWorkStats(),
    }, now);
  },

  analyze(
    positions: ReadonlyMap<string, GridPosition>,
    edges: readonly LayoutEdge[],
    removedSourceIndexes: readonly number[] = [],
  ) {
    const graph = compileGraph(positions, edges);
    if (!graph) return { ok: false as const, reason: "unsupported" as const };
    const removedSources = new Set(removedSourceIndexes);
    const removedSourceMask = Uint8Array.from(
      graph.sourceIndexes,
      (sourceIndex) => removedSources.has(sourceIndex) ? 1 : 0,
    );
    const removedGroups = sourceMaskToGroupMask(graph, removedSourceMask);
    const analyzed = analyzeConstraints(graph, removedGroups);
    if (!analyzed.ok) return { ok: false as const, reason: analyzed.reason };
    return {
      ok: true as const,
      feasible: !analyzed.value.conflict,
      conflictSourceIndexes: analyzed.value.conflict
        ? sourceIndexesForGroups(graph, analyzed.value.conflict)
        : undefined,
    };
  },

  hardValid(
    positions: ReadonlyMap<string, GridPosition>,
    edges: readonly LayoutEdge[],
    candidatePositions: ReadonlyMap<string, GridPosition>,
    removedSourceIndexes: readonly number[] = [],
    fixedIds: readonly string[] = [],
  ) {
    const graph = compileGraph(positions, edges);
    if (!graph) return false;
    const removedSources = new Set(removedSourceIndexes);
    const removedSourceMask = Uint8Array.from(
      graph.sourceIndexes,
      (sourceIndex) => removedSources.has(sourceIndex) ? 1 : 0,
    );
    return hardValidLayoutPositions(
      graph,
      sourceMaskToGroupMask(graph, removedSourceMask),
      candidatePositions,
      new Set(fixedIds),
    );
  },

  firstAdmissibleSeparator(
    nodeCount: number,
    baseArcs: readonly ConstraintExtensionArc[],
    defects: readonly ConstraintExtensionDefect[],
  ): number | undefined {
    const outgoing = [0, 1, 2].map(() =>
      Array.from({ length: nodeCount }, () => [] as number[])
    );
    for (const arc of baseArcs) outgoing[arc.axis][arc.from].push(arc.to);
    const hasAdmissibleSeparator = createConstraintSeparatorAdmission(nodeCount);
    const candidates = defects.map((defect, index) => ({ defect, index }));
    const selected = firstAdmissibleConstraintDefect(candidates, (candidate) =>
      hasAdmissibleSeparator(candidate.defect.alternatives, outgoing, () => false) === true
        ? candidate.defect
        : undefined
    );
    return selected && selected !== DEFECT_SCAN_CANCELLED ? defects.indexOf(selected) : undefined;
  },

  compact(
    positions: ReadonlyMap<string, GridPosition>,
    edges: readonly LayoutEdge[],
    options: {
      removedSourceIndexes?: readonly number[];
      fixedIds?: readonly string[];
      maximumStates?: number;
      /** Latches cooperative cancellation once this many incumbents published. */
      cancelAtIncumbent?: number;
      /** Raw cooperative predicate, observed alongside any incumbent latch. */
      shouldCancel?: () => boolean;
    } = {},
  ) {
    const graph = compileGraph(positions, edges);
    if (!graph) return { ok: false as const, reason: "unsupported" as const };
    const removedSources = new Set(options.removedSourceIndexes ?? []);
    const removedSourceMask = Uint8Array.from(
      graph.sourceIndexes,
      (sourceIndex) => removedSources.has(sourceIndex) ? 1 : 0,
    );
    const removedGroups = sourceMaskToGroupMask(graph, removedSourceMask);
    const incumbents: { positions: Map<string, GridPosition>; quality: LayoutQuality }[] = [];
    const workStats = emptyConstraintWorkStats();
    let status = { completed: true, cancelled: false, exhausted: false };
    let cancelRequested = false;
    const compacted = compactConstraints(graph, removedGroups, new Set(options.fixedIds), {
      maximumStates: options.maximumStates,
      shouldCancel: options.cancelAtIncumbent === undefined && options.shouldCancel === undefined
        ? undefined
        : () => cancelRequested || options.shouldCancel?.() === true,
      score: (candidate) => measureIntegralLayoutQuality(candidate, edges),
      onIncumbent: (candidate, quality) => {
        incumbents.push({
          positions: new Map(candidate),
          quality: { ...quality },
        });
        if (options.cancelAtIncumbent !== undefined &&
          incumbents.length >= options.cancelAtIncumbent) cancelRequested = true;
      },
      onFinish: (finished) => {
        status = finished;
      },
      workStats,
    });
    return compacted.ok
      ? {
        ok: true as const,
        positions: compacted.value,
        quality: measureIntegralLayoutQuality(compacted.value, edges),
        incumbents,
        status,
        workStats,
      }
      : { ok: false as const, reason: compacted.reason, incumbents, status, workStats };
  },

  search(
    positions: ReadonlyMap<string, GridPosition>,
    edges: readonly LayoutEdge[],
    options: ConstraintRepairOptions,
    runtime: SearchRuntimeOptions = {},
  ) {
    const graph = compileGraph(positions, edges);
    if (!graph) return { ok: false as const, reason: "unsupported" as const };
    const searched = constraintSearch(graph, options, positions, runtime);
    if (!searched.ok) return { ok: false as const, reason: searched.reason };
    const removedSources = expandGroupMask(graph, searched.value.removed);
    return {
      ok: true as const,
      ...searched.value,
      removed: removedSources,
      masks: searched.value.masks.map((mask) => expandGroupMask(graph, mask)),
      removedSourceIndexes: Array.from(removedSources, (value, edge) => value
        ? graph.sourceIndexes[edge]
        : -1).filter((sourceIndex) => sourceIndex !== -1),
    };
  },

  repairWithFailure(
    request: IntegralLayoutRequest,
    standard: IntegralLayoutPlan,
    options: ConstraintRepairOptions,
    fail: "search" | "compaction",
    trace?: (event: LayoutTraceEvent) => void,
  ) {
    return repairIntegralLayoutConstraintsWithRuntime(request, standard, options, trace, {
      search: fail === "search" ? () => failure("analysis") : undefined,
      compact: fail === "compaction" ? () => failure("compaction") : undefined,
    });
  },

  repairWithGravity(
    request: IntegralLayoutRequest,
    standard: IntegralLayoutPlan,
    options: ConstraintRepairOptions,
    gravity: NonNullable<RepairRuntimeOptions["gravity"]>,
    trace?: (event: LayoutTraceEvent) => void,
  ) {
    return repairIntegralLayoutConstraintsWithRuntime(request, standard, options, trace, {
      gravity,
    });
  },

  repairWithClock(
    request: IntegralLayoutRequest,
    standard: IntegralLayoutPlan,
    options: ConstraintRepairOptions,
    now: () => number,
    trace?: (event: LayoutTraceEvent) => void,
  ) {
    return repairIntegralLayoutConstraintsWithRuntime(request, standard, options, trace, { now });
  },
};
