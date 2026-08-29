#!/usr/bin/env python3
"""Verify Renovate detects the pins that replaced the version catalog.

The catalog drift check was retired; these detection checks are what proves
a bump cannot silently stop being proposed for the non-manifest surfaces
(issue #74 acceptance criteria). Each entry maps a tracked file to the
depNames Renovate must extract from it via custom.regex managers.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


EXPECTED = {
    "bootstrap-rs/src/main.rs": {"ghcr.io/controlplaneio-fluxcd/charts/flux-operator"},
    "pivot.sh": {"cert-manager", "cluster-api-operator"},
    ".github/workflows/validate.yml": {"renovate"},
}
REPO_ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    env = os.environ.copy()
    env.update(
        {
            "LOG_FORMAT": "json",
            "LOG_LEVEL": "debug",
            "RENOVATE_ENABLED_MANAGERS": "custom.regex",
            "RENOVATE_INCLUDE_PATHS": json.dumps(sorted(EXPECTED)),
        }
    )

    # The fixture copies the real renovate.json5 and the real tracked files
    # so the test exercises the exact patterns and shapes that ship.
    # platform=local + dry-run=lookup performs no writes and opens no PRs.
    with tempfile.TemporaryDirectory(prefix="renovate-coverage-test-") as temp:
        fixture_root = Path(temp)
        shutil.copy2(REPO_ROOT / "renovate.json5", fixture_root / "renovate.json5")
        for relative in EXPECTED:
            source = REPO_ROOT / relative
            target = fixture_root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)

        result = subprocess.run(
            ["renovate", "--platform=local", "--dry-run=lookup"],
            check=False,
            cwd=fixture_root,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )

    detected: dict[str, set[str]] = {path: set() for path in EXPECTED}
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
                if package_file not in detected:
                    continue
                for dependency in package.get("deps", []):
                    dep_name = dependency.get("depName")
                    if dep_name:
                        detected[package_file].add(dep_name)

    missing = {
        path: sorted(EXPECTED[path] - detected[path])
        for path in EXPECTED
        if EXPECTED[path] - detected[path]
    }
    if result.returncode or missing:
        print("Renovate coverage check failed", file=sys.stderr)
        print(f"exit code: {result.returncode}", file=sys.stderr)
        if missing:
            for path, deps in sorted(missing.items()):
                print(f"{path}: missing detection for {deps}", file=sys.stderr)
        if diagnostics:
            print("Renovate diagnostics:", file=sys.stderr)
            for diagnostic in diagnostics[-20:]:
                print(f"  - {diagnostic}", file=sys.stderr)
        return 1

    for path in sorted(detected):
        print(f"{path}: pin(s) detected: {sorted(detected[path])}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
