import assert from "node:assert/strict";
import test from "node:test";

import {
  disambiguateOneWayArrivalPorts,
  routedEndpointSide,
  routeIsManuallyAuthored,
  type OneWayPortConnection,
} from "./connection-ports.ts";

function endpoint(
  room: RoomNumber,
  side: RoomSide,
  portOffset = 0.5,
  portMode: PortMode = "AutoPinned",
): ConnectionEndpoint {
  return {
    room_number: room,
    side,
    port_offset: portOffset,
    port_mode: portMode,
  };
}

test("routing preserves Manual walls while AutoPinned ports follow the route", () => {
  assert.equal(routedEndpointSide(endpoint(1, "North", 0.3, "Manual"), "West"), "North");
  assert.equal(routedEndpointSide(endpoint(1, "North"), "West"), "West");
});

test("only Manual routing marks an author-drawn, user-owned centerline", () => {
  assert.equal(routeIsManuallyAuthored("Manual"), true);
  // The mapper persists its own routes — detour points included — as
  // solver-produced `Automatic`; the display-only modes are not routes.
  assert.equal(routeIsManuallyAuthored("Automatic"), false);
  assert.equal(routeIsManuallyAuthored("Simple"), false);
  assert.equal(routeIsManuallyAuthored("Stub"), false);
});

function positioned(
  connection: Omit<OneWayPortConnection, "positionA" | "positionB">,
  positionA: readonly [number, number],
  positionB: readonly [number, number],
): OneWayPortConnection {
  return {
    ...connection,
    positionA: { x: positionA[0], y: positionA[1] },
    positionB: { x: positionB[0], y: positionB[1] },
  };
}

function plain(
  value: ReadonlyMap<string, { endpointA: ConnectionEndpoint; endpointB: ConnectionEndpoint }>,
): Record<string, { endpointA: ConnectionEndpoint; endpointB: ConnectionEndpoint }> {
  return Object.fromEntries(value);
}

test("fans one-way arrivals around a reciprocal midpoint in source order", () => {
  const input: OneWayPortConnection[] = [
    positioned({
      key: "reciprocal",
      endpointA: endpoint(10, "North"),
      endpointB: endpoint(11, "South"),
    }, [0, 0], [0, -2]),
    positioned({
      key: "west-arrival",
      endpointA: endpoint(20, "South"),
      endpointB: endpoint(10, "North"),
      oneWayOriginRoom: 20,
    }, [-2, -2], [0, 0]),
    positioned({
      key: "east-arrival",
      endpointA: endpoint(10, "North"),
      endpointB: endpoint(30, "South"),
      oneWayOriginRoom: 30,
    }, [0, 0], [2, -2]),
  ];
  const result = disambiguateOneWayArrivalPorts(input);

  assert.equal(result.get("reciprocal")?.endpointA.port_offset, 0.5);
  assert.equal(result.get("west-arrival")?.endpointB.port_offset, 0.2);
  // The target is canonical endpoint A here, proving A/B order is irrelevant.
  assert.equal(result.get("east-arrival")?.endpointA.port_offset, 0.8);
  assert.equal(result.get("west-arrival")?.endpointA.port_offset, 0.5);
  assert.equal(result.get("east-arrival")?.endpointB.port_offset, 0.5);
});

test("is deterministic under input permutation and repeated application", () => {
  const input: OneWayPortConnection[] = [
    positioned({
      key: "zeta",
      endpointA: endpoint(1, "East"),
      endpointB: endpoint(9, "West"),
      oneWayOriginRoom: 1,
    }, [-2, -1], [0, 0]),
    positioned({
      key: "alpha",
      endpointA: endpoint(2, "East"),
      endpointB: endpoint(9, "West"),
      oneWayOriginRoom: 2,
    }, [-2, -1], [0, 0]),
  ];
  const forward = disambiguateOneWayArrivalPorts(input);
  const reversed = disambiguateOneWayArrivalPorts([...input].reverse());
  assert.deepEqual(plain(forward), plain(reversed));
  // Equal source projections fall back to the stable key.
  assert.equal(forward.get("alpha")?.endpointB.port_offset, 0.2);
  assert.equal(forward.get("zeta")?.endpointB.port_offset, 0.8);

  const reapplied = disambiguateOneWayArrivalPorts(
    input.map((connection) => ({
      ...connection,
      endpointA: forward.get(connection.key)!.endpointA,
      endpointB: forward.get(connection.key)!.endpointB,
    })),
  );
  assert.deepEqual(plain(reapplied), plain(forward));
});

test("orders three one-way arrivals across all canonical lanes", () => {
  const result = disambiguateOneWayArrivalPorts([
    positioned({
      key: "left",
      endpointA: endpoint(1, "South"),
      endpointB: endpoint(9, "North"),
      oneWayOriginRoom: 1,
    }, [-2, -2], [0, 0]),
    positioned({
      key: "middle",
      endpointA: endpoint(2, "South"),
      endpointB: endpoint(9, "North"),
      oneWayOriginRoom: 2,
    }, [0, -2], [0, 0]),
    positioned({
      key: "right",
      endpointA: endpoint(3, "South"),
      endpointB: endpoint(9, "North"),
      oneWayOriginRoom: 3,
    }, [2, -2], [0, 0]),
  ]);

  assert.equal(result.get("left")?.endpointB.port_offset, 0.2);
  assert.equal(result.get("middle")?.endpointB.port_offset, 0.5);
  assert.equal(result.get("right")?.endpointB.port_offset, 0.8);
});

