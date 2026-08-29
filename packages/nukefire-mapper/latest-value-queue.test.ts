import assert from "node:assert/strict";
import test from "node:test";
import {
  LatestValueQueue,
  type LatestValueQueueTimers,
} from "./latest-value-queue.ts";

interface TimerEntry {
  at: number;
  callback: () => void;
  cleared: boolean;
}

/** Deterministic clock whose timers observe the same manually advanced time. */
class ManualClock {
  now = 0;
  readonly #entries: TimerEntry[] = [];
  readonly timers: LatestValueQueueTimers = {
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

function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

async function settle(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

test("serializes the active value and coalesces pending work to the latest", async () => {
  const first = deferred();
  const consumed: number[] = [];
  const queue = new LatestValueQueue<number>(async (value) => {
    consumed.push(value);
    if (value === 1) await first.promise;
  });

  queue.push(1);
  queue.push(2);
  queue.push(3);
  assert.deepEqual(consumed, [1]);
  first.resolve();
  await queue.flush();
  assert.deepEqual(consumed, [1, 3]);
});

test("retains a consumer failure and rejects flush without starting newer work", async () => {
  const started = deferred();
  const release = deferred();
  const consumed: number[] = [];
  const observed: unknown[] = [];
  const failure = new Error("write failed");
  const queue = new LatestValueQueue<number>(async (value) => {
    consumed.push(value);
    started.resolve();
    await release.promise;
    throw failure;
  }, (error) => observed.push(error));

  queue.push(1);
  await started.promise;
  queue.push(2);
  release.resolve();
  await assert.rejects(queue.flush(), failure);
  queue.push(3);
  await settle();
  assert.deepEqual(consumed, [1]);
  assert.deepEqual(observed, [failure]);
});

function pacedQueue(clock: ManualClock, minIntervalMs = 1_500): {
  queue: LatestValueQueue<number>;
  consumed: Array<[number, number]>;
} {
  const consumed: Array<[number, number]> = [];
  const queue = new LatestValueQueue<number>(
    async (value) => {
      consumed.push([value, clock.now]);
    },
    undefined,
    { minIntervalMs, timers: clock.timers, now: () => clock.now },
  );
  return { queue, consumed };
}

test("the floor delays every consume after the first, coalescing to the newest", async () => {
  const clock = new ManualClock();
  const { queue, consumed } = pacedQueue(clock);

  queue.push(1);
  await settle();
  assert.deepEqual(consumed, [[1, 0]]);

  clock.advance(100);
  queue.push(2);
  queue.push(3);
  await settle();
  assert.deepEqual(consumed, [[1, 0]]);

  clock.advance(1_399);
  await settle();
  assert.deepEqual(consumed, [[1, 0]]);

  clock.advance(1);
  await settle();
  assert.deepEqual(consumed, [[1, 0], [3, 1_500]]);

  // The next value waits out a fresh interval measured from the last consume.
  queue.push(4);
  await settle();
  assert.deepEqual(consumed, [[1, 0], [3, 1_500]]);
  clock.advance(1_500);
  await settle();
  assert.deepEqual(consumed, [[1, 0], [3, 1_500], [4, 3_000]]);
});

test("flush drains a floor-delayed value immediately", async () => {
  const clock = new ManualClock();
  const { queue, consumed } = pacedQueue(clock);

  queue.push(1);
  await settle();
  clock.advance(200);
  queue.push(2);
  await settle();
  assert.deepEqual(consumed, [[1, 0]]);

  await queue.flush();
  assert.deepEqual(consumed, [[1, 0], [2, 200]]);
});

test("discarding a floor-delayed value leaves nothing for the trailing drain", async () => {
  const clock = new ManualClock();
  const { queue, consumed } = pacedQueue(clock);

  queue.push(1);
  await settle();
  clock.advance(200);
  queue.push(2);
  queue.discardPending();
  clock.advance(1_400);
  await settle();
  await queue.flush();
  assert.deepEqual(consumed, [[1, 0]]);
});
