#!/usr/bin/env python3
"""Fail when Smudgy's reviewed Web Audio release wiring drifts."""

from __future__ import annotations

import pathlib
import sys
import tomllib


ROOT = pathlib.Path(__file__).resolve().parent.parent
DENO_AUDIO_REV = "2998104e06b1e6021855902a65427df85ba5d8f3"
WEB_AUDIO_API_REV = "06bcc862de63edb8de6f6b8a38b04f8b5249cf1a"


def load_toml(relative: str) -> dict:
    with (ROOT / relative).open("rb") as source:
        return tomllib.load(source)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def require_exact_git_dependency(
    dependency: dict,
    *,
    name: str,
    git: str,
    revision: str,
) -> None:
    require(
        dependency == {"git": git, "rev": revision, "default-features": False},
        f"{name} must use only the reviewed immutable pin with defaults disabled",
    )


def require_feature(manifest: dict, name: str, expected: set[str], *, crate: str) -> None:
    actual = set(manifest.get("features", {}).get(name, []))
    require(
        actual == expected,
        f"{crate} feature {name!r}: expected {sorted(expected)}, got {sorted(actual)}",
    )


def require_optional_workspace_edge(dependency: dict, *, crate: str, name: str) -> None:
    require(
        dependency == {"workspace": True, "optional": True},
        f"{crate} {name} must be exactly optional and inherit the reviewed workspace pin",
    )


def dependency_sites(manifest: dict, dependency: str) -> set[str]:
    sites = {
        section
        for section in ("dependencies", "dev-dependencies", "build-dependencies")
        if dependency in manifest.get(section, {})
    }
    for target, target_manifest in manifest.get("target", {}).items():
        for section in ("dependencies", "dev-dependencies", "build-dependencies"):
            if dependency in target_manifest.get(section, {}):
                sites.add(f"target.{target}.{section}")
    return sites


