#!/usr/bin/env python3
"""Verify Renovate proposes digest pins for both air-gap image inventories."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


EXPECTED_FILES = {"airgap/images.txt", "airgap/zarf.yaml"}
REPO_ROOT = Path(__file__).resolve().parents[2]
DIGEST = re.compile(r"@sha256:[a-f0-9]{64}")


def main() -> int:
    env = os.environ.copy()
    env.update(
        {
            "LOG_FORMAT": "json",
            "LOG_LEVEL": "debug",
            "RENOVATE_ENABLED_MANAGERS": "custom.regex",
            "RENOVATE_INCLUDE_PATHS": json.dumps(sorted(EXPECTED_FILES)),
        }
    )

    with tempfile.TemporaryDirectory(prefix="renovate-digest-test-") as temp:
        fixture_root = Path(temp)
        shutil.copy2(REPO_ROOT / "renovate.json5", fixture_root / "renovate.json5")
        for relative in EXPECTED_FILES:
            source = REPO_ROOT / relative
            target = fixture_root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(DIGEST.sub("", source.read_text()))

        result = subprocess.run(
            ["renovate", "--platform=local", "--dry-run=lookup"],
            check=False,
            cwd=fixture_root,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )

    pin_counts = {package_file: 0 for package_file in EXPECTED_FILES}
    diagnostics = []
    for line in result.stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue

        if event.get("level", 0) >= 40:
            diagnostics.append(event.get("msg", line))
        if event.get("msg") != "packageFiles with updates":
            continue

        # Renovate records the looked-up package files under `config` in this
        # debug event. The manager name (currently `regex`) is intentionally
        # not hard-coded so a future custom-manager migration remains visible.
        for manager_files in event.get("config", {}).values():
            for package in manager_files:
                package_file = package.get("packageFile")
                if package_file not in pin_counts:
                    continue
                pin_counts[package_file] += sum(
                    update.get("updateType") == "pinDigest"
                    and update.get("newDigest", "").startswith("sha256:")
                    for dependency in package.get("deps", [])
                    for update in dependency.get("updates", [])
                )

    missing = [path for path, count in pin_counts.items() if count == 0]
    if result.returncode or missing:
        print("Renovate digest-pinning integration check failed", file=sys.stderr)
        print(f"exit code: {result.returncode}", file=sys.stderr)
        print(f"pin counts: {pin_counts}", file=sys.stderr)
        if diagnostics:
            print("Renovate diagnostics:", file=sys.stderr)
            for diagnostic in diagnostics[-20:]:
                print(f"  - {diagnostic}", file=sys.stderr)
        return 1

    for package_file in sorted(pin_counts):
        print(f"{package_file}: {pin_counts[package_file]} digest pin(s) proposed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
