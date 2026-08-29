export const DEFAULT_REFLOW_QUIET_MS = 350;

export type SnapshotLatencyLane = "topology" | "full-reflow";

export interface SnapshotLaneTimers {
  set(callback: () => void, delayMs: number): unknown;
  clear(handle: unknown): void;
}

export interface SnapshotLatencyLanesOptions<Snapshot> {
  /** Coalesce only adjacent, not-yet-ingested snapshots for the same center. */
  snapshotKey(snapshot: Snapshot): string | number;
  /** Runs synchronously and independently of either asynchronous layout lane. */
  followCurrent(snapshot: Snapshot): void;
  /** Ingest discoveries promptly; this callback must never move existing rooms. */
  runTopology(snapshot: Snapshot): Promise<void>;
  /** Reflow existing rooms only after the configured quiet window. */
  runFullReflow(snapshot: Snapshot, signal: AbortSignal, generation: number): Promise<void>;
  /**
   * Observes an enqueued snapshot displacing the active full reflow, with the
   * snapshot the aborted reflow was processing and the newly enqueued one.
   * Called synchronously, at most once per reflow task; `stop()` aborts
   * without notifying because its generation bump already invalidates the run.
   */
  onFullReflowAborted?(aborted: Snapshot, incoming: Snapshot): void;
  onError?(lane: SnapshotLatencyLane, snapshot: Snapshot, error: unknown): void;
  quietWindowMs?: number;
  timers?: SnapshotLaneTimers;
}

const defaultTimers: SnapshotLaneTimers = {
  set: (callback, delayMs) => setTimeout(callback, delayMs),
  clear: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};

interface TopologyTask {
  generation: number;
}

interface FullReflowTask<Snapshot> {
  generation: number;
  snapshot: Snapshot;
  controller: AbortController;
}

/**
 * Coordinates the mapper's latency-sensitive work without knowing anything
 * about Smudgy or NukeFire. Topology remains serialized and lossless across
 * distinct centers; only an obsolete full reflow is interruptible.
 */
export class SnapshotLatencyLanes<Snapshot> {
  readonly #options: Required<Pick<SnapshotLatencyLanesOptions<Snapshot>, "quietWindowMs">> &
    Omit<SnapshotLatencyLanesOptions<Snapshot>, "quietWindowMs" | "timers"> & {
      timers: SnapshotLaneTimers;
    };
  readonly #pendingTopology: Snapshot[] = [];
  #latestSnapshot: Snapshot | undefined;
  #reflowSnapshot: Snapshot | undefined;
  #quietTimer: unknown | undefined;
  #quietReady = false;
  #topologyTask: TopologyTask | undefined;
  #fullReflowTask: FullReflowTask<Snapshot> | undefined;
  #generation = 0;
  #started = false;

  constructor(options: SnapshotLatencyLanesOptions<Snapshot>) {
    this.#options = {
      ...options,
      quietWindowMs: options.quietWindowMs ?? DEFAULT_REFLOW_QUIET_MS,
      timers: options.timers ?? defaultTimers,
    };
  }

  get pendingTopologyCount(): number {
    return this.#pendingTopology.length;
  }

  get fullReflowActive(): boolean {
    return this.#fullReflowTask !== undefined;
  }

  get generation(): number {
    return this.#generation;
  }

  start(): void {
    if (this.#started) return;
    this.#started = true;
    this.#generation += 1;
    this.#drive();
  }

  stop(): void {
    if (!this.#started) return;
    this.#started = false;
    this.#generation += 1;
    this.#pendingTopology.length = 0;
    this.#latestSnapshot = undefined;
    this.#reflowSnapshot = undefined;
    this.#quietReady = false;
    this.#clearQuietTimer();
    this.#fullReflowTask?.controller.abort();
  }

  enqueue(snapshot: Snapshot): void {
    if (!this.#started) return;

    // Current-room following is deliberately outside both layout lanes. It
    // remains immediate even if an older topology write or reflow is in flight.
    this.#options.followCurrent(snapshot);

    this.#latestSnapshot = snapshot;
    this.#reflowSnapshot = undefined;
    const displaced = this.#fullReflowTask;
    if (displaced && !displaced.controller.signal.aborted) {
      displaced.controller.abort();
      this.#options.onFullReflowAborted?.(displaced.snapshot, snapshot);
    }
    this.#armQuietTimer();

    const last = this.#pendingTopology.at(-1);
    if (last !== undefined && this.#options.snapshotKey(last) === this.#options.snapshotKey(snapshot)) {
      this.#pendingTopology[this.#pendingTopology.length - 1] = snapshot;
    } else {
      this.#pendingTopology.push(snapshot);
    }
    this.#drive();
  }

  #armQuietTimer(): void {
    this.#clearQuietTimer();
    this.#quietReady = false;
    const generation = this.#generation;
    const handle = this.#options.timers.set(() => {
      if (this.#quietTimer !== handle) return;
      this.#quietTimer = undefined;
      if (!this.#started || this.#generation !== generation) return;
      this.#quietReady = true;
      this.#drive();
    }, this.#options.quietWindowMs);
    this.#quietTimer = handle;
  }

  #clearQuietTimer(): void {
    if (this.#quietTimer === undefined) return;
    this.#options.timers.clear(this.#quietTimer);
    this.#quietTimer = undefined;
  }

  #drive(): void {
    if (!this.#started || this.#topologyTask || this.#fullReflowTask) return;
    if (this.#pendingTopology.length > 0) {
      this.#startTopologyDrain();
      return;
    }
    if (this.#quietReady && this.#reflowSnapshot !== undefined) this.#startFullReflow();
  }

  #startTopologyDrain(): void {
    const task: TopologyTask = { generation: this.#generation };
    this.#topologyTask = task;
    void this.#drainTopology(task);
  }

  async #drainTopology(task: TopologyTask): Promise<void> {
    try {
      while (
        this.#started &&
        this.#generation === task.generation &&
        this.#pendingTopology.length > 0
      ) {
        const snapshot = this.#pendingTopology.shift();
        if (snapshot === undefined) continue;
        try {
          await this.#options.runTopology(snapshot);
          if (
            this.#started &&
            this.#generation === task.generation &&
            this.#latestSnapshot === snapshot
          ) {
            this.#reflowSnapshot = snapshot;
          }
        } catch (error) {
          if (!this.#started || this.#generation !== task.generation) return;
          this.#options.onError?.("topology", snapshot, error);
        }
      }
    } finally {
      if (this.#topologyTask === task) {
        this.#topologyTask = undefined;
        this.#drive();
      }
    }
  }

  #startFullReflow(): void {
    const snapshot = this.#reflowSnapshot;
    if (snapshot === undefined) return;
    this.#quietReady = false;
    const task: FullReflowTask<Snapshot> = {
      generation: this.#generation,
      snapshot,
      controller: new AbortController(),
    };
    this.#fullReflowTask = task;
    void this.#runFullReflow(task);
  }

  async #runFullReflow(task: FullReflowTask<Snapshot>): Promise<void> {
    try {
      await this.#options.runFullReflow(
        task.snapshot,
        task.controller.signal,
        task.generation,
      );
    } catch (error) {
      if (
        !task.controller.signal.aborted &&
        this.#started &&
        this.#generation === task.generation
      ) {
        this.#options.onError?.("full-reflow", task.snapshot, error);
      }
    } finally {
      if (this.#fullReflowTask === task) {
        this.#fullReflowTask = undefined;
        this.#drive();
      }
    }
  }
}
