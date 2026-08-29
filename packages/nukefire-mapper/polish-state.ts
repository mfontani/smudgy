/** Durable hint that an area's existing geometry may still admit improvement. */
export const AREA_POLISH_PENDING_PROPERTY = "nukefire.layout.polish-pending";

/** Value written while polish remains eligible. An empty value is the logical clear state. */
export const AREA_POLISH_PENDING_VALUE = "true";

/**
 * Durable memo beside the pending hint. The historical property name remains
 * stable, but v2 stores one resident-geometry fingerprint plus a bounded set
 * of exact, compact entry-context keys. An empty value is the logical clear
 * state.
 */
export const AREA_POLISH_EXHAUSTED_FINGERPRINT_PROPERTY =
  "nukefire.layout.polish-exhausted-fingerprint";

/** Structured memo schema; legacy single-geometry values remain readable but inert. */
export const AREA_POLISH_MEMO_SCHEMA_VERSION = 2;

/**
 * Bump when identical planner inputs can explore a materially different
 * deterministic search. Exact per-request budgets are also part of the key.
 */
export const AREA_POLISH_SEARCH_GENERATION = 1;

/** Bounded recency set of fruitless entry contexts retained for one geometry. */
export const MAX_AREA_POLISH_MEMO_CONTEXTS = 32;

export interface AreaPolishChartNode {
  readonly id: string;
  readonly relative: { readonly x: number; readonly y: number; readonly level: number };
}

export interface AreaPolishChartEdge {
  readonly from: string;
  readonly to: string;
  readonly direction: string;
  readonly constraintVector?: {
    readonly x: number;
    readonly y: number;
    readonly level: number;
  };
}

export interface AreaPolishWorkPolicy {
  readonly when: string;
  readonly maxDurationMs?: number;
  readonly maxRestarts?: number;
  readonly maxLayouts?: number;
  readonly maxPolishTournaments?: number;
  readonly maxPolishPasses?: number;
  readonly maxExtensionStates?: number;
  readonly maxMaskDiversifications?: number;
  readonly maxCrossingWork?: number;
}

/** Exact deterministic input scope for one passive polish attempt. */
export interface AreaPolishPlanningContext {
  readonly geometryFingerprint: string;
  /** 128-bit hash of the canonical center/chart/policy input. */
  readonly key: string;
}

export type AreaPolishMemo =
  | {
    readonly kind: "contexts";
    readonly geometryFingerprint: string;
    /** Oldest to newest, with deterministic oldest-first eviction. */
    readonly contextKeys: readonly string[];
  }
  | {
    /** Pre-v2 values keyed only resident geometry and are unsafe to suppress. */
    readonly kind: "legacy";
    readonly propertyValue: string;
  };

