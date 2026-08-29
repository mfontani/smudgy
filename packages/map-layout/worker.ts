import { executeLayoutWorkerRequest } from "./worker-executor.ts";
import type { LayoutWorkerRequest } from "./worker-protocol.ts";

interface LayoutWorkerScope {
  onmessage: ((event: { data: LayoutWorkerRequest }) => void) | null;
  postMessage(message: unknown): void;
}

const scope = globalThis as unknown as LayoutWorkerScope;

scope.onmessage = (event): void => {
  const request = event.data;
  scope.postMessage(executeLayoutWorkerRequest(request, (progressEvent) => {
    scope.postMessage({
      protocol: request.protocol,
      id: request.id,
      operation: request.operation,
      progress: true,
      event: progressEvent,
    });
  }));
};
