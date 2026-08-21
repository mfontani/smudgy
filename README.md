# smudgy

A modern MUD client for Windows, macOS, and Linux, built in Rust.

**[www.smudgy.org](https://www.smudgy.org)** — downloads, documentation, and
the scripting reference.

- Fast GPU-rendered terminal with themes, flexible pane layouts, and
  clickable links
- A full mapper: shared cloud maps, auto-routing and speedwalks, and a real
  map editor
- TypeScript scripting on an embedded Deno-based runtime — triggers, aliases,
  widgets, GMCP — with a package ecosystem for sharing scripts, sandboxed by
  default
- A source-build [Web Audio preview](WEB_AUDIO.md) for bounded accessibility
  earcons in trusted modules and sandboxed packages
- Works fully offline; a free cloud account adds map sharing, social
  features, and package publishing

## Building from source

```sh
cargo install patch-crate
cargo patch-crate
cargo run
```

builds and runs the client with a stable Rust toolchain. The first two
commands are a one-time setup: they materialize the workspace's dependency
source patches (see [patches/](patches/)) under `target/patch/` before the
first build, and again after a `cargo clean`. See
[CHANGELOG.md](CHANGELOG.md) for what's new.

The default and official release builds remain hardware-free. See
[Web Audio preview](WEB_AUDIO.md) for the explicit silent and physical feature
configurations and their current support boundary.

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
