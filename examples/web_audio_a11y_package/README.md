# Web Audio a11y package example

This is a local Smudgy package, so it runs in its own sandboxed isolate. Its
manifest asks only for the Smudgy alias and echo capabilities. Web Audio is a
baseline bounded API and needs no package audio permission; this package has no
filesystem, network, subprocess, FFI, or system-information access.

This is a source-build preview. Official release builds do not enable physical
Web Audio yet; see the repository's [Web Audio support and manual-test
matrix](../../WEB_AUDIO.md).

1. Build or run Smudgy with `--features web-audio-cpal`.
2. Copy this directory to
   `<smudgy-home>/<server>/packages/web-audio-a11y/` and enable the local package
   in Smudgy's Automations window.
3. Connect to that server and enter `/a11y-earcon`.

The package creates its own `AudioContext`, plays a quiet 880 Hz oscillator for
120 ms, receives `ended`, and closes the context. The declarations and CI
compile this exact package entry point against Smudgy's narrow hosted surface.
