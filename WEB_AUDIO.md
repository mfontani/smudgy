# Web Audio support

Smudgy has a bounded, standards-shaped Web Audio integration for short
scripted synthesis such as accessibility earcons. It is available to trusted
modules and sandboxed packages through the same per-session output boundary.

Official Windows, macOS, and Flatpak build paths select the physical Web Audio
feature. The API remains available through hosted silent emulation when a
compatible physical output is absent, full, or unavailable at startup. Platform
runtime evidence is listed below; configured packaging is not itself a runtime
or device-support claim.

Audio is optional: an unavailable physical output or lifecycle worker never
prevents the terminal client or Web Audio globals from starting. A no-argument
default `AudioContext` and explicit `sinkId: "none"` both remain normal,
joinable graphs backed by hosted silent output. The visible controls can save a
next-physical-start preference in that emulated mode and label it "not
applied." A stale or closing live session, inactive package scope, or output
that dies after physical operation began is a visible control failure and is
not persisted as a successful change.

## Volume and mute controls

The audio panel exposes one master row, one row for every active session,
and one row for each enabled sandbox package root. Trusted packages run on
Main and are explicitly labeled as session-controlled; Smudgy does not claim
truthful per-package control inside that shared isolate.

Volume is stored as a whole percentage from 0 through 100 and mapped linearly
to mixer gain 0.0 through 1.0. Mute is independent, so unmuting restores the
remembered volume. Master policy is global. Session policy uses the durable
server/profile identity, and sandbox policy adds its folded, versionless
owner/name root. Non-default package policies remain available across
uninstall/reinstall and version changes; default rows are compacted away.

Open and focus the panel with `Ctrl+Shift+A` (`Command+Shift+A` on macOS).
`Tab` and `Shift+Tab` cycle its focusable rows without a pointer. On a focused
row, the arrow keys adjust volume by five points, `Home` and `End` select 0 and
100, and `Space` toggles mute. Focus is visibly outlined and off-screen rows
are scrolled into view.

These are visible, keyboard-focusable controls, not a full screen-reader
semantics claim. The pinned official iced 0.14 widget stack does not yet expose
the required accessibility semantics for this custom surface. Smudgy does not
ship an iced/AccessKit fork in this slice, and full screen-reader navigation and
announcements remain out of scope.

## Build features

| Configuration | Web Audio availability | Output |
| --- | --- | --- |
| No feature (crate default) | Not installed in ordinary sessions | No audio device dependency |
| `web-audio` | Available only to callers that construct an explicit audio-scoped session | An injected logical output, including `sinkId: "none"`; no CPAL device backend |
| `web-audio-cpal` (official desktop packages) | Enables the desktop audio coordinator and includes `web-audio` | One shared default-device stream when available; otherwise default output and `sinkId: "none"` use hosted silent emulation |

For a physical source build:

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

Audible default contexts use the shared fixed 48,000 Hz mixer. An explicitly
requested non-48 kHz audible default context is currently rejected as
unsupported; assets at other rates are resampled by Web Audio within a 48 kHz
context. Hosted silent/emulated contexts have no hardware-rate contract and may
retain another requested logical context rate. This leaves a clear future seam
for context-to-output resampling without reconfiguring the process device.

## Supported hosted surface

The generated script declarations intentionally describe only this supported
online surface:

| Surface | Supported behavior |
| --- | --- |
| `AudioContext` | Default session output or `sinkId: "none"`; if physical output is unavailable, either form remains a silent, time-advancing context; `suspend()`, `resume()`, and absorbing `close()` |
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

The Flatpak package statically grants `--socket=pulseaudio`. That is a broad
application-level sandbox opening because the PulseAudio protocol can expose
both playback and capture capabilities to native code. Smudgy's CPAL integration
opens output only, and Web Audio provides no capture API or operation. Sandboxed
packages receive no new module-loading, filesystem, network, subprocess, FFI,
system-information, or capture authority; all existing checks remain in force.
The trusted Main isolate is already allow-all, including FFI. Native code it is
already authorized to load can inherit the Flatpak's new audio-server
reachability, including protocol-level capture capability. That residual is why
the static socket grant is broader than the Web Audio output API.

## Manual physical checks

### Current evidence

On 2026-08-20 `./bin/release.ps1 -BuildOnly` completed the unsigned
`release-full` physical-feature build. A validation-only unsigned Inno compile
then assembled the installer, and its compiler log confirmed that the app,
inspector, runtime DLLs, and `THIRD-PARTY-NOTICES.md` were included. That
installer was neither signed nor launched, so this is build/package-assembly
evidence only.

The ignored Windows/WASAPI lifecycle smoke also passed with:

```powershell
cargo test -p smudgy_audio --features physical-output --lib `
  system::tests::manual_default_device_silent_open_suspend_resume_close_and_repeat `
  --locked -- --ignored --exact --nocapture
```

The test opened the default device, exercised suspend/resume and logical input
shutdown, and reported `shutdown.clean == true` with `failure == None`. An
immediate second service open/shutdown also reported clean/none.

Windows live device removal, audible playback, and default-device change remain
pending. Physical runtime exercises on Linux and macOS also remain pending.

CI is configured to compile the physical feature and run injected-output tests
on Windows, Linux, and macOS, but it never requires a default audio device.
Configuration and compile results are not runtime evidence. The current support
table is intentionally evidence-bounded:

| Platform | Backend | Current runtime evidence | Still pending |
| --- | --- | --- | --- |
| Windows | WASAPI | Default-device stream open, suspend/resume, clean close, and immediate reopen | Packaged-app launch, audible playback, ordered application shutdown, manual no-device boot with time-advancing default and `"none"` contexts, truthful not-applied controls, restart-only physical recovery, live removal, default-device change |
| Linux Flatpak | ALSA via PulseAudio/PipeWire | None; release manifest and CI are configuration only on this Windows-authored checkpoint | Packaged-app runtime, playback, ordered application shutdown, manual no-device boot with time-advancing default and `"none"` contexts, truthful not-applied controls, restart-only physical recovery, close/reopen, live removal, default-device change |
| macOS | CoreAudio | None; release script and CI are configuration only on this Windows-authored checkpoint | Packaged-app runtime, playback, ordered application shutdown, manual no-device boot with time-advancing default and `"none"` contexts, truthful not-applied controls, restart-only physical recovery, close/reopen, live removal, default-device change |

Deterministic injected tests separately cover unavailable startup, a
time-advancing emulated no-argument default context, `sinkId: "none"`, truthful
not-applied controls, and restart-only physical recovery. Those proofs do not
replace any packaged-app/manual gate, including a host with its device removed
or disabled.

For removal, keep at least two default-output contexts and one `sinkId: "none"`
context alive. Confirm the first operational failure seals new physical
admission and notifies each physical endpoint once, while the no-device context
continues independently. Then close the sessions and confirm the application
reports logical endpoint, callback/source, mixer-session, and physical-driver
cleanup separately.

For a default-device change, record whether the operating system keeps the
existing stream alive or reports failure; Smudgy does not promise live migration
in this release. A later service start should use the then-current default.

Smudgy's shutdown proof covers its own joined physical-driver loop and exact
logical callback/source retirement. CPAL and an operating-system backend may
use internal helper threads whose joins are not exposed. Dropping their RAII
objects is therefore not documented as proof that every opaque helper joined.
