export const NUKEFIRE_ATLAS_NAME = "Nukefire";

/** Wait for the startup area refresh before the atlas catalogue can change. */
export async function afterAreaRefresh<T>(
  refresh: Promise<void>,
  operation: () => Promise<T>,
): Promise<T> {
  try {
    await refresh;
  } catch {
    // Atlas initialization remains useful when the independent area refresh fails.
  }
  return await operation();
}

/** Build a creation log record without reading the new Atlas handle's live storage getter. */
export function createdAtlasDecisionSummary(
  atlas: Pick<Atlas, "id" | "name">,
  storage: MapStorage,
): { atlasId: AtlasId; name: string; storage: MapStorage } {
  return { atlasId: atlas.id, name: atlas.name, storage };
}

/** Find the NukeFire atlas in local storage, creating it when absent. */
export async function upsertLocalNukeFireAtlas(
  atlasMapper: Pick<Mapper, "listAtlases" | "createAtlas">,
): Promise<Atlas> {
  const existing = (await atlasMapper.listAtlases()).find((atlas) =>
    atlas.storage === "local" && atlas.name === NUKEFIRE_ATLAS_NAME
  );
  return existing ?? await atlasMapper.createAtlas(NUKEFIRE_ATLAS_NAME, {
    storage: "local",
  });
}
