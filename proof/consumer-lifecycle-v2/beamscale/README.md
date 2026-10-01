# BeamScale bare-process fleet

This backend runs BeamScale on a small NixOS fleet without Kubernetes, containers, or mandatory microVMs.

## Responsibilities

- Cloud IaC owns machines, networking, disks, IAM/service identities, DNS and load balancers.
- NixOS owns the complete host configuration.
- Colmena owns stateless multi-host rollout/activation.
- `ores-proc-isolation-cli` owns the mandatory OS process sandbox for tenant-influenced workloads.
- `bmscl-supervisor` owns trusted BEAM supervision, generation selection, hot rollout/drain and capability mediation.
- `bmscl-lifecycle-agent` is the separate host lifecycle process. The shared executable remains **observe/preflight-only** in this module; process effects stay disabled until the active lifecycle composition and live host proof are explicitly enabled and certified.

The intended production fleet is 3-9 hosts distributed across at least two regions/failure domains. Region-local quorum/state services must not depend on WAN-synchronous consensus unless explicitly designed for it.

## Idle process lifecycle

See `process-lifecycle.md` for the target runtime-state, fencing, placement-epoch, wake-on-demand, shard-reassignment, and Unix-socket trust contracts.

`nixos/bmscl-lifecycle-agent.nix` defines a separate root host service and a dedicated `beamscale-workloads.slice`. The module is intentionally standalone until the fleet's Colmena wrapper imports an exact shared lifecycle-agent package. Lifecycle config v2 supplies explicit deployment environment and region identity, derives the fixed BeamScale product and host-control socket paths from `product=beamscale`, uses `LEASE_PROVIDER=cloudflare-do`, and sets `EFFECTS=observe`.

Observe mode deliberately carries no checkpoint root, lease endpoint, bearer credential, `LoadCredential`, cgroup-write authority, CRIU/ptrace/mount/DAC-bypass capability, or IP network address family. The service sees the cgroup hierarchy and trusted host socket directory read-only while effects remain disabled.

The paired `bmscl-supervisor` cooperative lifecycle socket is fixed at `/run/beamscale-lifecycle/product/control.sock`. The supervisor-owned setgid `product/` directory and `beamscale-lifecycle-control` group provide only cooperative socket reachability. The root-owned trusted host-control socket is separately fixed at `/run/beamscale-lifecycle/host/control.sock`, and `host/` is mode `0700 root:root`.

The two sockets deliberately do **not** share a writable parent. Directory write permission is enough to unlink or replace a Unix socket entry even if the socket inode itself is root-owned. The supervisor service therefore gets `ReadWritePaths` only for `/run/beamscale-lifecycle/product` and `InaccessiblePaths` for `/run/beamscale-lifecycle/host`. The host-control service must derive scheduler/process identity from trusted host state and owns the latter directory independently.

The host lifecycle daemon remains primary `uid=0,gid=0`, uses the control group only as a supplementary group for the cooperative product socket, and the supervisor authenticates the kernel `SO_PEERCRED` value before parsing cooperative requests. A sibling/runtime process that can reach the product socket inode still is not authorized to quiesce or resume the runtime.

Enabling the lifecycle module requires the exact existing supervisor systemd service name via `productService`. The module injects `BMSCL_LIFECYCLE_SOCKET=/run/beamscale-lifecycle/product/control.sock` into that service, requires and starts after its `.service` unit, runs the shared-agent `preflight`, and then runs `probe-product` before entering the observe loop. A missing, disabled, unreachable, or protocol-incompatible product bridge therefore fails lifecycle-agent startup closed instead of degrading into a silent unavailable poll loop.

Lifecycle config v2 derives both fixed socket paths; callers do not provide separate product- or host-socket path authority. Observe-only mode still requires the trusted socket parent to exist. Active freeze/thaw must remain disabled in BeamScale infra until the host/control-plane v2 server, distributed lease/fence authority, exact identity composition, durable transition recovery, and funded live cgroup proof are all wired together on an exact reviewed head.

The host deployment must provide the exact non-tenant OS user that runs `bmscl-supervisor` as `controlSocketOwner`; this module does not guess a service account. That owner can create the cooperative socket inside the private setgid product directory and the root lifecycle daemon can reach it without `CAP_DAC_OVERRIDE`. It receives no write or traversal authority into the trusted host-control directory.

The first future mutation tranche is cgroup-v2 freeze/thaw for warm-idle runtimes. Freeze/thaw reclaims scheduling CPU but does not claim to release resident memory. Real RAM release requires either deterministic terminate/reconstruct from immutable artifacts plus external durable state, or a separately reviewed typed hibernate/checkpoint helper for an explicitly compatible runtime class. Do not hide CRIU/restore authority behind a generic boolean on the main lifecycle daemon.

The distributed lifecycle design uses `ORESoftware/ores-locks-and-leases`, with Cloudflare Durable Objects as the initial authority and Fiducia as a later backend swap. The canonical lock namespace is `beamscale/runtime-lifecycle/<environment>/<region>/<runtime-id>`; node/controller identity is deliberately excluded. BeamScale-native locks must never be required to boot, wake or repair BeamScale itself. The product Unix socket remains cooperative admission/quiesce evidence only; trusted queue/demand and post-wake admission must come from host/control-plane authority before effects may be enabled.

## Isolation policy

`.ores-proc-isolation.yaml` is checked in beside this document. Validate it with the matching pinned `ores-proc-isolation-cli` before deployment. Production automation must record the policy file digest and isolation-tool version/commit used for admission.

The current policy uses only capabilities supported by the isolation CLI v1 schema. Independent host-level cgroup/systemd/NixOS controls should add memory/PID/CPU ceilings; do not invent unsupported YAML fields and assume they are enforced.

Tenant-worker launches must enable the isolation CLI's reviewed BeamScale tripwire mode. The policy exposes only the exact inert-wrapper directory and Unix socket, and replaces tenant `PATH` with the wrapper directory:

```bash
ores-proc-isolation doctor \
  --beamscale-honeypot \
  --config deploy/baremetal/.ores-proc-isolation.yaml

ores-proc-isolation run bmscl-worker \
  --beamscale-honeypot \
  --config deploy/baremetal/.ores-proc-isolation.yaml
```

Do not broaden these mounts to `/opt`, `/run`, `/bin`, `/sbin`, `/usr/bin`, or `/usr/sbin`. The isolation CLI independently rejects those broad paths in BeamScale honeypot mode.

A customer worker must never be launched if:

- the isolation policy cannot be parsed or validated;
- the requested process/profile is missing or ambiguous;
- the expected policy/tool digest differs from the installed copy;
- the host cannot enforce the required sandbox backend;
- `--beamscale-honeypot` is absent for a tenant-worker launch or the exact tripwire directory/socket fails the isolation CLI doctor checks;
- the executable/artifact digest is not admitted;
- a requested capability is broader than the release policy.

## Density rule

Prefer one trusted BEAM runtime to multiplex many lightweight admitted actors/processes where policy permits. Do not create one operating-system process per request merely for symmetry with other lambda platforms; the BeamScale advantage is amortizing the BEAM runtime while preserving per-invocation actor supervision and metering.

OS process boundaries are used where they materially improve tenant/runtime containment. Erlang actor isolation provides additional fault/resource structure inside that boundary, not a substitute for it.
