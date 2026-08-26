# EKS Management Cluster for the AWS Bootstrap Path

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Add a new step to knr-ops's `mise bootstrap` flow that provisions a real EKS cluster with `clusterawsadm` + `aws` CLI, so the AWS deployment path can run its management plane on EKS instead of a local kind cluster.

**Architecture:** Today `bootstrap.sh` (driven by `[tasks.bootstrap]` in `mise.toml`) always creates a kind cluster named `mgmt`, then installs the Flux Operator, the GitHub PAT + SOPS age secrets, and a `FluxInstance` that syncs `mgmt/aws/`. This plan adds a `MGMT_PLATFORM=kind|eks` switch (default `kind`, fully backward compatible). With `MGMT_PLATFORM=eks`, Step 1 provisions an EKS cluster (`knr-ops-mgmt`) in the management AWS account using the IAM roles from the existing `clusterawsadm` CloudFormation stack, points kubectl at it, and leaves every later bootstrap step untouched. GitOps contents (`mgmt/aws/`) do not change: Flux, CAPA (static `aws-credentials`), ACK, and the EKS workload clusters reconcile exactly as they do on kind today.

**Tech Stack:** bash (bootstrap.sh / teardown.sh), mise tasks, aws-cli + clusterawsadm (already pinned in `mise.aws.toml`), EKS managed node groups, Flux Operator/FluxInstance.

---

## Current context (verified against the repo)

- `mise.toml` `[tasks.bootstrap]` runs `./bootstrap.sh`; profile defaults to `aws`.
- `bootstrap.sh` Step 1 (lines 129-161) creates kind cluster `mgmt` and runs `kubectl config use-context kind-mgmt`. All later steps are cluster-agnostic (helm + kubectl against the current context).
- The container-engine preflight (lines 87-123) is only needed by the kind path; EKS needs no local engine.
- `mise.aws.toml` already pins `aws-cli` and `clusterawsadm 2.13.0`, sets `KNR_OPS_PROFILE=aws`, and provides `[tasks.aws-bootstrap]` (clusterawsadm CloudFormation stack) and `[tasks.kubeconfigs]`. The eks path therefore runs as `mise -E aws run bootstrap`.
- CAPA auth is static: `mgmt/aws/capi-providers/capa-system/aws-credentials.sops.yaml` (configSecret) + `AWSClusterControllerIdentity/default`. This plan keeps that unchanged (IRSA is a documented follow-up).
- `teardown.sh` Step 9 (lines 966-974) deletes the kind cluster via `_delete_kind_if_safe`; steps 1-8 are cluster-agnostic.
- `mise run validate` runs `bash -n` on `bootstrap.sh` and `teardown.sh` plus kustomize builds; CI mirrors it.
- Repo rules: never commit to `main`; work on a branch in a git worktree (concurrent sessions share `~/Code` clones); `mise run validate` before pushing; update `AGENTS.md` when workflows change.

## Assumptions (verify in Task 2 before relying on them)

- The `clusterawsadm bootstrap iam create-cloudformation-stack` stack (v2.13.0) creates roles `eks-controlplane.cluster-api-provider-aws.sigs.k8s.io` (trusts `eks.amazonaws.com`) and `eks-nodegroup.cluster-api-provider-aws.sigs.k8s.io` (trusts `ec2.amazonaws.com`) with the AWS-managed policies EKS needs. If verification shows otherwise, the fallback branch in Task 2 creates dedicated roles.
- The management account has a default VPC with public subnets in the mgmt region (default `eu-north-1`). Override via `MGMT_EKS_SUBNET_IDS` if not.
- Kubernetes version for the mgmt cluster: env `MGMT_EKS_VERSION`, default `1.33`; verify availability with `aws eks describe-cluster-versions --region <region>` and adjust if the region lags.

---

## Task 0: Worktree and feature branch

**Objective:** Isolate the work from other sessions sharing the clone; never touch `main`.

**Step 1: Create worktree and branch**

```bash
cd ~/Code/knr-ops
git worktree add ../knr-ops-eks-mgmt -b feat/eks-mgmt-platform main
cd ../knr-ops-eks-mgmt
```

Expected: new worktree at `~/Code/knr-ops-eks-mgmt` on branch `feat/eks-mgmt-platform`. All remaining tasks run inside this worktree. `.env` and `age.agekey` are gitignored; copy them in for the live run in Task 6:

