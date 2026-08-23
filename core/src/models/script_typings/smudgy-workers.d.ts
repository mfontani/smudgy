// =============================================================================
//  smudgy workers -- TypeScript declarations  (GENERATED -- DO NOT EDIT)
// =============================================================================
//  smudgy writes and overwrites this file every time a session starts. It teaches
//  VS Code (and any TypeScript-aware editor) about the `Worker` surface available
//  to scripts: background scripts on their own thread, deliberately narrower than
//  the browser DOM lib.
//
//  Edits here are lost on the next launch.
// =============================================================================

/**
 * A script running in the background on its own thread. Heavy work (say,
 * tallying a large capture or searching a big data set) runs there without
 * stalling triggers, aliases, or the display. A worker computes and exchanges
 * messages with the script that created it; it has no access to the network,
 * files, or the smudgy API.
 */
interface Worker extends EventTarget {
  /**
   * Called for each message the worker posts back. The value the worker sent
   * is `event.data`.
   */
  onmessage: ((this: Worker, event: MessageEvent) => unknown) | null;

  /**
   * Called when a message from the worker could not be delivered because its
   * value could not be copied between threads.
   */
  onmessageerror: ((this: Worker, event: MessageEvent) => unknown) | null;

  /**
   * Called when an error inside the worker goes uncaught. `event.message`
   * describes the error.
   */
  onerror: ((this: Worker, event: ErrorEvent) => unknown) | null;

  /**
   * Sends a value to the worker. The value is copied, so later changes on
   * either side are not seen by the other. Objects listed in `transfer` (an
   * `ArrayBuffer`, for example) are handed over instead of copied and become
   * unusable on the sending side.
   */
  postMessage(message: unknown, transfer?: Transferable[]): void;

  /**
   * Listens for the worker's `message`, `messageerror`, or `error` events.
   * The `onmessage`, `onmessageerror`, and `onerror` properties receive the
   * same events.
   */
  addEventListener(
    type: "message" | "messageerror",
    listener: (this: Worker, event: MessageEvent) => unknown,
    options?: boolean | AddEventListenerOptions,
  ): void;
  addEventListener(
    type: "error",
    listener: (this: Worker, event: ErrorEvent) => unknown,
    options?: boolean | AddEventListenerOptions,
  ): void;

  /** Removes a listener added with `addEventListener`. */
  removeEventListener(
    type: "message" | "messageerror",
    listener: (this: Worker, event: MessageEvent) => unknown,
    options?: boolean | EventListenerOptions,
  ): void;
  removeEventListener(
    type: "error",
    listener: (this: Worker, event: ErrorEvent) => unknown,
    options?: boolean | EventListenerOptions,
  ): void;

  /**
   * Stops the worker immediately. Work in progress is abandoned and no
   * further messages arrive. A stopped worker no longer counts toward the
   * running-worker limit.
   */
  terminate(): void;
}

/**
 * Runs a script in the background on another thread, so heavy work never
 * freezes your session. Talk to it with `postMessage` and `onmessage`:
 *
 * ```ts
 * import { createTrigger, echo, line } from "smudgy:core";
 *
 * // A trigger collects the session's kill lines as they arrive...
 * const capturedLines: string[] = [];
 * createTrigger(/is DEAD!/, () => { capturedLines.push(line.text); });
 *
 * // ...and a worker tallies the pile off-thread.
 * const source = `
 *   self.onmessage = (e) => {
 *     const kills = e.data.filter((line) => line.includes(" is DEAD!")).length;
 *     self.postMessage(kills);
 *   };
 * `;
 * const tally = new Worker(
 *   "data:text/javascript," + encodeURIComponent(source),
 *   { type: "module" },
 * );
 * tally.onmessage = (e) => {
 *   echo(`Kills this session: ${e.data}`); // prints the worker's answer
 *   tally.terminate();
 * };
 * tally.postMessage(capturedLines);
 * ```
 */
declare var Worker: {
  readonly prototype: Worker;

  /**
   * Creates a worker and starts it running. `specifier` is a `file:` URL
   * naming a module on disk, or a `data:` URL carrying the JavaScript itself
   * (as in the example above). `options` must include `type: "module"`;
   * `name` is an optional label, readable inside the worker as `self.name`.
   * Up to 8 workers can be running at once; creating another while 8 are
   * live throws.
   */
  new (specifier: string | URL, options: { type: "module"; name?: string }): Worker;
};
