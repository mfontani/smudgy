import assert from "node:assert/strict";
import test from "node:test";
import { parseAnsiFragments } from "./ansi.ts";
import { filteredGpsDestinations } from "./atlas-model.ts";
import {
  auctionPresentation,
  isAuctioneer,
} from "./comms-format.ts";
import { visibleContexts } from "./context-model.ts";
import { fragmentsFromStyleSpans } from "./line-style.ts";

function destination(name: string, zone: number, aliases = "") {
  return {
    name,
    zone,
    aliases,
    category: "zones",
    tags: "",
    difficulty: "normal",
  };
}

test("an exact GPS zone filter outranks incidental text matches", () => {
  const result = filteredGpsDestinations([
    destination("The 42nd Street Market", 7),
    destination("The Perfect Zone", 42),
    destination("Another Place", 8, "route 42"),
  ], "42");

  assert.deepEqual(result.map((entry) => entry.zone), [42, 7, 8]);
});

test("GPS filtering otherwise preserves catalog order", () => {
  const catalog = [
    destination("Alpha Ruins", 1),
    destination("Beta Ruins", 2),
    destination("Other", 3),
  ];
  assert.deepEqual(
    filteredGpsDestinations(catalog, "ruins").map((entry) => entry.name),
    ["Alpha Ruins", "Beta Ruins"],
  );
});

test("zone intelligence omits its BIGMAP action", () => {
  const contexts = [
    { id: "vault", actions: [{ id: "bigmap" }] },
    {
      id: "zone-intelligence",
      actions: [{ id: "zinfo" }, { id: "bigmap" }, { id: "huntme" }],
    },
  ];
  assert.deepEqual(visibleContexts(contexts), [
    { id: "vault", actions: [{ id: "bigmap" }] },
    {
      id: "zone-intelligence",
      actions: [{ id: "zinfo" }, { id: "huntme" }],
    },
  ]);
});

test("only the Auctioneer is classified as an auction update", () => {
  assert.equal(isAuctioneer("the Auctioneer"), true);
  assert.equal(isAuctioneer("Auctioneer"), true);
  assert.equal(isAuctioneer("Alice"), false);
  assert.equal(auctionPresentation("Alice auctions, 'hello'").event, "UPDATE");
});

test("full ANSI parsing retains truecolor, palette color, and plain text", () => {
  const fragments = parseAnsiFragments(
    "\x1b[0m\x1b[38;2;255;0;102mAlice\x1b[0m \x1b[32mhello",
  );

  assert.equal(fragments.map((fragment) => fragment.text).join(""), "Alice hello");
  assert.deepEqual(fragments[0].style?.fg, { r: 255, g: 0, b: 102 });
  assert.equal(fragments[1].style?.fg, "default");
  assert.deepEqual(fragments[2].style?.fg, {
    color: "green",
    bold: false,
    paletteBright: false,
  });
});

test("non-SGR CSI controls are consumed instead of leaking into chat", () => {
  const fragments = parseAnsiFragments("before\x1b[2Kafter");
  assert.equal(fragments.map((fragment) => fragment.text).join(""), "beforeafter");
});

test("captured terminal styles retain byte-accurate Skynet spans", () => {
  const fragments = fragmentsFromStyleSpans("(Skynet) ★ ready", [
    { begin: 0, end: 8, fg: "cyan" },
    { begin: 9, end: 12, fg: "yellow" },
    { begin: 13, end: 18, fg: "green" },
  ]);

  assert.equal(fragments.map((fragment) => fragment.text).join(""), "(Skynet) ★ ready");
  assert.equal(fragments.find((fragment) => fragment.text === "★")?.style?.fg, "yellow");
  assert.equal(fragments.find((fragment) => fragment.text === "ready")?.style?.fg, "green");
});
