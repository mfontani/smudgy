export interface ReflowPolicy {
  runPlanner: boolean;
  moveExisting: boolean;
  deferExistingReflow: boolean;
}

/** Decide how one latency lane may affect an area's existing coordinates. */
export function reflowPolicy(
  topologyGrowth: boolean,
  deferredReflow: boolean,
  allowExistingReflow: boolean,
  updateCoordinates: boolean,
): ReflowPolicy {
  return {
    runPlanner: topologyGrowth || (allowExistingReflow && deferredReflow),
    moveExisting: allowExistingReflow && updateCoordinates,
    deferExistingReflow: topologyGrowth && !allowExistingReflow && updateCoordinates
      ? true
      : allowExistingReflow
      ? false
      : deferredReflow,
  };
}