```bash
cp ~/Code/knr-ops/.env ~/Code/knr-ops/age.agekey .
```

---

## Task 1: Preflight support for MGMT_PLATFORM in bootstrap.sh

**Objective:** Validate the new env var, require `aws`/`clusterawsadm` only on the eks path, and skip the container-engine requirement when no kind cluster will be created.

**Files:**
- Modify: `bootstrap.sh` (preflight_checks, lines 13-124)

**Step 1: Add platform validation at the top of `preflight_checks()`** (after the existing PROFILE case):

```bash
  MGMT_PLATFORM="${MGMT_PLATFORM:-kind}"
  case "$MGMT_PLATFORM" in
    kind|eks) ;;
    *)
      echo "ERROR: unsupported MGMT_PLATFORM '$MGMT_PLATFORM' (expected 'kind' or 'eks')" >&2
      exit 1
      ;;
  esac
  if [ "$MGMT_PLATFORM" = eks ]; then
    if [ "$PROFILE" != aws ]; then
      echo "ERROR: MGMT_PLATFORM=eks is only valid with the aws profile" >&2
      exit 1
    fi
    for cmd in aws clusterawsadm; do
      command -v "$cmd" >/dev/null 2>&1 \
        || { echo "ERROR: $cmd not found in PATH (run: mise -E aws install)"; exit 1; }
    done
    aws sts get-caller-identity >/dev/null 2>&1 \
      || { echo "ERROR: no valid AWS credentials for the management account"; exit 1; }
  fi
```

**Step 2: Gate the container-engine detection on the kind platform.** Wrap the existing engine-detection block (lines 87-123, from `if [ -z "${CONTAINER_ENGINE:-}" ]` through the closing `esac`) in:

```bash
  if [ "$MGMT_PLATFORM" = kind ]; then
    ...existing CONTAINER_ENGINE detection unchanged...
  fi
```

Also change the echo at line 127 to print only for kind, or extend it:

```bash
if [ "$MGMT_PLATFORM" = kind ]; then
  echo ">>> Using container engine: ${CONTAINER_ENGINE} (socket: ${ENGINE_SOCK})"
fi
```

**Step 3: Verify syntax**

Run: `bash -n bootstrap.sh && echo OK`
Expected: `OK`

**Step 4: Verify the guard rails**

```bash
MGMT_PLATFORM=bogus ./bootstrap.sh 2>&1 | head -2
KNR_OPS_PROFILE=local-host MGMT_PLATFORM=eks ./bootstrap.sh 2>&1 | head -2
```

Expected: the two ERROR messages above, exit code 1, before any cluster work starts.

**Step 5: Commit**

```bash
git add bootstrap.sh
git commit -m "feat(bootstrap): add MGMT_PLATFORM preflight for eks management cluster"
```

---

## Task 2: Add the EKS provisioning function to bootstrap.sh

**Objective:** A self-contained `create_eks_mgmt_cluster()` that resolves IAM roles (clusterawsadm stack first, dedicated fallback), resolves subnets (default VPC first, env override), creates the EKS cluster + managed node group idempotently, and switches kubectl context.

**Files:**
- Modify: `bootstrap.sh` (insert before the `# ── Step 1` section)

**Step 1: Verify the clusterawsadm stack roles exist in the management account** (read-only; informs which branch the live run takes):

```bash
aws iam get-role --role-name eks-controlplane.cluster-api-provider-aws.sigs.k8s.io --query 'Role.Arn' --output text
aws iam list-attached-role-policies --role-name eks-controlplane.cluster-api-provider-aws.sigs.k8s.io --query 'AttachedPolicies[].PolicyArn' --output text
aws iam list-attached-role-policies --role-name eks-nodegroup.cluster-api-provider-aws.sigs.k8s.io --query 'AttachedPolicies[].PolicyArn' --output text
```

Expected: control-plane role carries `AmazonEKSClusterPolicy` (and ideally `AmazonEKSVPCResourceController`); nodegroup role carries `AmazonEKSWorkerNodePolicy`, `AmazonEKS_CNI_Policy`, `AmazonEC2ContainerRegistryReadOnly`. If missing, the fallback below covers it; no script change needed.

**Step 2: Insert the functions.** Complete code:

