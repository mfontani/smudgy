import assert from "node:assert/strict";
import test from "node:test";
import {
  buildNavigationRoute,
  formatRoute,
  splitArrivalCommands,
  takeCommandArgument,
  type NavigationAreaId,
  type NavigationRoom,
} from "./navigation.ts";

const AREA = [1, 2] as const;
const OTHER = [3, 4] as const;

function room(
  room_number: number,
  exits: NavigationRoom["exits"] = [],
  area_id: NavigationAreaId = AREA,
): NavigationRoom {
  return { area_id, room_number, exits };
}

test("turns weighted mapper paths into explicit and directional commands", () => {
  const rooms = new Map<string, NavigationRoom>([
    ["1:2:10", room(10, [{
      from_direction: "North",
      to_area_id: AREA,
      to_room_number: 11,
      command: "north",
    }])],
    ["1:2:11", room(11, [{
      from_direction: "Special",
      to_area_id: OTHER,
      to_room_number: 20,
      command: "enter portal",
      is_closed: true,
    }])],
    ["3:4:20", room(20, [], OTHER)],
  ]);
  const route = buildNavigationRoute(
    [[AREA, 10], [AREA, 11], [OTHER, 20]],
    (area, number) => rooms.get(`${area[0]}:${area[1]}:${number}`),
  );

  assert.equal(route.error, undefined);
  assert.deepEqual(route.steps.map(({ command, closed }) => ({ command, closed })), [
    { command: "north", closed: false },
    { command: "enter portal", closed: true },
  ]);
  assert.equal(formatRoute(route.steps), "north ; enter portal");
});

test("falls back to the canonical short command when an exit has none", () => {
  const route = buildNavigationRoute(
    [[AREA, 1], [AREA, 2]],
    (_area, number) => number === 1
      ? room(1, [{
        from_direction: "Northeast",
        to_area_id: AREA,
        to_room_number: 2,
        command: null,
      }])
      : room(2),
  );
  assert.equal(route.error, undefined);
  assert.equal(route.steps[0]?.command, "ne");
});

test("reports a path whose durable exit no longer exists", () => {
  const route = buildNavigationRoute(
    [[AREA, 1], [AREA, 2]],
    (_area, number) => room(number),
  );
  assert.match(route.error ?? "", /no exit/);
  assert.deepEqual(route.steps, []);
});

test("parses quoted room names without consuming arrival commands", () => {
  assert.deepEqual(takeCommandArgument('"The Temple of Technology" look north;look south'), {
    value: "The Temple of Technology",
    rest: "look north;look south",
  });
  assert.deepEqual(takeCommandArgument("3001 look"), { value: "3001", rest: "look" });
  assert.deepEqual(splitArrivalCommands(" look north; ;look south "), ["look north", "look south"]);
});
