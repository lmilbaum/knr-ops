#!/usr/bin/env bash
# build-package.sh — connected-side package build.
# Version bumps must update HOST_IMAGES, mise.toml's Zarf pin, stage-and-create-cluster.sh's KIND_NODE_IMAGE, and cluster-class.yaml's customImage together.
#
#   1. mise run validate (all overlays still build)
#   2. build-config-artifact.sh (trimmed airgap tree -> configured OCI registry)
#   3. stage the Zarf init package, host-daemon images, workload-node pod
#      images, and OCI charts into archives/
#   4. zarf package create
#
# Output: zarf-package-knr-ops-airgap-arm64-0.1.0.tar.zst next to airgap/.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

echo "==> 1/4 validate"
mise run validate

echo "==> 2/4 config artifact"
"$SCRIPT_DIR/build-config-artifact.sh"

# The package must pull the same config artifact that the preceding step
# published. Keep zarf.yaml's local-registry default for ordinary builds, but
# temporarily rewrite its image reference when OCI_REGISTRY is overridden.
if [ -n "${OCI_REGISTRY:-}" ]; then
  OCI_REPOSITORY="${OCI_REPOSITORY:-knr-ops-airgap}"
  OCI_TAG="${OCI_TAG:-latest}"
  ZARF_CONFIG="$REPO_ROOT/airgap/zarf.yaml"
  ZARF_CONFIG_BACKUP=$(mktemp "${TMPDIR:-/tmp}/knr-ops-zarf.XXXXXX")
  cp "$ZARF_CONFIG" "$ZARF_CONFIG_BACKUP"
  restore_zarf_config() {
    cp "$ZARF_CONFIG_BACKUP" "$ZARF_CONFIG"
    rm -f "$ZARF_CONFIG_BACKUP"
  }
  trap restore_zarf_config EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  EXPECTED_ARTIFACT="localhost:5001/knr-ops-airgap:latest"
  CONFIG_ARTIFACT="${OCI_REGISTRY}/${OCI_REPOSITORY}:${OCI_TAG}"
  MATCH_COUNT=$(grep -Fc "$EXPECTED_ARTIFACT" "$ZARF_CONFIG" || true)
  if [ "$MATCH_COUNT" -ne 1 ]; then
    echo "ERROR: expected exactly one config artifact reference: ${EXPECTED_ARTIFACT}" >&2
    exit 1
  fi
  sed -i.bak "s|${EXPECTED_ARTIFACT}|${CONFIG_ARTIFACT}|" "$ZARF_CONFIG"
  rm -f "${ZARF_CONFIG}.bak"
  echo "    zarf config artifact: ${CONFIG_ARTIFACT}"
fi

echo "==> 3/4 offline host assets, workload-node images, and OCI charts"
mkdir -p airgap/archives

mise x -- zarf tools download-init \
  --architecture arm64 \
  --output-directory airgap/archives

HOST_IMAGES=(
  kindest/node:v1.36.1
  kindest/node:v1.35.0
  kindest/haproxy:v20230606-42a2262b
  docker.io/library/registry:2
)
for img in "${HOST_IMAGES[@]}"; do
  docker pull --platform linux/arm64 "$img" >/dev/null
done
docker save -o airgap/archives/kindest_node_v1.36.1_mgmt.tar kindest/node:v1.36.1
docker save -o airgap/archives/kindest_node_v1.35.0.tar kindest/node:v1.35.0
docker save -o airgap/archives/kindest_haproxy_v20230606-42a2262b.tar kindest/haproxy:v20230606-42a2262b
docker save -o airgap/archives/docker.io_library_registry_2.tar docker.io/library/registry:2
echo "    saved Zarf init package and host-daemon image archives"

WORKLOAD_IMAGES=(
  registry.k8s.io/pause:3.10.1
  docker.io/kindest/kindnetd:v20260528-9350166c
  ghcr.io/controlplaneio-fluxcd/flux-operator:v0.58.0
  ghcr.io/fluxcd/source-controller:v1.9.4
  ghcr.io/fluxcd/kustomize-controller:v1.9.4
  ghcr.io/fluxcd/helm-controller:v1.6.3
  ghcr.io/fluxcd/notification-controller:v1.9.3
  ghcr.io/stefanprodan/podinfo:6.14.0
)
for img in "${WORKLOAD_IMAGES[@]}"; do
  docker pull --platform linux/arm64 "$img" >/dev/null
done
docker save -o airgap/archives/workload-pod-images.tar "${WORKLOAD_IMAGES[@]}"
echo "    saved airgap/archives/workload-pod-images.tar"

# OCI charts the workload cluster needs in the gap (seeded into knr-registry
# by the stage script): the per-cluster flux-operator chart (HelmChartProxy)
# and the podinfo chart (workload HelmRelease).
mkdir -p airgap/archives/charts
helm pull oci://ghcr.io/controlplaneio-fluxcd/charts/flux-operator --version 0.58.0 -d airgap/archives/charts
helm pull oci://ghcr.io/stefanprodan/charts/podinfo --version 6.14.0 -d airgap/archives/charts
echo "    staged charts: $(ls airgap/archives/charts/)"

echo "==> 4/4 zarf package create"
cd airgap
mise x -- zarf package create . --confirm

echo "==> Built:"
ls -lh zarf-package-knr-ops-airgap-*.tar.zst
