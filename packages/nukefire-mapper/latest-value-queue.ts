const EMPTY = Symbol("empty latest-value queue");
const NO_FAILURE = Symbol("no latest-value queue failure");

export interface LatestValueQueueTimers {
  set(callback: () => void, delayMs: number): unknown;
  clear(handle: unknown): void;
}

const defaultTimers: LatestValueQueueTimers = {
  set: (callback, delayMs) => setTimeout(callback, delayMs),
  clear: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};

export interface LatestValueQueueOptions {
  /**
   * Minimum quiet time between the completion of one consume and the start of
   * the next. The first consume is never delayed, and `flush()` drains any
   * retained value immediately: pacing shapes the steady stream of
   * intermediate values, never the first or the final result. Values arriving
   * during the wait still coalesce latest-wins, so the floor only delays the
   * drain rather than growing a backlog.
   */
  minIntervalMs?: number;
  timers?: LatestValueQueueTimers;
  now?: () => number;
}

/**
 * Serialize an asynchronous consumer while retaining only the newest value
 * that arrived during the active call. This gives progressive UI work natural
 * backpressure without making a CPU producer wait for map persistence.
 */
export class LatestValueQueue<T> {
  readonly #consume: (value: T) => Promise<void>;
  readonly #onError: ((error: unknown) => void) | undefined;
  readonly #minIntervalMs: number;
  readonly #timers: LatestValueQueueTimers;
  readonly #now: () => number;
  #pending: T | typeof EMPTY = EMPTY;
  #running: Promise<void> | undefined;
  #delayHandle: unknown | undefined;
  #lastConsumedAt: number | undefined;
  #failure: unknown | typeof NO_FAILURE = NO_FAILURE;

  constructor(
    consume: (value: T) => Promise<void>,
    onError?: (error: unknown) => void,
    options: LatestValueQueueOptions = {},
  ) {
    this.#consume = consume;
    this.#onError = onError;
    this.#minIntervalMs = options.minIntervalMs ?? 0;
    this.#timers = options.timers ?? defaultTimers;
    this.#now = options.now ?? (() => Date.now());
  }

  push(value: T): void {
    if (this.#failure !== NO_FAILURE) return;
    this.#pending = value;
    // An armed interval timer already owns the next drain; replacing the
    // retained value is all a newer arrival needs to do.
    if (!this.#running && this.#delayHandle === undefined) this.#drainOrDelay();
  }

  discardPending(): void {
    this.#pending = EMPTY;
  }

  /**
   * Wait until every retained value is consumed. The interval floor is
   * bypassed: a flush marks an authoritative boundary — typically the final
   * plan — which must never wait on pacing meant for intermediate values.
   */
  async flush(): Promise<void> {
    for (;;) {
      if (this.#delayHandle !== undefined) {
        this.#timers.clear(this.#delayHandle);
        this.#delayHandle = undefined;
      }
      if (this.#running === undefined) {
        if (this.#pending === EMPTY || this.#failure !== NO_FAILURE) break;
        this.#running = this.#drain(true);
      }
      await this.#running;
    }
    if (this.#failure !== NO_FAILURE) throw this.#failure;
  }

  /** Remaining quiet time the floor still owes since the previous consume. */
  #floorDelayMs(): number {
    if (this.#minIntervalMs <= 0 || this.#lastConsumedAt === undefined) return 0;
    return this.#minIntervalMs - (this.#now() - this.#lastConsumedAt);
  }

  #drainOrDelay(): void {
    const delayMs = this.#floorDelayMs();
    if (delayMs <= 0) {
      this.#running = this.#drain(false);
      return;
    }
    const handle = this.#timers.set(() => {
      if (this.#delayHandle !== handle) return;
      this.#delayHandle = undefined;
      if (this.#pending !== EMPTY && !this.#running && this.#failure === NO_FAILURE) {
        this.#running = this.#drain(false);
      }
    }, delayMs);
    this.#delayHandle = handle;
  }

  async #drain(ignoreFloor: boolean): Promise<void> {
    try {
      while (this.#pending !== EMPTY) {
        if (!ignoreFloor && this.#floorDelayMs() > 0) break;
        const value = this.#pending;
        this.#pending = EMPTY;
        await this.#consume(value);
        this.#lastConsumedAt = this.#now();
      }
    } catch (error) {
      this.#pending = EMPTY;
      this.#failure = error;
      try {
        this.#onError?.(error);
      } catch {
        // Error observation must not obscure the retained consumer failure.
      }
    } finally {
      this.#running = undefined;
      if (this.#pending !== EMPTY && this.#failure === NO_FAILURE) {
        if (ignoreFloor) this.#running = this.#drain(true);
        else this.#drainOrDelay();
      }
    }
  }
}
