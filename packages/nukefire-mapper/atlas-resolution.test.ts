import assert from "node:assert/strict";
import test from "node:test";
import {
  afterAreaRefresh,
  createdAtlasDecisionSummary,
  NUKEFIRE_ATLAS_NAME,
  upsertLocalNukeFireAtlas,
} from "./atlas-resolution.ts";

function atlas(id: number, name: string, storage: MapStorage): Atlas {
  return {
    id: [0, id],
    name,
    storage,
    toString: () => name,
  };
}

test("reuses the existing local Nukefire atlas", async () => {
  const cloud = atlas(1, NUKEFIRE_ATLAS_NAME, "cloud");
  const local = atlas(2, NUKEFIRE_ATLAS_NAME, "local");
  let createCalls = 0;

  const resolved = await upsertLocalNukeFireAtlas({
    listAtlases: async () => [cloud, local],
    createAtlas: async () => {
      createCalls += 1;
      return atlas(3, NUKEFIRE_ATLAS_NAME, "local");
    },
  });

  assert.equal(resolved, local);
  assert.equal(createCalls, 0);
});

test("creates the Nukefire atlas in local storage when absent", async () => {
  const created = atlas(2, NUKEFIRE_ATLAS_NAME, "local");
  const calls: Array<{ name: string; storage: string }> = [];

  const resolved = await upsertLocalNukeFireAtlas({
    listAtlases: async () => [atlas(1, NUKEFIRE_ATLAS_NAME, "cloud")],
    createAtlas: async (name, options) => {
      calls.push({ name, storage: options.storage });
      return created;
    },
  });

  assert.equal(resolved, created);
  assert.deepEqual(calls, [{ name: NUKEFIRE_ATLAS_NAME, storage: "local" }]);
});

test("startup atlas work waits for the area refresh to settle", async () => {
  let finishRefresh: (() => void) | undefined;
  const refresh = new Promise<void>((resolve) => {
    finishRefresh = resolve;
  });
  let atlasStarted = false;
  const initialized = afterAreaRefresh(refresh, async () => {
    atlasStarted = true;
    return "atlas";
  });

  await Promise.resolve();
  assert.equal(atlasStarted, false);
  finishRefresh?.();
  assert.equal(await initialized, "atlas");
  assert.equal(atlasStarted, true);
});

test("a failed area refresh does not prevent atlas initialization", async () => {
  const initialized = await afterAreaRefresh(
    Promise.reject(new Error("refresh failed")),
    async () => "atlas",
  );
  assert.equal(initialized, "atlas");
});

test("atlas creation logging does not read the live storage getter", () => {
  const created = atlas(3, NUKEFIRE_ATLAS_NAME, "local");
  Object.defineProperty(created, "storage", {
    get: () => {
      throw new Error("Atlas not found");
    },
  });

  assert.deepEqual(createdAtlasDecisionSummary(created, "local"), {
    atlasId: [0, 3],
    name: NUKEFIRE_ATLAS_NAME,
    storage: "local",
  });
});
