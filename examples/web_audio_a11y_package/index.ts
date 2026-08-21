import { createAlias, echo } from "smudgy:core";

let activeContext: AudioContext | undefined;

async function playEarcon() {
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
    echo("Sandboxed Web Audio earcon finished.");
  };

  oscillator.start();
  oscillator.stop(context.currentTime + 0.12);
}

createAlias("^/a11y-earcon$", () => {
  void playEarcon();
});

echo("Sandboxed Web Audio a11y package loaded; enter /a11y-earcon.");