function compareCodeUnits(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function finiteOrToken(
  value: number | undefined,
): number | "infinity" | "-infinity" | "nan" | null {
  if (value === undefined) return null;
  if (Number.isNaN(value)) return "nan";
  if (value === Number.POSITIVE_INFINITY) return "infinity";
  if (value === Number.NEGATIVE_INFINITY) return "-infinity";
  return value;
}

const FNV_1A_128_OFFSET = 0x6c62272e07bb014262b821756295c58dn;
const FNV_1A_128_PRIME = 0x0000000001000000000000000000013bn;
const UINT128_MASK = (1n << 128n) - 1n;

/** Compact the potentially large canonical Map.Local chart into 128 bits. */
function contextKey(canonical: string): string {
  let hash = FNV_1A_128_OFFSET;
  for (const byte of new TextEncoder().encode(canonical)) {
    hash ^= BigInt(byte);
    hash = (hash * FNV_1A_128_PRIME) & UINT128_MASK;
  }
  return hash.toString(16).padStart(32, "0");
}

/**
 * Canonicalize every request-local input which can change a bounded passive
 * search despite identical resident geometry. Node/edge enumeration order is
 * irrelevant; the anchor, chart, perfect-search mode, generation, and exact
 * budgets are not.
 */
export function createAreaPolishPlanningContext(input: {
  readonly geometryFingerprint: string;
  readonly centerId: string | undefined;
  readonly nodes: readonly AreaPolishChartNode[];
  readonly edges: readonly AreaPolishChartEdge[];
  readonly searchForPerfectLayouts: boolean;
  readonly policy: Readonly<AreaPolishWorkPolicy>;
}): AreaPolishPlanningContext {
  const nodes = input.nodes
    .map((node) => [
      node.id,
      node.relative.x,
      node.relative.y,
      node.relative.level,
    ] as const)
    .sort((a, b) => compareCodeUnits(JSON.stringify(a), JSON.stringify(b)));
  const edges = input.edges
    .map((edge) => [
      edge.from,
      edge.to,
      edge.direction,
      edge.constraintVector
        ? [edge.constraintVector.x, edge.constraintVector.y, edge.constraintVector.level]
        : null,
    ] as const)
    .sort((a, b) => compareCodeUnits(JSON.stringify(a), JSON.stringify(b)));
  const policy = input.policy;
  const canonical = JSON.stringify([
    AREA_POLISH_SEARCH_GENERATION,
    input.centerId ?? null,
    nodes,
    edges,
    input.searchForPerfectLayouts,
    policy.when,
    finiteOrToken(policy.maxDurationMs),
    finiteOrToken(policy.maxRestarts),
    finiteOrToken(policy.maxLayouts),
    finiteOrToken(policy.maxPolishTournaments),
    finiteOrToken(policy.maxPolishPasses),
    finiteOrToken(policy.maxExtensionStates),
    finiteOrToken(policy.maxMaskDiversifications),
    finiteOrToken(policy.maxCrossingWork),
  ]);
  return {
    geometryFingerprint: input.geometryFingerprint,
    key: contextKey(canonical),
  };
}

/**
 * The composite fixed-point bit incorporates unfinished constraint, separator,
 * mask, polish, and crossing frontiers. It is an exact-context proof, not an
 * area-global proof. The optional cutoff and cancellation fields distinguish
 * deterministic ceiling exhaustion — the other outcome worth memoizing —
 * from wall-deadline, cancellation, and error stops.
 */
export interface AreaPolishReport {
  readonly geometricFixedPoint: boolean;
  /** "time" marks a machine-speed wall-deadline stop; ceilings use other values. */
  readonly cutoff?: string;
  /** "time" and "error" mark polish stops that are not deterministic ceilings. */
  readonly polishCutoff?: string;
  readonly extensionSearch?: { readonly cancelled: boolean; readonly exhausted?: boolean };
  readonly crossingRepair?: { readonly cancelled: boolean; readonly exhausted?: boolean };
}

export type AreaPolishEvent =
  | { readonly kind: "topology-deferred" }
  | { readonly kind: "polish-started" }
  | {
    readonly kind: "polish-completed";
    readonly report?: Readonly<AreaPolishReport>;
    /** True when this attempt durably changed resident geometry. */
    readonly improved?: boolean;
    /** Exact starting context; used only when completion was fruitless. */
    readonly context?: Readonly<AreaPolishPlanningContext>;
  };

export interface AreaPolishTransition {
  /** Semantic state to retain in the area mirror. */
  readonly pending: boolean;
  /**
   * Value for `AreaMutator.setAreaProperty`, or undefined when no durable
   * write is needed. The empty string makes an existing property logically
   * clear; backends may retain that empty value in their property storage.
   */
  readonly propertyValue?: string;
}

/** Read the mapper-owned boolean property defensively across hand-edited maps. */
export function areaPolishPending(propertyValue: string | undefined): boolean {
  return propertyValue?.trim().toLowerCase() === AREA_POLISH_PENDING_VALUE;
}

/**
 * Reduce one planning observation into the durable eligibility hint.
 *
 * A Worker fixed point is relative to one entry anchor and Map.Local chart, so
 * it cannot retire area-wide eligibility. Every observation therefore keeps
 * the durable hint; the bounded context memo suppresses only exact fruitless
 * repetitions. An attempt cancelled, aborted, or failed mid-flight never
 * reaches this reducer at all, which likewise retains the hint.
 */
export function reduceAreaPolishState(
  currentPending: boolean,
  _event: Readonly<AreaPolishEvent>,
): AreaPolishTransition {
  const pending = true;
  return {
    pending,
    propertyValue: pending === currentPending
      ? undefined
      : pending
      ? AREA_POLISH_PENDING_VALUE
      : "",
  };
}

/** Read the durable memo defensively across hand-edited maps; blank clears it. */
export function polishExhaustedFingerprint(
  propertyValue: string | undefined,
): string | undefined {
  const value = propertyValue?.trim();
  return value ? value : undefined;
}

function boundedUniqueContextKeys(values: readonly string[]): string[] {
  const seen = new Set<string>();
  const newestFirst: string[] = [];
  for (let index = values.length - 1; index >= 0; index -= 1) {
    const value = values[index];
    if (!value || seen.has(value)) continue;
    seen.add(value);
    newestFirst.push(value);
    if (newestFirst.length >= MAX_AREA_POLISH_MEMO_CONTEXTS) break;
  }
  return newestFirst.reverse();
}

/** Parse a durable v2 context set; legacy and malformed nonblank values stay inert. */
export function areaPolishMemo(propertyValue: string | undefined): AreaPolishMemo | undefined {
  const value = polishExhaustedFingerprint(propertyValue);
  if (!value) return undefined;
  try {
    const parsed = JSON.parse(value) as unknown;
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      const record = parsed as Record<string, unknown>;
      if (record.v === AREA_POLISH_MEMO_SCHEMA_VERSION &&
        typeof record.g === "string" && record.g.length > 0 &&
        Array.isArray(record.c) && record.c.every((entry) =>
          typeof entry === "string" && /^[0-9a-f]{32}$/.test(entry)
        )) {
        const contexts = boundedUniqueContextKeys(record.c as string[]);
        if (contexts.length > 0) {
          return {
            kind: "contexts",
            geometryFingerprint: record.g,
            contextKeys: contexts,
          };
        }
      }
    }
  } catch {
    // The old property was the raw resident-geometry JSON, not a wrapper.
  }
  return { kind: "legacy", propertyValue: value };
}

