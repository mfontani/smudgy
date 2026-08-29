import type {
  LayoutPlannerSnapshot,
  LayoutPlannerSubscriber,
} from "./planner-state.ts";

const IDLE_SNAPSHOT: Readonly<LayoutPlannerSnapshot> = Object.freeze({
  sequence: 0,
  status: "idle",
  operation: "none",
  phase: "idle",
  elapsedMs: 0,
  nodes: 0,
  residents: 0,
  edges: 0,
  work: Object.freeze({
    layoutsConsidered: 0,
    compactionAttempts: 0,
    restarts: 0,
    feasibilityChecks: 0,
    rawIncumbents: 0,
    softIncumbents: 0,
    distinctLayouts: 0,
    maskDiversifications: 0,
    separatorStates: 0,
    separatorBranches: 0,
    separatorCyclePrunes: 0,
    crossingsConsidered: 0,
    macrosConsidered: 0,
    pushClosures: 0,
    maxDepth: 0,
    visitedStates: 0,
  }),
});

class MutableLayoutPlannerState {
  #value = IDLE_SNAPSHOT;
  readonly #subscribers = new Set<LayoutPlannerSubscriber>();

  get value(): Readonly<LayoutPlannerSnapshot> {
    return this.#value;
  }

  subscribe(subscriber: LayoutPlannerSubscriber): () => void {
    this.#subscribers.add(subscriber);
    try {
      subscriber(this.#value);
    } catch {
      // Match later publication isolation for the retained initial delivery.
    }
    return () => this.#subscribers.delete(subscriber);
  }

  publish(snapshot: LayoutPlannerSnapshot): void {
    this.#value = Object.freeze({
      ...snapshot,
      work: Object.freeze({ ...snapshot.work }),
      currentQuality: snapshot.currentQuality && Object.freeze({ ...snapshot.currentQuality }),
      standardQuality: snapshot.standardQuality && Object.freeze({ ...snapshot.standardQuality }),
      bestQuality: snapshot.bestQuality && Object.freeze({ ...snapshot.bestQuality }),
    });
    for (const subscriber of this.#subscribers) {
      try {
        subscriber(this.#value);
      } catch {
        // Telemetry must never affect layout correctness or Worker lifecycle.
      }
    }
  }
}

export const mutableLayoutPlannerState = new MutableLayoutPlannerState();