def main() -> None:
    workspace = load_toml("Cargo.toml")
    ui = load_toml("ui/Cargo.toml")
    core = load_toml("core/Cargo.toml")
    script = load_toml("script/Cargo.toml")
    audio = load_toml("audio/Cargo.toml")
    audio_web = load_toml("audio_web/Cargo.toml")

    require_exact_git_dependency(
        workspace["workspace"]["dependencies"]["deno_audio"],
        name="deno_audio",
        git="https://github.com/smudgy-mud/deno_audio.git",
        revision=DENO_AUDIO_REV,
    )
    require_exact_git_dependency(
        audio_web["dev-dependencies"]["web-audio-api"],
        name="web-audio-api-rs",
        git="https://github.com/smudgy-mud/web-audio-api-rs",
        revision=WEB_AUDIO_API_REV,
    )
    workspace_dependencies = workspace["workspace"]["dependencies"]
    require(
        workspace_dependencies["cpal"] == {
            "version": "=0.18.2",
            "default-features": False,
        },
        "workspace CPAL must remain exact 0.18.2 with defaults disabled",
    )
    require(
        workspace_dependencies["kira"] == {
            "version": "0.12.3",
            "default-features": False,
        },
        "workspace Kira must remain 0.12.3 with defaults disabled",
    )

    require_optional_workspace_edge(
        audio["dependencies"]["cpal"], crate="smudgy_audio", name="cpal"
    )
    require(
        audio["dependencies"]["kira"] == {"workspace": True},
        "smudgy_audio Kira edge drifted",
    )
    require(
        audio_web["dependencies"]["deno_audio"] == {"workspace": True},
        "smudgy_audio_web deno_audio edge drifted",
    )
    require_optional_workspace_edge(
        script["build-dependencies"]["deno_audio"],
        crate="smudgy_script build",
        name="deno_audio",
    )
    require(
        script["dev-dependencies"]["deno_audio"] == {"workspace": True},
        "smudgy_script test-only deno_audio edge drifted",
    )
    for dependency in ("deno_audio", "smudgy_audio_web"):
        require_optional_workspace_edge(
            core["dependencies"][dependency], crate="smudgy_core", name=dependency
        )
    for dependency in ("futures", "smudgy_audio", "smudgy_audio_web"):
        require_optional_workspace_edge(
            ui["dependencies"][dependency], crate="smudgy_ui", name=dependency
        )

    allowed_consumers = {
        "cpal": {"audio:dependencies"},
        "kira": {"audio:dependencies"},
        "deno_audio": {
            "audio_web:dependencies",
            "core:dependencies",
            "script:build-dependencies",
            "script:dev-dependencies",
        },
    }
    observed_consumers = {dependency: set() for dependency in allowed_consumers}
    # theme/ and widgets/ are first-party UI path dependencies but deliberately
    # not workspace members, so include them in the native-stack escape scan.
    reviewed_crates = [*workspace["workspace"]["members"], "theme", "widgets"]
    for member in reviewed_crates:
        manifest = load_toml(f"{member}/Cargo.toml")
        for dependency in observed_consumers:
            observed_consumers[dependency].update(
                f"{member}:{site}"
                for site in dependency_sites(manifest, dependency)
            )
    for dependency, expected in allowed_consumers.items():
        require(
            observed_consumers[dependency] == expected,
            f"{dependency} consumers: expected {sorted(expected)}, "
            f"got {sorted(observed_consumers[dependency])}",
        )

    require_feature(audio, "physical-output", {"dep:cpal"}, crate="smudgy_audio")
    require_feature(script, "web-audio", {"dep:deno_audio"}, crate="smudgy_script")
    require_feature(
        core,
        "web-audio",
        {"dep:deno_audio", "dep:smudgy_audio_web", "smudgy_script/web-audio"},
        crate="smudgy_core",
    )
    require_feature(core, "web-audio-cpal", {"web-audio"}, crate="smudgy_core")
    require_feature(ui, "web-audio", {"smudgy_core/web-audio"}, crate="smudgy_ui")
    require_feature(
        ui,
        "web-audio-cpal",
        {
            "web-audio",
            "dep:futures",
            "dep:smudgy_audio",
            "dep:smudgy_audio_web",
            "smudgy_audio/physical-output",
        },
        crate="smudgy_ui",
    )

    for crate, manifest in (
        ("smudgy_ui", ui),
        ("smudgy_core", core),
        ("smudgy_script", script),
        ("smudgy_audio", audio),
    ):
        require(
            not manifest.get("features", {}).get("default"),
            f"{crate} defaults must remain hardware-free",
        )

    lock = load_toml("Cargo.lock")
    locked_by_name: dict[str, list[dict]] = {}
    for package in lock["package"]:
        locked_by_name.setdefault(package["name"], []).append(package)

    def require_unique_locked(name: str, version: str, source: str) -> None:
        packages = locked_by_name.get(name, [])
        require(len(packages) == 1, f"Cargo.lock must contain exactly one {name}")
        require(packages[0].get("version") == version, f"Cargo.lock {name} version drifted")
        require(packages[0].get("source") == source, f"Cargo.lock {name} source drifted")

    registry = "registry+https://github.com/rust-lang/crates.io-index"
    require_unique_locked("cpal", "0.18.2", registry)
    require_unique_locked("kira", "0.12.3", registry)
    require_unique_locked(
        "deno_audio",
        "0.1.0-alpha.1",
        f"git+https://github.com/smudgy-mud/deno_audio.git?rev={DENO_AUDIO_REV}#{DENO_AUDIO_REV}",
    )
    require_unique_locked(
        "web-audio-api",
        "1.7.0",
        f"git+https://github.com/smudgy-mud/web-audio-api-rs?rev={WEB_AUDIO_API_REV}#{WEB_AUDIO_API_REV}",
    )

    # Release-readiness intentionally leaves shipping artifacts hardware-free.
    # Accessible mixer controls must land before all three paths are enabled
    # together and this gate is revised in the same change.
    for relative in (
        "bin/release.ps1",
        "bin/release-mac.sh",
        "packaging/linux/org.smudgy.Smudgy.yml",
        ".github/workflows/release.yml",
    ):
        release_text = (ROOT / relative).read_text(encoding="utf-8")
        if relative == ".github/workflows/release.yml":
            cargo_about_install = "cargo install --locked cargo-about --features cli"
            require(
                release_text.count(cargo_about_install) == 2,
                "release workflow cargo-about setup drifted",
            )
            release_text = release_text.replace(cargo_about_install, "")
        for marker in (
            "--features",
            "--all-features",
            "web-audio-cpal",
            "smudgy_audio/physical-output",
        ):
            require(
                marker not in release_text,
                f"{relative} contains {marker!r} before accessible controls",
            )

    print("Web Audio dependency, feature, lockfile, and release-readiness wiring is exact.")


if __name__ == "__main__":
    try:
        main()
    except (KeyError, OSError, RuntimeError, tomllib.TOMLDecodeError) as error:
        print(f"Web Audio wiring audit failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