/** Canonical durable representation used for semantic no-op write detection. */
export function areaPolishMemoPropertyValue(memo: Readonly<AreaPolishMemo>): string {
  return memo.kind === "legacy"
    ? memo.propertyValue
    : JSON.stringify({
      v: AREA_POLISH_MEMO_SCHEMA_VERSION,
      g: memo.geometryFingerprint,
      c: memo.contextKeys,
    });
}

/**
 * True when a completed attempt stopped only at deterministic work ceilings
 * or genuinely exhausted frontiers without reaching the geometric fixed
 * point. A wall-deadline stop depends on machine speed, and a cancellation or
 * error reflects this run rather than the topology, so none of those prove
 * that retrying the same geometry is futile.
 */
export function reportsCeilingExhaustion(
  report: Readonly<AreaPolishReport> | undefined,
): boolean {
  const deterministicCutoff = report?.cutoff === "restarts" ||
    report?.cutoff === "layouts" || report?.cutoff === "extensions" ||
    report?.cutoff === "masks" || report?.polishCutoff === "tournaments" ||
    report?.polishCutoff === "passes" ||
    report?.extensionSearch?.exhausted === true || report?.crossingRepair?.exhausted === true;
  return report !== undefined && deterministicCutoff &&
    report.geometricFixedPoint !== true &&
    report.cutoff !== "time" &&
    report.polishCutoff !== "time" &&
    report.polishCutoff !== "error" &&
    report.extensionSearch?.cancelled !== true &&
    report.crossingRepair?.cancelled !== true;
}

export interface AreaPolishMemoTransition {
  /** Bounded context set to retain in the area mirror, or undefined when clear. */
  readonly memo: AreaPolishMemo | undefined;
  /**
   * Value for `AreaMutator.setAreaProperty`, or undefined when no durable
   * write is needed. The empty string makes an existing property logically
   * clear; backends may retain that empty value in their property storage.
   */
  readonly propertyValue?: string;
}

/**
 * Reduce one planning observation into the durable exhausted-attempt memo.
 *
 * A context-relative fixed point records the exact final context it proved;
 * a fruitless deterministic ceiling records the exact starting context it
 * exhausted. Durable improvement without a fixed-point proof clears every old
 * context because the next attempt starts from new geometry. Fresh prompt-lane
 * growth also clears the memo. Every other observation leaves it untouched,
 * so a cancelled or wall-deadlined pass can never add a suppression a genuine
 * polish opportunity does not deserve.
 */
export function reduceAreaPolishMemo(
  currentMemo: Readonly<AreaPolishMemo> | undefined,
  event: Readonly<AreaPolishEvent>,
): AreaPolishMemoTransition {
  let memo = currentMemo as AreaPolishMemo | undefined;
  if (event.kind === "topology-deferred") {
    memo = undefined;
  } else if (event.kind === "polish-completed") {
    const fixedPoint = event.report?.geometricFixedPoint === true;
    const memoizableCeiling = event.improved !== true && reportsCeilingExhaustion(event.report);
    if (event.improved === true) {
      memo = undefined;
    }
    if (event.context && (fixedPoint || memoizableCeiling)) {
      const currentContexts = event.improved !== true &&
          currentMemo?.kind === "contexts" &&
          currentMemo.geometryFingerprint === event.context.geometryFingerprint
        ? currentMemo.contextKeys
        : [];
      if (!currentContexts.includes(event.context.key)) {
        memo = {
          kind: "contexts",
          geometryFingerprint: event.context.geometryFingerprint,
          contextKeys: boundedUniqueContextKeys([
            ...currentContexts,
            event.context.key,
          ]),
        };
      }
    }
  }
  const before = currentMemo && areaPolishMemoPropertyValue(currentMemo);
  const after = memo && areaPolishMemoPropertyValue(memo);
  return {
    memo,
    propertyValue: before === after ? undefined : after ?? "",
  };
}

