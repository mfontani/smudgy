// =============================================================================
//  Map pane — the smudgy MapView with a room header and a live GPS strip
// =============================================================================
//  Everything in this pane is fed by bindings (room name/area from Room.Info,
//  the GPS line from a small derived state), so the widget mounts once and
//  never rebuilds — the MapView keeps its zoom and pan across room changes.

import { createState, mapper, send, session, getSettings } from "smudgy:core";
import { room as mapRoomChanged } from "smudgy:events/map";
import {
  Button,
  Column,
  Container,
  MapView,
  Row,
  Space,
  Text,
  createWidget,
  type MapStyleApplication,
} from "smudgy:widgets";
import { layoutState } from "smudgy:state/kapusniak/nukefire-mapper";
import {
  nukefire,
  watchMessage,
  type CharGps,
} from "smudgy://kapusniak/nukefire-gmcp";
import { showLayoutState, widgetTextSize } from "./config.ts";
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
          crossAreaLabelBackground: getSettings().palette?.background,
          
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
      {showLayoutState &&
        <Container width="fill" background={UI.card}>
          <Column width="fill" padding={6} spacing={3}>
            <Row spacing={8}>
              <Text size={widgetTextSize(10)} color={UI.gold}>LAYOUT</Text>
              <Text size={widgetTextSize(10)} color={UI.bright}>
                {layoutState.bind("status", { fallback: "idle" })}
              </Text>
              <Text size={widgetTextSize(10)} color={UI.dim}>
                {layoutState.bind("phase", { fallback: "idle" })}
              </Text>
              <Space width="fill" />
              <Text size={widgetTextSize(10)} color={UI.dim}>
                {layoutState.bind("elapsedMs", { fallback: 0 })} ms
              </Text>
            </Row>
            <Row spacing={10}>
              <Text size={widgetTextSize(9)} color={UI.text}>
                layouts {layoutState.bind("work.layoutsConsidered", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                compactions {layoutState.bind("work.compactionAttempts", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                restarts {layoutState.bind("work.restarts", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                checks {layoutState.bind("work.feasibilityChecks", { fallback: 0 })}
              </Text>
            </Row>
            <Row spacing={10}>
              <Text size={widgetTextSize(9)} color={UI.text}>
                crossing-pairs {layoutState.bind("work.crossingsConsidered", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                macros {layoutState.bind("work.macrosConsidered", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                pushes {layoutState.bind("work.pushClosures", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                depth {layoutState.bind("work.maxDepth", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                states {layoutState.bind("work.visitedStates", { fallback: 0 })}
              </Text>
            </Row>
            <Row spacing={10}>
              <Text size={widgetTextSize(9)} color={UI.dim}>CURRENT</Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                rays {layoutState.bind("currentQuality.cardinalRayViolations", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                reciprocal {layoutState.bind("currentQuality.reciprocalRayViolations", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                routes {layoutState.bind("currentQuality.routingViolations", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                rooms {layoutState.bind("currentQuality.roomObstructions", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                ports {layoutState.bind("currentQuality.exitPortViolations", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                2way-ports {layoutState.bind("currentQuality.reciprocalExitPortViolations", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.text}>
                crossings {layoutState.bind("currentQuality.linkCrossings", { fallback: 0 })}
              </Text>
            </Row>
            <Row spacing={10}>
              <Text size={widgetTextSize(9)} color={UI.gold}>BEST</Text>
              <Text size={widgetTextSize(9)} color={UI.bright}>
                rays {layoutState.bind("bestQuality.cardinalRayViolations", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.bright}>
                reciprocal {layoutState.bind("bestQuality.reciprocalRayViolations", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.bright}>
                routes {layoutState.bind("bestQuality.routingViolations", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.bright}>
                rooms {layoutState.bind("bestQuality.roomObstructions", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.bright}>
                ports {layoutState.bind("bestQuality.exitPortViolations", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.bright}>
                2way-ports {layoutState.bind("bestQuality.reciprocalExitPortViolations", { fallback: 0 })}
              </Text>
              <Text size={widgetTextSize(9)} color={UI.bright}>
                crossings {layoutState.bind("bestQuality.linkCrossings", { fallback: 0 })}
              </Text>
            </Row>
          </Column>
        </Container>}
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
