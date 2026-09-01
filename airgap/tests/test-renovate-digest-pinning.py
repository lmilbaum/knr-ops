#!/usr/bin/env python3
"""Verify Renovate proposes digest pins for both air-gap image inventories."""

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tests"))
from renovate_harness import run_renovate

EXPECTED_FILES = ["airgap/images.txt", "airgap/zarf.yaml"]
DIGEST = re.compile(r"@sha256:[a-f0-9]{64}")


def main() -> int:
    # Strip existing digests so the only pinDigest proposals Renovate can
    # make are new ones: proves the managers propose pins for every ref.
    result = run_renovate(
        EXPECTED_FILES, transform=lambda _, text: DIGEST.sub("", text)
    )

    pin_counts = {
        package_file: result.pin_digest_count(package_file)
        for package_file in EXPECTED_FILES
    }
    missing = [path for path, count in pin_counts.items() if count == 0]
    if result.returncode or missing:
        print("Renovate digest-pinning integration check failed", file=sys.stderr)
        print(f"exit code: {result.returncode}", file=sys.stderr)
        print(f"pin counts: {pin_counts}", file=sys.stderr)
        result.print_diagnostics()
        return 1

    for package_file in sorted(pin_counts):
        print(f"{package_file}: {pin_counts[package_file]} digest pin(s) proposed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