/** Suppress only a context whose geometry, anchor/chart, generation, and budgets all match. */
export function polishRetrySuppressed(
  memo: Readonly<AreaPolishMemo> | undefined,
  context: Readonly<AreaPolishPlanningContext>,
): boolean {
  return memo?.kind === "contexts" &&
    memo.geometryFingerprint === context.geometryFingerprint &&
    memo.contextKeys.includes(context.key);
}

export interface AreaPolishEntryObservation {
  readonly entered: boolean;
  readonly retry: boolean;
  /** Departed area on a genuine cross-area entry; absent on startup/same-area movement. */
  readonly previousAreaKey: string | undefined;
}

/**
 * Turns a stream of current-area observations into one passive retry per
 * entry. Movement between rooms in the same area cannot repeatedly restart an
 * expensive polish; leaving and later returning makes the durable hint
 * eligible again.
 */
export class AreaPolishEntryTracker {
  #currentAreaKey: string | undefined;
  #retryAreaKey: string | undefined;

  get currentAreaKey(): string | undefined {
    return this.#currentAreaKey;
  }

  /** Current area whose one passive attempt for this visit is still unused. */
  get retryAreaKey(): string | undefined {
    return this.#retryAreaKey;
  }

  observe(
    areaKey: string,
    pending: boolean,
    polishEnabled = true,
  ): AreaPolishEntryObservation {
    const previousAreaKey = this.#currentAreaKey;
    const entered = previousAreaKey !== areaKey;
    this.#currentAreaKey = areaKey;
    if (entered) {
      this.#retryAreaKey = pending && polishEnabled ? areaKey : undefined;
    }
    return {
      entered,
      retry: this.#retryAreaKey === areaKey && entered,
      previousAreaKey: entered ? previousAreaKey : undefined,
    };
  }

  /** New topology may justify one fresh attempt even without leaving the area. */
  markPending(areaKey: string, polishEnabled = true): void {
    if (polishEnabled && this.#currentAreaKey === areaKey) {
      this.#retryAreaKey = areaKey;
    }
  }

  /** Consume the current visit's attempt before starting cancelable work. */
  consumeRetry(areaKey: string): boolean {
    if (this.#retryAreaKey !== areaKey) return false;
    this.#retryAreaKey = undefined;
    return true;
  }

  clear(): void {
    this.#currentAreaKey = undefined;
    this.#retryAreaKey = undefined;
  }
}

/**
 * Observational equivalence for cloned snapshot payloads: the same object, or
 * byte-equal JSON. Serialization order can make equal observations compare
 * unequal, which only withholds a restoration; a false equality cannot occur.
 * Cost is proportional to the snapshot payload, never to the live area.
 */
export function equivalentSnapshotPayloads(a: unknown, b: unknown): boolean {
  return a === b || JSON.stringify(a) === JSON.stringify(b);
}

/** Per-visit bookkeeping one cancelable polish pass consumed before it ran. */
export interface QuietPolishClaim {
  readonly retryConsumed: boolean;
  readonly deferredRemoved: boolean;
  /** The pass durably committed at least one progressive improvement. */
  readonly progressed?: boolean;
}

/**
 * Tracks what each quiet polish pass consumed, keyed by the snapshot the pass
 * was planning. When the displacing snapshot satisfies the injected
 * equivalence — the mapper accepts any displacement that stays within the
 * polished area — the claims are returned for restoration instead of
 * forfeiting the visit's attempt. Any other abort settles to nothing, keeping
 * the forfeit-until-reentry posture, and a completed polish discharges its
 * claim so a later pass over the same snapshot cannot resurrect a spent
 * attempt.
 */
export class QuietPolishClaims<Snapshot extends object> {
  readonly #equivalent: (aborted: Snapshot, incoming: Snapshot) => boolean;
  readonly #claims = new WeakMap<Snapshot, Map<string, QuietPolishClaim>>();

