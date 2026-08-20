// Build Smudgy with `--features web-audio-cpal`, copy this file into a
// server's `modules/` directory, connect, and enter `/earcon`.
//
// The same module can be the entry point of a sandboxed accessibility package.
// Such a package needs Smudgy `aliases` and `echo` capabilities, but no network,
// filesystem, subprocess, or special audio permission.

import { createAlias, echo } from "smudgy:core";

let activeContext: AudioContext | undefined;

createAlias("^/earcon$", async () => {
  if (activeContext && activeContext.state !== "closed") {
    await activeContext.close();
  }

  const context = new AudioContext({ latencyHint: "interactive" });
  activeContext = context;

  const oscillator = context.createOscillator();
  const gain = context.createGain();
  oscillator.frequency.value = 880;
  gain.gain.value = 0.06;
  oscillator.connect(gain);
  gain.connect(context.destination);

  oscillator.onended = async () => {
    await context.close();
    if (activeContext === context) activeContext = undefined;
    echo("Web Audio earcon finished.");
  };

  oscillator.start();
  oscillator.stop(context.currentTime + 0.12);
});

echo("Web Audio earcon loaded; enter /earcon to play it.");
