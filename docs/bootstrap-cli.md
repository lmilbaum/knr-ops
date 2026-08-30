# The bootstrap CLI (knr-bootstrap)

The imperative part of knr-ops — the one-time bootstrap, the CAPI pivot into
the self-managed management cluster, and (once its port lands) the teardown —
lives in a single Rust binary, `knr-bootstrap`, in [`bootstrap-rs/`](../bootstrap-rs/).
Everything the binary runs is a one-time imperative step; after it finishes,
Flux owns the world and the binary is not needed again until teardown.

The binary is a behavioral port of the shell scripts it replaces
(`bootstrap.sh` + `pivot.sh`; `teardown.sh` is being ported under
[#100](https://github.com/polarsquad/knr-ops/issues/100)). It preserves the
scripts' step order, `>>>` progress messages, error text, and environment
interface, with two deliberate upgrades over the scripts:

- **Reruns are safe by default.** An existing healthy `mgmt` kind cluster is
  reused and every step is idempotent, so a partially failed bootstrap or
  pivot can be resumed by rerunning. Pass `--recreate` to delete and rebuild
  the kind cluster instead.
- **No shell, no quoting.** Tool invocations pass typed argv entries, secrets
  travel through stdin and the environment (never argv), and the HTTP checks
  (GitHub branch availability, registry readiness) run through reqwest with
  explicit timeouts instead of `curl`.

## Build

```sh
cd bootstrap-rs
cargo build            # debug; add --release for an optimized binary
./target/debug/knr-bootstrap --help
```

CI builds, lints (fmt, clippy `-D warnings`), and tests the crate on every
change under `bootstrap-rs/` (`.github/workflows/bootstrap-rs.yml`). The
toolchain is pinned in `bootstrap-rs/rust-toolchain.toml`; dependencies are
locked in `Cargo.lock`.

## Interface

The CLI surface is the scripts' surface: a positional profile, `--recreate`,
and the environment. The expanded flag interface is intentionally deferred
(follow the [#92](https://github.com/polarsquad/knr-ops/issues/92) and
[#95](https://github.com/polarsquad/knr-ops/issues/95) issue threads).

```sh
knr-bootstrap [PROFILE] [--recreate]
```

- `PROFILE`: `aws` (default) or `local-host`. A non-empty `KNR_OPS_PROFILE`
  takes precedence over the positional argument, matching `bootstrap.sh`.
- `--recreate`: delete and recreate an existing `mgmt` kind cluster instead of
  reusing it. Required when the profile or the kind/registry configuration
  changed since the cluster was created; the reuse path does not detect drift.

Every `${VAR:-default}` knob the scripts read is read the same way: unset and
empty both fall back to the default. Where a knob configures both the binary
and a delegated `mise` child (for example `oci-push`), the resolved value is
forwarded to the child explicitly.

| Knob | Default | Used by |
|---|---|---|
| `KNR_OPS_PROFILE` | positional arg, then `aws` | profile selection |
| `REGISTRY_PORT` | `5001` | local-host: host port of the local OCI registry |
| `REGISTRY_READY_RETRIES` | `120` | local-host: registry readiness poll attempts |
| `LOCAL_RECONCILE_TIMEOUT` | `15m` | local-host: management/workload reconciliation waits |
| `CONTAINER_ENGINE` | auto (`docker`, else `podman`) | kind/container engine selection |
| `GIT_REPO_URL` | — (required, aws) | Flux Git source for the management cluster |
| `GITHUB_TOKEN` | — (required, aws) | PAT with read access to the repo |
| `GITHUB_USER` | `git` | Git clone user for the Flux secret |
| `AGE_KEY_FILE` | `age.agekey` | SOPS age private key loaded into the `sops-age` secret |
| `AGE_PUBLIC_KEY` | derived from `AGE_KEY_FILE` | `create`-mode public key override |
| `OCI_REPOSITORY` / `OCI_TAG` | `knr-ops` / `latest` | local-host: OCI artifact name |
| `BOOTSTRAP_PIVOT` | `1` | `0` skips the pivot; kind stays the management cluster |
| `MGMT_KUBECONFIG` | `~/.kube/knr-ops-mgmt.yaml` | exported management kubeconfig |
| `MGMT_READY_TIMEOUT` | `40m` aws / `15m` local-host | management cluster provisioning wait |
| `MGMT_POLL_INTERVAL` | `10` (seconds) | management cluster provisioning poll |
| `BOOTSTRAP_KUBECONTEXT` | `kind-mgmt` | context the pivot runs from |
| `PIVOT_SKIP_DELETE` | `0` | `1` keeps the kind bootstrap cluster for inspection |

## What a run does

1. **Preflight**: profile validation, required tools (`kind`, `helm`,
   `kubectl`, `clusterctl`, `mise` on both profiles; `flux` and `curl`
   additionally on local-host), container engine detection, and (aws) the
   GitHub repo/branch/token checks and age key file validation.
2. **Bootstrap** the `mgmt` kind cluster: local registry (local-host), Flux
   Operator install, `flux-github-pat` + `sops-age` secrets (aws) or the
   initial OCI artifact publish (local-host), `FluxInstance` sync handoff,
   and the reconciliation watch.
3. **Pivot** (default; `BOOTSTRAP_PIVOT=0` opts out): wait for the
   self-managed management cluster (provisioned by CAPI from
   `mgmt/<env>/clusters/management/`), export its kubeconfig, imperatively
   install cert-manager + the CAPI operator + provider CRs on the target at
   the same versions as the Git HelmReleases, suspend Flux in kind,
   `clusterctl move`, unpause the moved Clusters, seed Flux on the target,
   and delete the kind bootstrap cluster.

If any phase fails, fix the cause and rerun: the bootstrap steps are
rerun-safe, an existing healthy kind cluster is reused (a cluster whose
kubeconfig context is gone can be recovered with
`kind export kubeconfig --name mgmt` before rerunning), and
`clusterctl move` is re-runnable. Recovery details and the never-delete-moved-objects
warning are in [Operations: pivot recovery](./operations.md#pivot-recovery).

## Parity status and script retirement

The shell scripts stay in the repository until the binary has completed full
real runs per profile; they are removed in a follow-up once both parity gates
pass. `mise run bootstrap` and `mise run pivot` still invoke the scripts —
run the binary from `bootstrap-rs/` until the switchover. Chart versions the
binary installs imperatively are pinned as Renovate-annotated constants in
`bootstrap-rs/src/main.rs` and grouped with their manifest counterparts (see
[Dependencies](./dependencies.md)).

## Roadmap

- [#100](https://github.com/polarsquad/knr-ops/issues/100): port `teardown.sh`
  into the CLI (post-pivot teardown semantics resolved as part of it).
- [#98](https://github.com/polasquad/knr-ops/issues/98): externalize
  knr-ops-specific constants (cluster names, sync paths, chart pins) from
  compiled-in values into a `bootstrap.toml` owned by the repository, so the
  binary becomes a generic bootstrap engine with knr-ops as its first
  consumer.