```bash
# ── EKS management cluster helpers (MGMT_PLATFORM=eks) ───────────────────────
# ensure_role <name> <trusted-service> <policy-arn>...
# Idempotently creates an IAM role with a service trust policy and attaches the
# given AWS-managed policies. Prints the role ARN.
ensure_role() {
  local name="$1" service="$2"; shift 2
  local arn
  arn="$(aws iam get-role --role-name "$name" --query 'Role.Arn' --output text 2>/dev/null || true)"
  if [ -z "$arn" ] || [ "$arn" = "None" ]; then
    echo "    Creating IAM role '${name}'..." >&2
    arn="$(aws iam create-role --role-name "$name" \
      --assume-role-policy-document "{
        \"Version\": \"2012-10-17\",
        \"Statement\": [{
          \"Effect\": \"Allow\",
          \"Principal\": {\"Service\": \"${service}\"},
          \"Action\": \"sts:AssumeRole\"
        }]
      }" \
      --tags "Key=knr-ops,Value=management" \
      --query 'Role.Arn' --output text)"
  fi
  local policy
  for policy in "$@"; do
    aws iam attach-role-policy --role-name "$name" --policy-arn "$policy"
  done
  printf '%s\n' "$arn"
}

create_eks_mgmt_cluster() {
  local cluster_name="${MGMT_EKS_CLUSTER_NAME:-knr-ops-mgmt}"
  local region="${MGMT_EKS_REGION:-${AWS_REGION:-eu-north-1}}"
  local k8s_version="${MGMT_EKS_VERSION:-1.33}"
  local node_type="${MGMT_EKS_NODE_INSTANCE_TYPE:-t3.medium}"

  echo ">>> Provisioning EKS management cluster '${cluster_name}' (${region}, k8s ${k8s_version})..."

  # Idempotency: reuse an ACTIVE cluster (EKS creation is slow and billed).
  local status
  status="$(aws eks describe-cluster --name "$cluster_name" --region "$region" \
    --query 'cluster.status' --output text 2>/dev/null || true)"
  if [ "$status" = "ACTIVE" ]; then
    echo ">>> Cluster '${cluster_name}' already ACTIVE, reusing it."
    echo "    Run 'mise -E aws run teardown' first to recreate from scratch."
  else
    if [ -n "$status" ] && [ "$status" != "None" ]; then
      echo "ERROR: cluster '${cluster_name}' exists in transitional state '${status}'." >&2
      echo "       Wait for it to settle or delete it, then re-run bootstrap." >&2
      exit 1
    fi

    # IAM roles: prefer the clusterawsadm CloudFormation stack roles; fall back
    # to dedicated bootstrap roles with AWS-managed policies.
    local cluster_role_arn node_role_arn
    cluster_role_arn="$(aws iam get-role \
      --role-name eks-controlplane.cluster-api-provider-aws.sigs.k8s.io \
      --query 'Role.Arn' --output text 2>/dev/null || true)"
    node_role_arn="$(aws iam get-role \
      --role-name eks-nodegroup.cluster-api-provider-aws.sigs.k8s.io \
      --query 'Role.Arn' --output text 2>/dev/null || true)"
    if [ -z "$cluster_role_arn" ] || [ "$cluster_role_arn" = "None" ]; then
      cluster_role_arn="$(ensure_role knr-ops-mgmt-eks-cluster eks.amazonaws.com \
        arn:aws:iam::aws:policy/AmazonEKSClusterPolicy \
        arn:aws:iam::aws:policy/AmazonEKSVPCResourceController)"
    fi
    if [ -z "$node_role_arn" ] || [ "$node_role_arn" = "None" ]; then
      node_role_arn="$(ensure_role knr-ops-mgmt-eks-node ec2.amazonaws.com \
        arn:aws:iam::aws:policy/AmazonEKSWorkerNodePolicy \
        arn:aws:iam::aws:policy/AmazonEKS_CNI_Policy \
        arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryReadOnly)"
    fi
    echo "    Cluster role: ${cluster_role_arn}"
    echo "    Node role:    ${node_role_arn}"

    # Networking: default VPC public subnets (nodes get public IPs, no NAT/EIP
    # needed). Override with MGMT_EKS_SUBNET_IDS="subnet-aaa,subnet-bbb".
    local subnet_ids="${MGMT_EKS_SUBNET_IDS:-}"
    if [ -z "$subnet_ids" ]; then
      local default_vpc
      default_vpc="$(aws ec2 describe-vpcs --region "$region" \
        --filters Name=isDefault,Values=true \
        --query 'Vpcs[0].VpcId' --output text)"
      if [ -z "$default_vpc" ] || [ "$default_vpc" = "None" ]; then
        echo "ERROR: no default VPC in ${region}; set MGMT_EKS_SUBNET_IDS explicitly." >&2
        exit 1
      fi
      subnet_ids="$(aws ec2 describe-subnets --region "$region" \
        --filters Name=vpc-id,Values="$default_vpc" \
        --query 'Subnets[].SubnetId' --output text | tr '\t' ',')"
    fi
    echo "    Subnets: ${subnet_ids}"

    echo "    Creating EKS control plane (10-15 min)..."
    aws eks create-cluster --region "$region" --name "$cluster_name" \
      --kubernetes-version "$k8s_version" \
      --role-arn "$cluster_role_arn" \
      --resources-vpc-config "subnetIds=${subnet_ids},endpointPublicAccess=true,endpointPrivateAccess=false" \
      --access-config "authenticationMode=API_AND_CONFIG_MAP" \
      --tags "knr-ops=management,profile=aws" >/dev/null
    aws eks wait cluster-active --name "$cluster_name" --region "$region"

    echo "    Creating managed node group 'default' (3-5 min)..."
    # shellcheck disable=SC2086 # --subnets wants a space-separated list
    aws eks create-nodegroup --region "$region" \
      --cluster-name "$cluster_name" \
      --nodegroup-name default \
      --node-role "$node_role_arn" \
      --subnets ${subnet_ids//,/ } \
      --instance-types "$node_type" \
      --ami-type AL2023_x86_64_STANDARD \
      --scaling-config "minSize=2,desiredSize=2,maxSize=3" \
      --tags "knr-ops=management" >/dev/null
    aws eks wait nodegroup-active --region "$region" \
      --cluster-name "$cluster_name" --nodegroup-name default
  fi

  aws eks update-kubeconfig --name "$cluster_name" --region "$region" \
    --alias "$cluster_name" >/dev/null
  kubectl config use-context "$cluster_name"
  echo ">>> Waiting for EKS nodes to be Ready..."
  kubectl wait --for=condition=Ready node --all --timeout=600s
}
```

