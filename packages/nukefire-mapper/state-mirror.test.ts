import assert from "node:assert/strict";
import test from "node:test";
import {
  LAYOUT_STATE_PUBLISH_INTERVAL_MS,
  ThrottledMirror,
  type MirrorTimers,
} from "./state-mirror.ts";

interface TimerEntry {
  at: number;
  callback: () => void;
  cleared: boolean;
}

/** Deterministic clock whose timers observe the same manually advanced time. */
class ManualClock {
  now = 0;
  readonly #entries: TimerEntry[] = [];
  readonly timers: MirrorTimers = {
    set: (callback, delayMs) => {
      const entry = { at: this.now + delayMs, callback, cleared: false };
      this.#entries.push(entry);
      return entry;
    },
    clear: (handle) => {
      (handle as TimerEntry).cleared = true;
    },
  };

  advance(ms: number): void {
    this.now += ms;
    for (;;) {
      const due = this.#entries
        .filter((entry) => !entry.cleared && entry.at <= this.now)
        .sort((left, right) => left.at - right.at)[0];
      if (!due) return;
      due.cleared = true;
      due.callback();
    }
  }
}

function throttled(clock: ManualClock): {
  mirror: ThrottledMirror<number>;
  published: Array<[number, number]>;
} {
  const published: Array<[number, number]> = [];
  const mirror = new ThrottledMirror<number>(
    (value) => published.push([value, clock.now]),
    LAYOUT_STATE_PUBLISH_INTERVAL_MS,
    { timers: clock.timers, now: () => clock.now },
  );
  return { mirror, published };
}

test("the mirror interval is the documented five-per-second floor", () => {
  assert.equal(LAYOUT_STATE_PUBLISH_INTERVAL_MS, 200);
});

test("the first value publishes immediately and a burst coalesces to the newest", () => {
  const clock = new ManualClock();
  const { mirror, published } = throttled(clock);

  mirror.set(1);
  assert.deepEqual(published, [[1, 0]]);

  // A burst inside the interval publishes nothing until the trailing edge.
  clock.advance(50);
  mirror.set(2);
  mirror.set(3);
  clock.advance(50);
  mirror.set(4);
  assert.deepEqual(published, [[1, 0]]);

  clock.advance(100);
  assert.deepEqual(published, [[1, 0], [4, 200]]);
});

test("the trailing publish restarts the interval for later values", () => {
  const clock = new ManualClock();
  const { mirror, published } = throttled(clock);

  mirror.set(1);
  clock.advance(150);
  mirror.set(2);
  clock.advance(50);
  assert.deepEqual(published, [[1, 0], [2, 200]]);

  clock.advance(100);
  mirror.set(3);
  assert.deepEqual(published, [[1, 0], [2, 200]]);
  clock.advance(100);
  assert.deepEqual(published, [[1, 0], [2, 200], [3, 400]]);
});

test("a quiet stream publishes every value without delay", () => {
  const clock = new ManualClock();
  const { mirror, published } = throttled(clock);

  mirror.set(1);
  clock.advance(200);
  mirror.set(2);
  clock.advance(500);
  mirror.set(3);
  assert.deepEqual(published, [[1, 0], [2, 200], [3, 700]]);
});
