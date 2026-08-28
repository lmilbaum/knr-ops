#!/usr/bin/env python3
"""Validate the Syft JSON SBOMs extracted from a signed Zarf package."""

import json
from pathlib import Path
import sys


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: verify-sbom.py SBOM_DIRECTORY", file=sys.stderr)
        return 2

    root = Path(sys.argv[1])
    documents = sorted(root.rglob("*.json"))
    if not documents:
        print(f"ERROR: no Syft JSON SBOMs found under {root}", file=sys.stderr)
        return 1

    errors: list[str] = []
    artifact_count = 0
    for document in documents:
        try:
            sbom = json.loads(document.read_text())
        except (OSError, json.JSONDecodeError) as error:
            errors.append(f"{document}: invalid JSON: {error}")
            continue

        descriptor = sbom.get("descriptor", {})
        if not isinstance(descriptor.get("name"), str) or not descriptor.get("name"):
            errors.append(f"{document}: descriptor.name is missing")
        if not isinstance(sbom.get("artifacts"), list):
            errors.append(f"{document}: artifacts is not a list")
            continue
        if not isinstance(sbom.get("source"), dict):
            errors.append(f"{document}: source is not an object")
        schema = sbom.get("schema")
        if not isinstance(schema, dict):
            errors.append(f"{document}: schema is not an object")
        elif (
            not isinstance(schema.get("version"), str)
            or not schema["version"]
            or "anchore/syft" not in schema.get("url", "")
        ):
            errors.append(f"{document}: schema is not an Anchore Syft JSON schema")
        artifact_count += len(sbom["artifacts"])

    if errors:
        print("SBOM validation failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1

    print(
        f"validated {len(documents)} Syft JSON SBOM document(s) "
        f"containing {artifact_count} artifact(s)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