**Step 3: Verify syntax**

Run: `bash -n bootstrap.sh && echo OK`
Expected: `OK`

**Step 4: Commit**

```bash
git add bootstrap.sh
git commit -m "feat(bootstrap): add EKS management cluster provisioning function"
```

---

## Task 3: Branch Step 1 on MGMT_PLATFORM

**Objective:** Dispatch between kind and eks at Step 1; leave everything after it untouched.

**Files:**
- Modify: `bootstrap.sh` (Step 1 section, lines 129-161)

**Step 1: Wrap the existing kind block.** Replace the section header and kind creation with:

```bash
# ── Step 1: Create the management cluster (kind or EKS) ──────────────────────
if [ "$MGMT_PLATFORM" = eks ]; then
  create_eks_mgmt_cluster
else
echo ">>> Creating kind cluster 'mgmt'..."
...existing kind creation block unchanged (lines 130-156)...
fi
```

The existing kind block keeps its body byte-identical, including the `KIND_REGISTRY_PATCH` local-host logic and the `kind create cluster` heredoc. Note the local-host profile never reaches the eks branch (Task 1's preflight rejects it).

**Step 2: Branch the context wait.** The existing lines 158-161 (`kubectl config use-context kind-mgmt` + `kubectl wait`) move inside the else branch:

```bash
else
  ...kind create...
  echo ">>> Waiting for cluster node to be ready..."
  kubectl config use-context kind-mgmt
  kubectl wait --for=condition=Ready node --all --timeout=120s
fi
```

(`create_eks_mgmt_cluster` already switches context and waits for nodes.)

**Step 3: Update the completion banner** (aws branch, around line 407) to reflect the platform:

```bash
if [ "$PROFILE" = aws ]; then
  echo ">>> Bootstrap complete! Flux is now reconciling from ${GIT_REPO_URL}"
  echo ">>> Management cluster: ${MGMT_PLATFORM} ($([ "$MGMT_PLATFORM" = eks ] && echo "${MGMT_EKS_CLUSTER_NAME:-knr-ops-mgmt}" || echo kind-mgmt))"
  echo ">>> Watch progress with: flux get kustomizations --watch"
```

**Step 4: Verify syntax and the kind path is untouched**

```bash
bash -n bootstrap.sh && echo OK
git diff main -- bootstrap.sh | grep -c '^-.*kind create cluster'   # expect 0 (kind block not modified, only moved)
```

Expected: `OK`, and the kind heredoc appears only as context (no semantic change).

**Step 5: Commit**

```bash
git add bootstrap.sh
git commit -m "feat(bootstrap): dispatch management cluster creation on MGMT_PLATFORM"
```

---

## Task 4: Teardown support for the EKS management cluster

**Objective:** `teardown.sh` deletes the eks nodegroup + cluster when `MGMT_PLATFORM=eks`, instead of the kind cluster.

**Files:**
- Modify: `teardown.sh` (Step 9, lines 966-974; header comment lines 5-14)

**Step 1: Replace Step 9.** Complete code:

```bash
# ── Step 9: Delete the management cluster (kind or EKS) ─────────────────────
# Cluster-aware: only deletes once CLUSTERS_CONFIRMED_GONE=1 (or when
# FORCE_KIND_DELETE=1 for kind / FORCE_MGMT_DELETE=1 for eks). Disarm the EXIT
# trap first so it doesn't double-fire.
trap - EXIT
if [ "${AWS_ONLY:-0}" != "1" ]; then
  if [ "${MGMT_PLATFORM:-kind}" = eks ]; then
    mgmt_cluster="${MGMT_EKS_CLUSTER_NAME:-knr-ops-mgmt}"
    mgmt_region="${MGMT_EKS_REGION:-${AWS_REGION:-eu-north-1}}"
    info "Deleting EKS management nodegroup '${mgmt_cluster}/default'..."
    if aws eks describe-nodegroup --cluster-name "$mgmt_cluster" \
        --nodegroup-name default --region "$mgmt_region" >/dev/null 2>&1; then
      aws eks delete-nodegroup --cluster-name "$mgmt_cluster" \
        --nodegroup-name default --region "$mgmt_region" >/dev/null
      aws eks wait nodegroup-deleted --cluster-name "$mgmt_cluster" \
        --nodegroup-name default --region "$mgmt_region"
    fi
    info "Deleting EKS management cluster '${mgmt_cluster}'..."
    if aws eks describe-cluster --name "$mgmt_cluster" \
        --region "$mgmt_region" >/dev/null 2>&1; then
      aws eks delete-cluster --name "$mgmt_cluster" --region "$mgmt_region" >/dev/null
      aws eks wait cluster-deleted --name "$mgmt_cluster" --region "$mgmt_region"
    fi
    kubectl config delete-context "$mgmt_cluster" >/dev/null 2>&1 || true
    success "EKS management cluster deleted (or was already absent)"
  else
    _delete_kind_if_safe
  fi
else
  warn "AWS_ONLY mode – skipping management cluster deletion"
fi
```

Note: teardown.sh's existing steps 1-8 (suspend Flux, delete CAPI workload clusters, AWS orphan cleanup, providers, helm releases, secrets) run against the current kubectl context and need no change; they work identically against the EKS mgmt cluster. The fallback IAM roles from Task 2 (`knr-ops-mgmt-eks-cluster`, `knr-ops-mgmt-eks-node`) are left in place, matching the existing convention that the clusterawsadm stack is kept. Deleting them is a one-liner documented in operations.md.

**Step 2: Update the header comment** (line 14): `#   9. Delete the kind management cluster` becomes `#   9. Delete the management cluster (kind, or EKS when MGMT_PLATFORM=eks)`.

**Step 3: Verify syntax**

Run: `bash -n teardown.sh && echo OK`
Expected: `OK`

**Step 4: Commit**

```bash
git add teardown.sh
git commit -m "feat(teardown): delete EKS management cluster when MGMT_PLATFORM=eks"
```

---

## Task 5: Configuration template and docs

**Objective:** Document the new env vars and the eks bootstrap flow.

**Files:**
- Modify: `.env.example`
- Modify: `docs/operations.md` (Bootstrap section, lines 69-89; Prerequisites; Teardown)
- Modify: `AGENTS.md` (bootstrap description and Common tasks)

**Step 1: Add to `.env.example`** (after the AWS credentials section):

```bash
# ── Management cluster platform (AWS profile, optional) ─────────────────────
# kind (default): local kind cluster, free, needs Docker/Podman.
# eks: real EKS cluster in the management account (~$0.10/hr control plane
# plus 2x t3.medium nodes). Requires: mise -E aws run bootstrap, valid AWS
# credentials, and the clusterawsadm stack (mise -E aws run aws-bootstrap).
# MGMT_PLATFORM="eks"
# MGMT_EKS_CLUSTER_NAME="knr-ops-mgmt"
# MGMT_EKS_REGION="eu-north-1"
# MGMT_EKS_VERSION="1.33"
# MGMT_EKS_NODE_INSTANCE_TYPE="t3.medium"
# MGMT_EKS_SUBNET_IDS="subnet-aaa,subnet-bbb"   # default: default VPC subnets
```

**Step 2: Update `docs/operations.md`.** In the Bootstrap section, after the existing numbered list, add:

```markdown
With `MGMT_PLATFORM=eks` (AWS profile only), Step 1 provisions a real EKS
cluster instead of kind:

```sh
mise -E aws run aws-bootstrap   # once per account (clusterawsadm IAM stack)
MGMT_PLATFORM=eks mise -E aws run bootstrap
```

The eks path requires `aws` and `clusterawsadm` (the `aws` mise env layer) and
valid management-account credentials, but no local container engine. It
creates the control plane and a 2-node managed node group in the default
VPC's public subnets (no NAT gateways or Elastic IPs), then runs the same
Flux handoff as the kind path. EKS provisioning adds roughly 15-20 minutes
before Flux starts reconciling. The management cluster coexists with the
CAPA-managed workload clusters: those live in their own CAPA-created VPCs.
Cost: about $0.10/hr for the control plane plus the node EC2 instances;
`MGMT_PLATFORM=eks mise -E aws run teardown` removes both (the
clusterawsadm stack and any `knr-ops-mgmt-eks-*` fallback IAM roles are left
in place, matching the existing teardown convention).
```

Update the Prerequisites section: under "AWS profile only", add that `MGMT_PLATFORM=eks` additionally needs `eks:CreateCluster`/`CreateNodegroup`/`Describe*` and, if the clusterawsadm eks roles are absent, `iam:CreateRole`/`AttachRolePolicy` for the bootstrap fallback roles. Update the Teardown section to mention the eks branch.

**Step 3: Update `AGENTS.md`.** In the repo description, change "A local kind cluster bootstraps Flux" to mention the optional EKS management plane, and add to Common tasks:

```markdown
MGMT_PLATFORM=eks mise -E aws run bootstrap   # EKS management cluster variant (AWS profile)
```

**Step 4: Verify**

Run: `mise run validate`
Expected: `bash -n` passes for both scripts, all kustomize builds pass, exit 0.

**Step 5: Commit**

```bash
git add .env.example docs/operations.md AGENTS.md
git commit -m "docs: document MGMT_PLATFORM=eks management cluster path"
```

---

## Task 6: Live end-to-end run (acceptance test)

**Objective:** Prove the eks path against the real management account, including the full GitOps handoff.

**Prerequisites:** `.env` + `age.agekey` copied into the worktree (Task 0), AWS credentials for the management account exported, `mise -E aws install` done.

**Step 1: Preflight IAM verification** (from Task 2, Step 1) and version check:

```bash
aws eks describe-cluster-versions --region eu-north-1 \
  --query 'clusterVersions[?status==`STANDARD_SUPPORT`].clusterVersion' --output text
```

Confirm `MGMT_EKS_VERSION` (default 1.33) is listed; export an override if not.

**Step 2: Run bootstrap**

```bash
MGMT_PLATFORM=eks mise -E aws run bootstrap
```

Expected output sequence: preflight passes with no container-engine message; "Provisioning EKS management cluster"; control plane wait (~10-15 min); nodegroup wait (~3-5 min); nodes Ready; Flux Operator + secrets + FluxInstance install; "Bootstrap complete! ... Management cluster: eks (knr-ops-mgmt)". Second run must print "already ACTIVE, reusing it" and proceed straight to Flux install (idempotency check).

**Step 3: Verify the management plane**

```bash
kubectl config current-context                 # knr-ops-mgmt
kubectl get nodes                              # 2x t3.medium, Ready
kubectl get kustomizations -n flux-system      # progressing toward Ready
aws eks describe-cluster --name knr-ops-mgmt --region eu-north-1 --query 'cluster.status'
```

**Step 4: Verify the GitOps chain reaches the same end state as the kind path**

```bash
kubectl get clusters.cluster.x-k8s.io -A       # workload clusters Provisioning/Provisioned
kubectl get pods -n capa-system                # CAPA controller Running
kubectl get roles.iam.services.k8s.aws -n ack-system
```

Expected: identical readiness to a kind-profile run (EKS workload clusters in eu-north-1/eu-west-1 reach Provisioned, per docs/operations.md "Verifying the full chain").

**Step 5: Verify teardown**

```bash
MGMT_PLATFORM=eks mise -E aws run teardown
aws eks describe-cluster --name knr-ops-mgmt --region eu-north-1  # expect ResourceNotFoundException
```

**Step 6: Commit** (nothing to commit if no fixes were needed; otherwise fix forward in small commits)

---

## Task 7: Open the PR

**Step 1:** `mise run validate` one final time in the worktree.

**Step 2:** Push the branch and open a PR (do not merge; CI and konflate rendered-diff review gate it):

```bash
git push -u origin feat/eks-mgmt-platform
gh pr create --title "feat: EKS management cluster option for the AWS bootstrap path" \
  --body "Adds MGMT_PLATFORM=eks to bootstrap.sh: provisions a knr-ops-mgmt EKS cluster with clusterawsadm/aws CLI instead of kind for the aws profile. kind remains the default; local-host profile is untouched. Validated end to end: EKS mgmt cluster provisions, Flux hands off, CAPA workload clusters reconcile, teardown removes the EKS cluster."
```

**Step 3: Cleanup after merge**

```bash
cd ~/Code/knr-ops
git worktree remove ../knr-ops-eks-mgmt
```

---

## Files likely to change

| File | Change |
|---|---|
| `bootstrap.sh` | MGMT_PLATFORM preflight, conditional engine check, `ensure_role` + `create_eks_mgmt_cluster`, Step 1 dispatch, banner |
| `teardown.sh` | Step 9 eks branch, header comment |
| `.env.example` | MGMT_PLATFORM / MGMT_EKS_* vars |
| `docs/operations.md` | Bootstrap + Prerequisites + Teardown sections |
| `AGENTS.md` | Architecture sentence + Common tasks |

No changes under `mgmt/aws/` or `workload/`: the whole point is that the GitOps payload is platform-agnostic.

## Tests / validation

- `mise run validate` (bash -n + kustomize builds) after every script change; mirrors CI.
- Negative preflight checks (Task 1, Step 4).
- Live acceptance: Task 6 (bootstrap, idempotent re-run, GitOps chain parity, teardown).

## Risks, tradeoffs, open questions

- **Cost.** kind is free; the eks path bills ~$0.10/hr control plane + 2 nodes (~$4-5/day). Default stays kind; docs flag the cost. The idempotent reuse means a forgotten cluster keeps billing; teardown is the fix.
- **clusterawsadm role assumptions.** The eks-controlplane/eks-nodegroup role names and attached managed policies are assumed from clusterawsadm conventions for v2.13.0 and verified in Task 2 Step 1; the fallback roles make the script correct either way. If the stack roles exist but lack a required managed policy, prefer the fallback roles (flip the lookup order) rather than mutating stack-owned roles.
- **Default VPC.** Keeps the reference implementation minimal and avoids NAT/EIP quotas, but is not production-shaped. `MGMT_EKS_SUBNET_IDS` is the documented escape hatch; a CAPA-managed mgmt VPC is out of scope.
- **K8s version drift.** EKS supported versions move; `MGMT_EKS_VERSION` is explicit and verified in Task 6 rather than resolved dynamically.
- **Open question (follow-up, not this PR): IRSA.** Once the management plane runs on EKS, the static `aws-credentials` secret + `AWSClusterControllerIdentity` can be replaced with IRSA per the CAPA cross-account guide (kubernetes-sigs/cluster-api-provider-aws PR #6188, `docs/book/src/topics/eks/cross-account-role-assumption.md`): associate the cluster's OIDC provider, trust `capa-system`/`capa-eks-control-plane-system` service accounts, and move to `AWSClusterRoleIdentity` for workload accounts. That conversion also gives this repo a real environment to validate the upstream guide end to end. Keeping it out of scope here: the eks path must first prove parity with kind using the existing static-credential flow.
