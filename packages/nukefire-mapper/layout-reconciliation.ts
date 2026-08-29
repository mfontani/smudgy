export interface ReconciliationPosition {
  x: number;
  y: number;
  level: number;
}

export interface ReconciliationRoom {
  position: ReconciliationPosition;
}

export interface ReconciliationUpdate<Key> {
  id: string;
  key: Key;
  position: ReconciliationPosition;
}

function samePosition(
  left: Readonly<ReconciliationPosition>,
  right: Readonly<ReconciliationPosition>,
): boolean {
  return left.x === right.x && left.y === right.y && left.level === right.level;
}

/**
 * Diff a complete layout against the live post-checkpoint room mirror.
 *
 * By default this considers every planned resident. A caller may instead pass
 * the union of final `movedExisting` ids and ids durably touched by progressive
 * candidates. That union matters because `movedExisting` is relative to the
 * original request: a final plan which restores a checkpoint move can omit the
 * room even though its live post-checkpoint coordinate still needs a write.
 */
export function reconciliationUpdates<Room extends ReconciliationRoom, Key>(
  roomsById: ReadonlyMap<string, Readonly<Room>>,
  plannedPositions: ReadonlyMap<string, Readonly<ReconciliationPosition>>,
  keyOf: (room: Readonly<Room>) => Key,
  ids: Iterable<string> = roomsById.keys(),
): ReconciliationUpdate<Key>[] {
  const updates: ReconciliationUpdate<Key>[] = [];
  for (const id of ids) {
    const room = roomsById.get(id);
    if (!room) continue;
    const position = plannedPositions.get(id);
    if (!position || samePosition(room.position, position)) continue;
    updates.push({ id, key: keyOf(room), position });
  }
  return updates;
}
