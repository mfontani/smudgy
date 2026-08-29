import {
  planIntegralLayout,
  type LayoutTraceCandidate,
  type LayoutTraceEvent,
} from "./layout.ts";
import { repairIntegralLayoutConstraints } from "./constraint-layout.ts";
import { planLayoutModel } from "./model.ts";
import {
  decodeIntegralLayoutPlan,
  encodeIntegralLayoutPlan,
  encodePlannedLayout,
  LAYOUT_WORKER_PROTOCOL_VERSION,
  serializeLayoutWorkerError,
  type LayoutWorkerRequest,
  type LayoutWorkerResponse,
} from "./worker-protocol.ts";

const MAX_RETAINED_TRACE_EVENTS = 4_096;
// A diagnostic trace is returned only after synchronous planning completes.
// Bound the resident-coordinate payload separately so a long-running repair
// cannot retain thousands of otherwise-complete whole-map incumbents.
const MAX_RETAINED_TRACE_POSITION_ENTRIES = 65_536;

function candidatePositionCount(candidate: LayoutTraceCandidate | undefined): number {
  return candidate?.positions?.length ?? 0;
}

function tracePositionCount(event: LayoutTraceEvent): number {
  if (event.type === "candidate-batch") return candidatePositionCount(event.best);
  if (event.type === "selection") return candidatePositionCount(event.selected);
  if (event.type === "improvement" || event.type === "vacuum" ||
    event.type === "obstruction-repair" || event.type === "bridge-vacuum" ||
    event.type === "crossing-repair") {
    return candidatePositionCount(event.before) + candidatePositionCount(event.after);
  }
  if (event.type === "obstruction-candidates") {
    return event.candidates.reduce(
      (total, candidate) => total + candidatePositionCount(candidate.result),
      0,
    );
  }
  if (event.type === "constraint-improvement") {
    return candidatePositionCount(event.candidate);
  }
  return 0;
}

/** Execute one clone-safe protocol request inside the Worker realm. */
export function executeLayoutWorkerRequest(
  request: LayoutWorkerRequest,
  progress?: (event: LayoutTraceEvent) => void,
): LayoutWorkerResponse {
  const traceEvents: LayoutTraceEvent[] = [];
  let retainedTracePositions = 0;
  // Trace events exist only for a consumer: a retained diagnostic trace or a
  // requested live progress stream. Everything else plans hook-free, so jobs
  // nobody is watching build and post no per-event payloads at all.
  const stream = request.streamProgress ? progress : undefined;
  const trace = request.collectTrace || stream
    ? (event: LayoutTraceEvent) => {
      const positionCount = tracePositionCount(event);
      if (request.collectTrace && traceEvents.length < MAX_RETAINED_TRACE_EVENTS &&
        retainedTracePositions + positionCount <= MAX_RETAINED_TRACE_POSITION_ENTRIES) {
        traceEvents.push(event);
        retainedTracePositions += positionCount;
      }
      stream?.(event);
    }
    : undefined;

  try {
    if (request.operation === "integral") {
      const integralRequest = { ...request.request, trace };
      const result = planIntegralLayout(integralRequest);
      return {
        protocol: LAYOUT_WORKER_PROTOCOL_VERSION,
        id: request.id,
        operation: request.operation,
        ok: true,
        result: encodeIntegralLayoutPlan(result),
        traceEvents,
      };
    }

    if (request.operation === "constraint-repair") {
      const result = repairIntegralLayoutConstraints(
        { ...request.request, trace },
        decodeIntegralLayoutPlan(request.standard),
        request.options,
        trace,
      );
      return {
        protocol: LAYOUT_WORKER_PROTOCOL_VERSION,
        id: request.id,
        operation: request.operation,
        ok: true,
        result: encodeIntegralLayoutPlan(result),
        traceEvents,
      };
    }

    const result = planLayoutModel(request.model, request.change, {
      ...request.options,
      trace,
    });
    return {
      protocol: LAYOUT_WORKER_PROTOCOL_VERSION,
      id: request.id,
      operation: request.operation,
      ok: true,
      result: encodePlannedLayout(result),
      traceEvents,
    };
  } catch (error) {
    return {
      protocol: LAYOUT_WORKER_PROTOCOL_VERSION,
      id: request.id,
      operation: request.operation,
      ok: false,
      error: serializeLayoutWorkerError(error),
      traceEvents,
    };
  }
}
