/**
 * Decide whether one room's planned coordinates may be written durably.
 *
 * A pass that ran with `moveExisting === false` cannot relocate a room the
 * mirror already holds, no matter what positions its plan reports: the prompt
 * lane's no-move contract is enforced mapper-side rather than trusted to the
 * Worker's `allowExistingMoves` request flag. New placements always write,
 * and a coordinate already applied durably during planning is never rewritten.
 */
export function coordinateWriteAllowed(
  created: boolean,
  positionApplied: boolean,
  moveExisting: boolean,
): boolean {
  if (positionApplied) return false;
  return created || moveExisting;
}

/**
 * Resident ids the final reconciliation may diff against the live mirror.
 *
 * Progressive checkpoint writes always reconcile — their durable coordinates
 * exist whether or not the final plan restores them — while the plan's own
 * `movedExisting` claims count only when the pass was allowed to move
 * existing rooms.
 */
export function reconcilableResidentIds(
  moveExisting: boolean,
  movedExisting: Iterable<string>,
  appliedPositionIds: Iterable<string>,
): Set<string> {
  const ids = new Set(appliedPositionIds);
  if (moveExisting) {
    for (const id of movedExisting) ids.add(id);
  }
  return ids;
}
