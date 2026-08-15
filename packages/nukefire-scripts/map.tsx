// =============================================================================
//  Map pane — the smudgy MapView with a room header and a live GPS strip
// =============================================================================
//  Everything in this pane is fed by bindings (room name/area from Room.Info,
//  the GPS line from a small derived state), so the widget mounts once and
//  never rebuilds — the MapView keeps its zoom and pan across room changes.

import { createState, mapper, send, session } from "smudgy:core";
import { room as mapRoomChanged } from "smudgy:events/map";
import {
  Button,
  Column,
  MapView,
  Row,
  Space,
  Text,
  createWidget,
  type MapStyleApplication,
} from "smudgy:widgets";
import {
  nukefire,
  watchMessage,
  type CharGps,
} from "smudgy://kapusniak/nukefire-gmcp";
import { widgetTextSize } from "./config.ts";
import {
  CURRENT_ROOM_STYLE,
  ROUTE_STYLE,
  currentRoomMapViewApply,
  gpsRouteRaw,
  mapViewRoute,
  type RouteRoom,
} from "./map-route.ts";
import { GPS_CLEAR, UI } from "./theme.ts";

const PANE = "Map";

interface GpsView {
  line: string;
  color: string;
}

const gpsView = createState<GpsView>("gpsView");
gpsView.set({ line: "no route set", color: UI.faint });
const gpsMapApply = createState<MapStyleApplication[]>("gpsMapApply");
gpsMapApply.set([]);

let latestGps: Readonly<CharGps> | undefined;

function currentMappedRoom(): Room | undefined {
  const location = mapper.getCurrentLocation();
  if (location?.room !== undefined) {
    return mapper.getAreaById(location.area).room(location.room);
  }
  const vnum = nukefire.value?.Room?.Info?.num;
  return Number.isSafeInteger(vnum)
    ? mapper.findRoomByExternalId(String(vnum))
    : undefined;
}

function refreshGpsRoute(): void {
  const routeRaw = gpsRouteRaw(latestGps);
  const current = currentMappedRoom();
  const currentRoomApply = currentRoomMapViewApply(
    current as RouteRoom | undefined,
  );
  if (!latestGps?.active || !routeRaw) {
    gpsMapApply.set(currentRoomApply);
    return;
  }
  try {
    gpsMapApply.set([
      ...currentRoomApply,
      ...mapViewRoute(
        current as RouteRoom | undefined,
        routeRaw,
        (areaId, roomNumber) =>
          mapper.getAreaById(areaId).room(roomNumber) as RouteRoom | undefined,
      ),
    ]);
  } catch {
    // The mapper can be between area snapshots while movement GMCP arrives.
    // Its subsequent map:room event retries against the settled topology.
    gpsMapApply.set(currentRoomApply);
  }
}

function updateGps(gps: Readonly<CharGps> | undefined): void {
  latestGps = gps;
  if (gps?.active) {
    gpsView.set({
      line: `→ ${gps.destination} · ${gps.steps} steps · next: ${gps.next || "?"}`,
      color: UI.gold,
    });
  } else {
    gpsView.set({ line: "no route set", color: UI.faint });
  }
  refreshGpsRoute();
}

watchMessage("Char.GPS", updateGps);
// State watches are write-triggered rather than replaying retained state.
// Seed the GPS strip and route accent immediately when scripts reload after
// Char.GPS arrived, so a stationary player still sees the active route.
updateGps(nukefire.value?.Char?.GPS);

// NukeFire's mapper may finish applying a newly discovered room after the
// Char.GPS message. Refresh again when Smudgy commits the new current room.
mapRoomChanged.on(refreshGpsRoute);

function mount(): void {
  createWidget(
    "nf-map",
    <Column width="fill" height="fill" padding={6} spacing={6}>
      <Row spacing={8}>
        <Text size={widgetTextSize(14)} color={UI.bright}>
          {nukefire.bind("Room.Info.name", { fallback: "NukeFire" })}
        </Text>
        <Space width="fill" />
        <Text size={widgetTextSize(11)} color={UI.dim}>
          {nukefire.bind("Room.Info.area", { fallback: "" })}
        </Text>
      </Row>
      <MapView
        defaultStyle={{
          crossAreaLabelVisibility: "hover",
          crossAreaLabelBackground: UI.navyDeep,
        }}
        styles={{
          [CURRENT_ROOM_STYLE]: {
            crossAreaLabelVisibility: "always",
          },
          [ROUTE_STYLE]: {
            connectionColor: UI.gold,
            connectionWidth: 2,
            roomStroke: UI.gold,
            roomStrokeWidth: 2,
            crossAreaLabelVisibility: "always",
          },
        }}
        apply={gpsMapApply.bind()}
      />
      <Row spacing={8}>
        <Text size={widgetTextSize(11)} color={UI.gold}>GPS</Text>
        <Text size={widgetTextSize(11)} color={gpsView.bind("color")}>{gpsView.bind("line")}</Text>
        <Space width="fill" />
        <Button variant="subtle" onPress={() => send(GPS_CLEAR)}>
          <Text size={widgetTextSize(10)} color={UI.dim}>clear</Text>
        </Button>
      </Row>
    </Column>,
    { pane: PANE },
  );
}

export function open(): void {
  session.mainPane.split("right", { name: PANE, width: 400, terminal: false });
  mount();
}

export function close(): void {
  session.panes.get(PANE)?.close();
}
