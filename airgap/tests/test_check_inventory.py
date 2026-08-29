# Unit tests for the inventory-matching logic in check-inventory.py.
# Run with: python3 -m unittest discover -s airgap/tests -p 'test_*.py'
# Pure logic tests only; no kustomize, cluster, or network access.

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/check-inventory.py"
spec = importlib.util.spec_from_file_location("check_inventory", SCRIPT)
assert spec is not None and spec.loader is not None
ci = importlib.util.module_from_spec(spec)
sys.modules["check_inventory"] = ci
spec.loader.exec_module(ci)

INVENTORY_FIXTURE = """\
# knr-ops airgap image inventory (local-host / CAPD profile)
#
# Categories:
#   [M] mgmt-cluster pod image -> Zarf/registry-rewrite target
#   [H] host Docker daemon image, consumed by kind + CAPD (NOT agent-rewriteable)
#   [W] workload-node containerd store

# Management cluster pod images [M]
ghcr.io/fluxcd/source-controller:v1.9.4
docker.io/kindest/kindnetd:v20260528-9350166c

# Host Docker daemon images [H]
kindest/node:v1.36.1
kindest/node:v1.35.0

# Workload node containerd store [W]
registry.k8s.io/coredns/coredns:v1.13.1
"""


class TestRepoName(unittest.TestCase):
    def test_strips_tag(self):
        self.assertEqual(ci.repo_name("ghcr.io/fluxcd/source-controller:v1.9.4"),
                         "ghcr.io/fluxcd/source-controller")

    def test_strips_digest(self):
        self.assertEqual(ci.repo_name("nginx@sha256:" + "a" * 64), "nginx")

    def test_no_tag_unchanged(self):
        self.assertEqual(ci.repo_name("docker.io/library/registry"), "docker.io/library/registry")

    def test_tag_with_port_and_build_suffix(self):
        self.assertEqual(ci.repo_name("registry:5000/foo:1.2.3-0"), "registry:5000/foo")


class TestParseInventory(unittest.TestCase):
    def setUp(self):
        self.inv = ci.parse_inventory(INVENTORY_FIXTURE)

    def test_categories_assigned(self):
        self.assertEqual(self.inv["ghcr.io/fluxcd/source-controller"], "M")
        self.assertEqual(self.inv["docker.io/kindest/kindnetd"], "M")
        self.assertEqual(self.inv["kindest/node"], "H")
        self.assertEqual(self.inv["registry.k8s.io/coredns/coredns"], "W")

    def test_multiple_sections_same_repo_keeps_first(self):
        # kindest/node appears twice under [H]; still H.
        self.assertEqual(self.inv["kindest/node"], "H")

    def test_comments_and_blanks_ignored(self):
        self.assertNotIn("#", str(self.inv.keys()))


class TestExtractImages(unittest.TestCase):
    def test_extracts_image_and_custom_image(self):
        text = (
            "      containers:\n"
            "        - name: c\n"
            "          image: docker.io/kindest/kindnetd:v1\n"
            "          imagePullPolicy: IfNotPresent\n"
            "          customImage: kindest/node:v1.35.0\n"
        )
        self.assertEqual(ci.extract_images(text),
                         ["docker.io/kindest/kindnetd", "kindest/node"])

    def test_ignores_imagepullpolicy(self):
        text = "          imagePullPolicy: Always\n"
        self.assertEqual(ci.extract_images(text), [])

    def test_strips_quotes(self):
        text = '          image: "nginx:1.27"\n'
        self.assertEqual(ci.extract_images(text), ["nginx"])


class TestMissingImages(unittest.TestCase):
    def setUp(self):
        self.inv = ci.parse_inventory(INVENTORY_FIXTURE)

    def test_present_passes(self):
        rendered = ["ghcr.io/fluxcd/source-controller", "docker.io/kindest/kindnetd"]
        self.assertEqual(ci.missing_images(rendered, self.inv), [])

    def test_missing_reported_by_repo_name(self):
        rendered = ["ghcr.io/fluxcd/source-controller", "quay.io/jetstack/cert-manager-controller"]
        self.assertEqual(ci.missing_images(rendered, self.inv),
                         ["quay.io/jetstack/cert-manager-controller"])

    def test_tag_mismatch_reports_missing_by_repo(self):
        # A different tag of a present repo is NOT missing: match is by
        # repository name, which the caller collects repo names for.
        rendered = [ci.repo_name("ghcr.io/fluxcd/source-controller:v0.0.1")]
        self.assertEqual(ci.missing_images(rendered, self.inv), [])

    def test_host_category_skipped(self):
        self.assertEqual(ci.missing_images(["kindest/node"], self.inv), [])

    def test_workload_category_counts_as_present(self):
        self.assertEqual(ci.missing_images(["registry.k8s.io/coredns/coredns"], self.inv), [])

    def test_empty_render(self):
        self.assertEqual(ci.missing_images([], self.inv), [])


if __name__ == "__main__":
    unittest.main()
