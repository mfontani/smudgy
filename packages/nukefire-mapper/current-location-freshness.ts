import { isUsableVnum } from "./model.ts";

/**
 * Opaque authority captured when NukeFire reports a new current room.
 * Delayed topology and layout work must retain its observation and may only
 * publish a marker while that exact observation is still current.
 */
export interface CurrentLocationObservation {
  readonly vnum: number;
  readonly ticket: number;
}

/**
 * Separates lossless snapshot ingestion from latest-only player-location
 * publication. Observing an unmapped room still supersedes older publishers;
 * the room can be published later once topology creates or discovers it.
 */
export class CurrentLocationFreshness {
  #nextTicket = 0;
  #current: CurrentLocationObservation | undefined;

  observe(vnum: number): CurrentLocationObservation | undefined {
    if (!isUsableVnum(vnum)) return undefined;
    if (this.#current?.vnum === vnum) return this.#current;
    const observation = { vnum, ticket: ++this.#nextTicket };
    this.#current = observation;
    return observation;
  }

  isCurrent(observation: Readonly<CurrentLocationObservation>): boolean {
    return this.#current?.ticket === observation.ticket &&
      this.#current.vnum === observation.vnum;
  }

  /** Resolve and publish the latest observation without exposing mutable state. */
  publishIfCurrent<Value>(
    resolve: (vnum: number) => Value | undefined,
    publish: (
      value: Value,
      observation: Readonly<CurrentLocationObservation>,
    ) => void,
  ): boolean {
    const observation = this.#current;
    if (!observation) return false;
    const value = resolve(observation.vnum);
    if (value === undefined || !this.isCurrent(observation)) return false;
    publish(value, observation);
    return true;
  }

  clear(): void {
    this.#current = undefined;
  }
}
