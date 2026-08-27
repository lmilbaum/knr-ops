#!/usr/bin/env python3
# Airgap inventory consistency check for the local-host profile.
#
# Builds every kustomize overlay under mgmt/local-host, collects container
# image references from the rendered output, and verifies each repository
# appears in airgap/images.txt. Images whose repository is listed under
# category [H] (host Docker daemon, not agent-rewriteable) are skipped.
#
# Modes:
#   default: exit 1 and print a table if any image repository is missing
#   --list : print all rendered image references, no exit-code effect
#
# Stdlib only (Python 3.11+). Works with either a standalone kustomize
# binary or `kubectl kustomize`; whichever is found first is used.

import argparse
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
OVERLAY_ROOT = REPO_ROOT / "mgmt" / "local-host"
IMAGES_FILE = REPO_ROOT / "airgap" / "images.txt"

# Keys in rendered manifests whose scalar value is a container image ref.
IMAGE_KEYS = ("image", "customImage")

# Repo names listed under category [H] in images.txt are host Docker
# daemon images consumed by kind/CAPD; Zarf cannot rewrite them, so the
# inventory check must not require them to be agent-rewriteable.
HOST_CATEGORY = "H"

_REF_LINE = re.compile(r"^[^\S\n]*(?P<key>image|customImage):\s*(?P<ref>[^\s#]+)\s*$")

def parse_inventory(text):
    # Parse images.txt: category comes from the most recent section header
    # comment containing a [M]/[H]/[W] marker; entries inherit it.
    # Returns {repository_name: category}.
    section = re.compile(r"^\s*#.*\[(?P<cat>[MHW])\]")
    entry = re.compile(r"^(?P<ref>[^\s#]+)\s*(?:#.*)?$")
    inventory = {}
    category = None
    for line in text.splitlines():
        m = section.match(line)
        if m:
            category = m.group("cat")
            continue
        if line.strip().startswith("#") or not line.strip():
            continue
        m = entry.match(line)
        if m:
            repo = repo_name(m.group("ref"))
            inventory.setdefault(repo, category)
    return inventory

def repo_name(ref):
    # Strip tag or digest, keeping the repository name exactly as written.
    ref = ref.strip().strip("'\"")
    ref = re.sub(r"(@sha256:[a-f0-9]+)$", "", ref)
    ref = re.sub(r"(:[^:/@]+)$", "", ref)
    return ref

def extract_images(rendered_text):
    # Collect image refs from rendered YAML text without a YAML parser.
    refs = []
    for line in rendered_text.splitlines():
        m = _REF_LINE.match(line)
        if m and m.group("key") in IMAGE_KEYS:
            refs.append(repo_name(m.group("ref")))
    return refs

def missing_images(rendered_repos, inventory):
    # Pure function: given rendered repository names (iterable) and an
    # inventory mapping {repo: category or None}, return the sorted list
    # of repositories that are missing entirely (not present under any
    # category). Repos present under [H] are host-side and not checked.
    missing = []
    for repo in sorted(set(rendered_repos)):
        cat = inventory.get(repo)
        if cat is None:
            missing.append(repo)
        elif cat == HOST_CATEGORY:
            continue  # host-side image, not rewriteable; skip
    return missing

def find_kustomize():
    if shutil.which("kustomize"):
        return ["kustomize", "build"]
    if shutil.which("kubectl"):
        return ["kubectl", "kustomize"]
    return None

def build_overlays(cmd, overlay_root):
    # Build every overlay containing a kustomization.yaml under overlay_root.
    rendered = {}
    overlays = sorted(
        p.parent for p in overlay_root.rglob("kustomization.yaml")
    )
    if not overlays:
        raise SystemExit(f"error: no kustomization.yaml found under {overlay_root}")
    for overlay in overlays:
        proc = subprocess.run(
            cmd + [str(overlay)], capture_output=True, text=True, check=False
        )
        if proc.returncode != 0:
            raise SystemExit(
                f"error: {' '.join(cmd)} failed for {overlay}:\n{proc.stderr}"
            )
        rendered[str(overlay.relative_to(REPO_ROOT))] = proc.stdout
    return rendered

def main():
    parser = argparse.ArgumentParser(
        description="Check that every image in mgmt/local-host rendered "
        "manifests appears in airgap/images.txt"
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="print all rendered image references and exit",
    )
    args = parser.parse_args()

    cmd = find_kustomize()
    if cmd is None:
        raise SystemExit("error: neither kustomize nor kubectl found in PATH")

    rendered = build_overlays(cmd, OVERLAY_ROOT)
    inventory = parse_inventory(IMAGES_FILE.read_text())

    rendered_repos = []
    for overlay, text in sorted(rendered.items()):
        rendered_repos.extend(extract_images(text))

    if args.list:
        for repo in sorted(set(rendered_repos)):
            cat = inventory.get(repo, "-")
            print(f"{repo} [{cat}]")
        return 0

    missing = missing_images(rendered_repos, inventory)
    if missing:
        print(f"missing from {IMAGES_FILE.relative_to(REPO_ROOT)}:")
        print(f"{'repository':<60}")
        print("-" * 60)
        for repo in missing:
            print(f"{repo:<60}")
        return 1
    print(
        f"ok: {len(set(rendered_repos))} rendered image repositories "
        "all present in airgap/images.txt"
    )
    return 0

if __name__ == "__main__":
    sys.exit(main())