  constructor(
    equivalent: (aborted: Snapshot, incoming: Snapshot) => boolean =
      equivalentSnapshotPayloads,
  ) {
    this.#equivalent = equivalent;
  }

  /**
   * Merge one area's consumed bookkeeping into the pass's claim. A retried
   * attempt within one pass records nothing new, so the original claim stands.
   */
  record(snapshot: Snapshot, areaKey: string, claim: QuietPolishClaim): void {
    if (!claim.retryConsumed && !claim.deferredRemoved) return;
    const byArea = this.#claims.get(snapshot) ?? new Map<string, QuietPolishClaim>();
    const existing = byArea.get(areaKey);
    byArea.set(
      areaKey,
      existing
        ? {
          retryConsumed: existing.retryConsumed || claim.retryConsumed,
          deferredRemoved: existing.deferredRemoved || claim.deferredRemoved,
          ...(existing.progressed || claim.progressed ? { progressed: true } : {}),
        }
        : claim,
    );
    this.#claims.set(snapshot, byArea);
  }

  /**
   * Note one durable progressive improvement committed by the recorded pass.
   * Progress makes a later abort of the pass fruitful rather than fruitless:
   * the durable ratchet means a resumed search starts from a strictly better
   * map. Without a recorded claim there is nothing an abort could restore, so
   * there is nothing to mark.
   */
  markProgress(snapshot: Snapshot, areaKey: string): void {
    const byArea = this.#claims.get(snapshot);
    const existing = byArea?.get(areaKey);
    if (!byArea || !existing || existing.progressed) return;
    byArea.set(areaKey, { ...existing, progressed: true });
  }

  /** A completed polish genuinely spent its claim; nothing remains to restore. */
  discharge(snapshot: Snapshot, areaKey: string): void {
    const byArea = this.#claims.get(snapshot);
    if (!byArea) return;
    byArea.delete(areaKey);
    if (byArea.size === 0) this.#claims.delete(snapshot);
  }

  /**
   * Resolve an aborted pass's claims. The claims are always cleared; they are
   * returned only when the incoming snapshot satisfies the injected
   * equivalence against the one the aborted pass was planning.
   */
  settle(aborted: Snapshot, incoming: Snapshot): ReadonlyMap<string, QuietPolishClaim> {
    const byArea = this.#claims.get(aborted);
    this.#claims.delete(aborted);
    if (!byArea || !this.#equivalent(aborted, incoming)) return new Map();
    return byArea;
  }
}

/**
 * Consecutive fruitless resumptions one visit may spend per area. Each unit
 * costs a full quiet window plus a started-and-displaced Worker search, so
 * the ceiling caps a visit's abort-restart churn at a handful of wasted
 * searches while comfortably covering ordinary walk-pause rhythms; any
 * durable improvement restarts the allowance.
 */
export const MAX_FRUITLESS_QUIET_RESUMES = 8;

/**
 * Bounds how often one visit may restore a displaced quiet pass that has yet
 * to commit anything durable. Movement inside an area displaces the active
 * pass, and restoring the attempt each time keeps polish alive for an active
 * player — but an area that cannot improve would otherwise restart a full
 * search on every step forever. A displaced pass that committed a progressive
 * improvement restarts its area's allowance, as do topology growth, completed
 * polish, and area re-entry at their call sites. Once the allowance is spent
 * the visit forfeits, and the durable pending hint remains the backstop for
 * the next entry. This is the within-visit half of the churn story; the
 * exhausted-fingerprint memo separately ends cross-visit retries over
 * geometry a completed attempt proved unimprovable.
 */
export class QuietResumeBudget {
  readonly #limit: number;
  readonly #fruitless = new Map<string, number>();

  constructor(limit = MAX_FRUITLESS_QUIET_RESUMES) {
    this.#limit = limit;
  }

  /**
   * Charge one displaced pass against its area's allowance. A pass that made
   * durable progress restarts the allowance and always resumes; a fruitless
   * one consumes a unit and resumes only while units remain.
   */
  allowResume(areaKey: string, progressed: boolean): boolean {
    if (progressed) {
      this.#fruitless.delete(areaKey);
      return true;
    }
    const used = (this.#fruitless.get(areaKey) ?? 0) + 1;
    this.#fruitless.set(areaKey, used);
    return used <= this.#limit;
  }

  /** Fresh evidence for the area — growth, completion, re-entry — restarts it. */
  reset(areaKey: string): void {
    this.#fruitless.delete(areaKey);
  }

  clear(): void {
    this.#fruitless.clear();
  }
}
