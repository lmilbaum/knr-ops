#!/usr/bin/env python3
"""Cross-check bootstrap.toml against the Git manifests (issue #98).

The chart versions knr-bootstrap installs imperatively must equal the
versions Flux reconciles from Git, or Flux cannot adopt the imperative
installs without drift. Fails when they disagree, when a declared
provider-manifest or sync path is missing, when an environment's kind
does not match its section name (the section name is authoritative for
profile resolution), or when the file does not parse.
Requires Python 3.11+ (tomllib); mise and CI both provide it.
"""

import re
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

# chart key in [charts] -> manifest files that must carry the same version
CHART_MANIFESTS = {
    "flux-operator": [
        "mgmt/aws/addons/flux-apps/flux-operator.yaml",
        "mgmt/local-host/addons/flux-apps/flux-operator.yaml",
    ],
    "cert-manager": [
        "mgmt/aws/infrastructure/cert-manager/helmrelease.yaml",
        "mgmt/local-host/infrastructure/cert-manager/helmrelease.yaml",
    ],
    "capi-operator": [
        "mgmt/aws/infrastructure/capi-operator/helmrelease.yaml",
        "mgmt/local-host/infrastructure/capi-operator/helmrelease.yaml",
    ],
}

VERSION_RE = re.compile(r'^\s*version:\s*"([^"]+)"\s*$', re.MULTILINE)


def manifest_version(path: Path) -> str:
    matches = VERSION_RE.findall(path.read_text())
    if len(matches) != 1:
        raise SystemExit(
            f"FAILED: expected exactly one quoted version: line in {path}, found {len(matches)}"
        )
    return matches[0]


def main() -> int:
    failures = []
    config_path = REPO_ROOT / "bootstrap.toml"
    try:
        config = tomllib.loads(config_path.read_text())
    except (OSError, tomllib.TOMLDecodeError) as error:
        print(f"FAILED: cannot parse {config_path}: {error}")
        return 1

    charts = config.get("charts", {})
    for chart, manifests in CHART_MANIFESTS.items():
        declared = charts.get(chart)
        if declared is None:
            failures.append(f"charts.{chart} missing from bootstrap.toml")
            continue
        for relative in manifests:
            actual = manifest_version(REPO_ROOT / relative)
            if actual != declared:
                failures.append(
                    f"charts.{chart} = {declared} but {relative} pins {actual}"
                )

    for name, env in config.get("environments", {}).items():
        kind = env.get("kind")
        if kind != name:
            failures.append(
                f"environments.{name} kind {kind!r} must match its section name"
            )
        sync = REPO_ROOT / env.get("sync-path", "")
        if not sync.is_dir():
            failures.append(f"environments.{name}.sync-path '{env.get('sync-path')}' is not a directory")
        for manifest in env.get("provider-manifests", []):
            if not (REPO_ROOT / manifest).is_file():
                failures.append(f"environments.{name} provider manifest missing: {manifest}")
        for fallback in env.get("move-fallbacks", []):
            if not (REPO_ROOT / fallback.get("manifest", "")).is_file():
                failures.append(
                    f"environments.{name} move-fallback manifest missing: "
                    f"{fallback.get('manifest')}"
                )

    if failures:
        print("bootstrap.toml cross-check FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print(f"bootstrap.toml cross-check OK ({len(CHART_MANIFESTS)} charts, "
          f"{len(config.get('environments', {}))} environments)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
