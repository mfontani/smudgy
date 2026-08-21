# Web Audio preview

Smudgy has a bounded, standards-shaped Web Audio integration for short
scripted synthesis such as accessibility earcons. It is available to trusted
modules and sandboxed packages through the same per-session output boundary.

This is currently a source-build preview. Official release builds remain
hardware-free while accessible master, session, and package volume and mute
controls are completed. Packages must not yet require Web Audio in a published
release of Smudgy.

## Build features

| Configuration | Web Audio availability | Output |
| --- | --- | --- |
| No feature (the release default) | Not installed in ordinary sessions | No audio device dependency |
| `web-audio` | Available only to callers that construct an explicit audio-scoped session | An injected logical output, including `sinkId: "none"`; no CPAL device backend |
| `web-audio-cpal` | Enables the desktop audio coordinator and includes `web-audio` | One shared default-device stream plus `sinkId: "none"` |

For the physical preview:

```sh
cargo run -p smudgy_ui --features web-audio-cpal
```

`web-audio-cpal` reaches CPAL only through
`smudgy_audio/physical-output`. It uses WASAPI on Windows, ALSA on Linux, and
CoreAudio on macOS. Linux source builds need the ALSA development package
(`libasound2-dev` on Ubuntu/Debian). The current process mixer runs at 48,000 Hz
stereo and rejects a default device that cannot provide that exact format.

The hardware-free feature is primarily for deterministic tests and embedders.
Selecting it on `smudgy_ui` alone does not silently create audio authority for
ordinary desktop sessions.

## Supported hosted surface

The generated script declarations intentionally describe only this supported
online surface:

| Surface | Supported behavior |
| --- | --- |
| `AudioContext` | Default session output or `sinkId: "none"`; `suspend()`, `resume()`, and absorbing `close()` |
| `destination` | The context's private destination on its session Script bus |
| `GainNode` | Construction, connection, and scalar `gain.value` mutation |
| `OscillatorNode` | Sine, square, sawtooth, and triangle waves; scalar frequency/detune; `start()`, `stop()`, and `ended` |
| Connections | Exact node/parameter `connect()` and `disconnect()` forms for the hosted graph |

Other node families, custom/periodic waves, `AudioParam` automation methods,
offline rendering, decoding, worklets, media capture, and device selection are
outside this hosted compatibility promise. The pinned alpha extension may
expose additional working symbols, but packages cannot rely on those as Smudgy
product surface. Operations that the hosted layer rejects are validated before
they mutate its mixer.

See [the trusted module example](examples/web_audio_earcon.ts) and [the complete
sandboxed package example](examples/web_audio_a11y_package/). The package asks
only for alias and echo capabilities; it has no audio grant.

## Sandbox boundary

Web Audio is a bounded baseline API inside an audio-scoped session, including
inside sandboxed accessibility packages. It does not add filesystem, network,
module-loading, subprocess, FFI, system-information, or audio-capture authority.
Audio sources remain subject to the package's existing source-access rules.
Graphs and mixer inputs are private to their context, isolate, and session.

## Manual physical checks

### Current evidence

On 2026-08-19 the ignored Windows/WASAPI lifecycle smoke passed with:

```powershell
cargo test -p smudgy_audio --features physical-output --lib `
  system::tests::manual_default_device_silent_open_suspend_resume_close_and_repeat `
  --locked -- --ignored --exact --nocapture
```

The test opened the default device, exercised suspend/resume and logical input
shutdown, and reported `shutdown.clean == true` with `failure == None`. An
immediate second service open/shutdown also reported clean/none.

This branch configures platform compile and injected-output test jobs, but those
jobs are not runtime-device evidence until CI has run them. Windows live device
removal and default-device change remain pending. Physical runtime exercises on
Linux and macOS also remain pending.

CI is configured to compile the physical feature and run injected-output tests
on Windows, Linux, and macOS, but it never requires a default audio device.
Before enabling physical output in a release, record the following on available
hosts:

| Platform | Backend | Required exercise |
| --- | --- | --- |
| Windows | WASAPI | playback, suspend/resume, close/reopen, live removal, default-device change |
| Linux | ALSA | playback, suspend/resume, close/reopen, live removal, default-device change |
| macOS | CoreAudio | playback, suspend/resume, close/reopen, live removal, default-device change |

For removal, keep at least two default-output contexts and one `sinkId: "none"`
context alive. Confirm the first operational failure seals new physical
admission and notifies each physical endpoint once, while the no-device context
continues independently. Then close the sessions and confirm the application
reports logical endpoint, callback/source, mixer-session, and physical-driver
cleanup separately.

For a default-device change, record whether the operating system keeps the
existing stream alive or reports failure; Smudgy does not promise live migration
in this preview. A later service start should use the then-current default.

Smudgy's shutdown proof covers its own joined physical-driver loop and exact
logical callback/source retirement. CPAL and an operating-system backend may
use internal helper threads whose joins are not exposed. Dropping their RAII
objects is therefore not documented as proof that every opaque helper joined.
