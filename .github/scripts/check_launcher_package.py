#!/usr/bin/env python3

"""Check the staged launcher package against the launcher it ships.

`bin/codex.js` resolves its native binary through an optional dependency, by
name, at runtime. `build_npm_package.py` writes those names into the staged
`package.json`. Nothing connects the two but string equality, and a mismatch is
invisible until an install on a user's machine cannot find its binary — by
which point the version is spent, because npm will not let it be republished.

Run against the staging directory and tarball that a `--package codex` build
just produced. Expects STAGE_DIR, TARBALL and VERSION in the environment, or
falls back to the paths the `tests` workflow uses.
"""

import json
import os
import re
import tarfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
LAUNCHER = REPO_ROOT / "codex-cli" / "bin" / "codex.js"

NPM_NAME = "codex-rewind"

# The dist-tags release.yml publishes the platform builds under, which are also
# the suffixes `compute_platform_package_version` appends.
PLATFORM_TAGS = (
    "linux-x64",
    "linux-arm64",
    "darwin-x64",
    "darwin-arm64",
    "win32-x64",
    "win32-arm64",
)

REQUIRED_TARBALL_ENTRIES = (
    "package/package.json",
    "package/bin/codex.js",
    # Apache-2.0 §4(a) and §4(d). A release once shipped without these.
    "package/LICENSE",
    "package/NOTICE",
)


def main() -> None:
    runner_temp = Path(os.environ.get("RUNNER_TEMP", "/tmp"))
    stage_dir = Path(os.environ.get("STAGE_DIR", runner_temp / "stage"))
    tarball = Path(os.environ.get("TARBALL", runner_temp / "launcher.tgz"))
    version = os.environ["VERSION"]

    failures: list[str] = []

    staged = json.loads((stage_dir / "package.json").read_text(encoding="utf-8"))

    if staged.get("name") != NPM_NAME:
        failures.append(f"staged name is {staged.get('name')!r}, expected {NPM_NAME!r}")

    if staged.get("version") != version:
        failures.append(
            f"staged version is {staged.get('version')!r}, expected {version!r}"
        )

    # The bin name is the command users type. Renaming it silently is a broken
    # upgrade for everyone who already has it on their PATH.
    if staged.get("bin") != {"codexr": "bin/codex.js"}:
        failures.append(f"staged bin is {staged.get('bin')!r}, expected codexr")

    # `optionalDependencies` is how npm installs a binary at all, and the
    # launcher's own table is the only other place these names appear.
    optional = staged.get("optionalDependencies") or {}
    launcher_source = LAUNCHER.read_text(encoding="utf-8")
    named_in_launcher = set(re.findall(rf'"({NPM_NAME}-[a-z0-9-]+)"', launcher_source))

    expected = {f"{NPM_NAME}-{tag}" for tag in PLATFORM_TAGS}

    if set(optional) != expected:
        failures.append(
            "optionalDependencies name "
            f"{sorted(set(optional))}, expected {sorted(expected)}"
        )
    if named_in_launcher != expected:
        failures.append(
            f"bin/codex.js names {sorted(named_in_launcher)}, expected {sorted(expected)}"
        )

    # Pinned by exact version on purpose: release.yml publishes the launcher
    # last precisely because these have to already resolve.
    for tag in PLATFORM_TAGS:
        alias = f"{NPM_NAME}-{tag}"
        want = f"npm:{NPM_NAME}@{version}-{tag}"
        got = optional.get(alias)
        if got != want:
            failures.append(f"{alias} resolves to {got!r}, expected {want!r}")

    with tarfile.open(tarball) as archive:
        entries = set(archive.getnames())
    for required in REQUIRED_TARBALL_ENTRIES:
        if required not in entries:
            failures.append(f"tarball is missing {required}")

    if failures:
        print("The staged launcher package would not install correctly:")
        for failure in failures:
            print(f"  - {failure}")
        raise SystemExit(1)

    print(f"launcher package {version} stages and packs correctly")
    print(f"  tarball entries: {len(entries)}")
    for alias in sorted(optional):
        print(f"  {alias} -> {optional[alias]}")


if __name__ == "__main__":
    main()