test("orders two arrivals after a Manual early-lane endpoint", () => {
  const result = disambiguateOneWayArrivalPorts([
    positioned({
      key: "manual-left",
      endpointA: endpoint(9, "North", 0.2, "Manual"),
      endpointB: endpoint(10, "South"),
    }, [0, 0], [0, -3]),
    positioned({
      key: "left-arrival",
      endpointA: endpoint(1, "South"),
      endpointB: endpoint(9, "North"),
      oneWayOriginRoom: 1,
    }, [-2, -2], [0, 0]),
    positioned({
      key: "right-arrival",
      endpointA: endpoint(2, "South"),
      endpointB: endpoint(9, "North"),
      oneWayOriginRoom: 2,
    }, [2, -2], [0, 0]),
  ]);

  assert.equal(result.get("manual-left")?.endpointA.port_offset, 0.2);
  assert.equal(result.get("left-arrival")?.endpointB.port_offset, 0.5);
  assert.equal(result.get("right-arrival")?.endpointB.port_offset, 0.8);
});

test("different target walls do not form a crowded cohort", () => {
  const result = disambiguateOneWayArrivalPorts([
    positioned({
      key: "north",
      endpointA: endpoint(1, "South"),
      endpointB: endpoint(9, "North"),
      oneWayOriginRoom: 1,
    }, [0, -2], [0, 0]),
    positioned({
      key: "west",
      endpointA: endpoint(2, "East"),
      endpointB: endpoint(9, "West"),
      oneWayOriginRoom: 2,
    }, [-2, 0], [0, 0]),
  ]);

  assert.equal(result.get("north")?.endpointB.port_offset, 0.5);
  assert.equal(result.get("west")?.endpointB.port_offset, 0.5);
});

test("an already distinct off-center wall-mate does not displace an arrival", () => {
  const result = disambiguateOneWayArrivalPorts([
    positioned({
      key: "manual-lane",
      endpointA: endpoint(9, "North", 0.8, "Manual"),
      endpointB: endpoint(10, "South"),
    }, [0, 0], [0, -2]),
    positioned({
      key: "arrival",
      endpointA: endpoint(1, "South"),
      endpointB: endpoint(9, "North"),
      oneWayOriginRoom: 1,
    }, [0, -2], [0, 0]),
  ]);

  assert.equal(result.get("manual-lane")?.endpointA.port_offset, 0.8);
  assert.equal(result.get("arrival")?.endpointB.port_offset, 0.5);
});

test("Manual and protected arrivals remain fixed while reserving their slots", () => {
  const result = disambiguateOneWayArrivalPorts([
    positioned({
      key: "manual",
      endpointA: endpoint(1, "South"),
      endpointB: endpoint(9, "North", 0.2, "Manual"),
      oneWayOriginRoom: 1,
    }, [-3, -2], [0, 0]),
    positioned({
      key: "protected",
      endpointA: endpoint(2, "South"),
      endpointB: endpoint(9, "North", 0.5),
      oneWayOriginRoom: 2,
      protected: true,
    }, [0, -2], [0, 0]),
    positioned({
      key: "movable",
      endpointA: endpoint(3, "South"),
      endpointB: endpoint(9, "North"),
      oneWayOriginRoom: 3,
    }, [3, -2], [0, 0]),
  ]);

  assert.equal(result.get("manual")?.endpointB.port_offset, 0.2);
  assert.equal(result.get("protected")?.endpointB.port_offset, 0.5);
  assert.equal(result.get("movable")?.endpointB.port_offset, 0.8);
});

test("a no-longer-crowded AutoPinned arrival heals to its midpoint home", () => {
  const result = disambiguateOneWayArrivalPorts([
    positioned({
      key: "arrival",
      endpointA: endpoint(1, "East"),
      endpointB: endpoint(9, "West", 0.2),
      oneWayOriginRoom: 1,
    }, [-2, 0], [0, 0]),
  ]);

  assert.equal(result.get("arrival")?.endpointB.port_offset, 0.5);
  assert.equal(result.get("arrival")?.endpointA.port_offset, 0.5);
});

test("bidirectional and ambiguous endpoint pairs are never moved", () => {
  const result = disambiguateOneWayArrivalPorts([
    positioned({
      key: "reciprocal",
      endpointA: endpoint(9, "West", 0.4),
      endpointB: endpoint(1, "East", 0.6),
    }, [0, 0], [-2, 0]),
    positioned({
      key: "self-loop",
      endpointA: endpoint(9, "North", 0.35),
      endpointB: endpoint(9, "North", 0.65),
      oneWayOriginRoom: 9,
    }, [0, 0], [0, 0]),
  ]);

  assert.deepEqual(plain(result), {
    reciprocal: {
      endpointA: endpoint(9, "West", 0.4),
      endpointB: endpoint(1, "East", 0.6),
    },
    "self-loop": {
      endpointA: endpoint(9, "North", 0.35),
      endpointB: endpoint(9, "North", 0.65),
    },
  });
});
