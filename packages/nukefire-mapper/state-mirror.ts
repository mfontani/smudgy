/**
 * Publish floor for the cross-isolate `layoutState` telemetry mirror. A long
 * constraint search can emit thousands of planner snapshots; five per second
 * is indistinguishable in a human-read panel while eliminating almost all of
 * the cross-isolate serialization and widget re-render cost.
 */
export const LAYOUT_STATE_PUBLISH_INTERVAL_MS = 200;

export interface MirrorTimers {
  set(callback: () => void, delayMs: number): unknown;
  clear(handle: unknown): void;
}

const defaultTimers: MirrorTimers = {
  set: (callback, delayMs) => setTimeout(callback, delayMs),
  clear: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};

export interface ThrottledMirrorOptions {
  timers?: MirrorTimers;
  now?: () => number;
}

/**
 * Rate-limits a high-frequency publisher to at most one publish per interval.
 * The first value publishes immediately; values arriving inside the interval
 * are retained latest-wins and published on the trailing edge, so the final
 * value always lands.
 */
export class ThrottledMirror<T> {
  readonly #publish: (value: T) => void;
  readonly #intervalMs: number;
  readonly #timers: MirrorTimers;
  readonly #now: () => number;
  #retained: T | undefined;
  #hasRetained = false;
  #handle: unknown | undefined;
  #lastPublishedAt: number | undefined;

  constructor(
    publish: (value: T) => void,
    intervalMs: number,
    options: ThrottledMirrorOptions = {},
  ) {
    this.#publish = publish;
    this.#intervalMs = intervalMs;
    this.#timers = options.timers ?? defaultTimers;
    this.#now = options.now ?? (() => Date.now());
  }

  set(value: T): void {
    if (this.#handle !== undefined) {
      this.#retained = value;
      this.#hasRetained = true;
      return;
    }
    const now = this.#now();
    const elapsed = this.#lastPublishedAt === undefined
      ? this.#intervalMs
      : now - this.#lastPublishedAt;
    if (elapsed >= this.#intervalMs) {
      this.#lastPublishedAt = now;
      this.#publish(value);
      return;
    }
    this.#retained = value;
    this.#hasRetained = true;
    this.#handle = this.#timers.set(() => {
      this.#handle = undefined;
      if (!this.#hasRetained) return;
      const retained = this.#retained as T;
      this.#retained = undefined;
      this.#hasRetained = false;
      this.#lastPublishedAt = this.#now();
      this.#publish(retained);
    }, this.#intervalMs - elapsed);
  }
}
