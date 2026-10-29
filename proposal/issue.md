# Proposal: an alpha Sandbox building block (isolated microVMs addressed as actors)

*Draft issue for dapr/dapr. Local draft, not filed.*

## Summary

Add an alpha **Sandbox** building block. Apps get an API that runs commands in an isolated
environment, such as a microVM, per sandbox ID.

- **The sandbox ID.** It is a Dapr actor ID of an internal actor type that daprd hosts itself,
  as it hosts workflow actors.
- **The VM.** It comes from a driver: a pluggable component in a new `sandbox` category, behind
  a gRPC contract that does not depend on any one VMM.

## Why

**The need.** AI agents, function execution and notebooks need one isolated environment per
session. It must start fast and behave the same on a laptop and on Kubernetes.

**Why actors fit.** Dapr's virtual actors already give what such a system needs:
- one live instance per ID;
- routing to the owner pod;
- calls that run one at a time;
- idle cleanup.

## API sketch (alpha, HTTP and gRPC)

- `POST /v1.0-alpha1/sandboxes/{component}/{id}/exec` runs a command. The first call for an
  unknown ID activates it, from a warm pool, a snapshot restore or a boot.
- `POST /v1.0-alpha1/sandboxes/{component}` creates a sandbox with an ID chosen by the sidecar.
- `POST .../{id}/sessions`, `POST .../{id}/sessions/{sessionId}` and
  `DELETE .../{id}/sessions/{sessionId}` handle long-lived processes.
- `DELETE /v1.0-alpha1/sandboxes/{component}/{id}` releases a sandbox.
- A target app ID reaches the sandboxes of a dedicated host app: the `dapr-app-id` header or
  `appId` over HTTP, `app_id` over gRPC. On Kubernetes, only that app's pods run untrusted code.
- An activation token makes a lost sandbox visible (410) rather than silently recreated.

## Proof of concept

**What was built**
- `dapr/dapr` v1.19.0 plus 27 patches: 15 as measured, then 12 from a fix wave after the
  final review (error paths and tests, not re-measured).
- A microVM driver in Rust as a pluggable component: microsandbox 0.7.11, on KVM or
  Hypervisor.framework.

**What it measured.** On kind on a bare-metal Linux host with KVM, cross-pod, measured at the
caller's sidecar, a new sandbox ID served from the warm pool ran `echo ok` at:
- **p99 16.44 ms sequentially.** This covers the 482 of 500 requests the pool served; the other
  18 needed a restore.
- **p99 11.36 ms at 20 new sandboxes per second per host pod.** All from the pool, with 0 errors.

Routing and activation took under 1 ms of the 7.18 ms median.

**Where it gets slower**
- At 40 new sandboxes per second per pod, 19.7% fell back to a restore. The warm-pool refill,
  not Dapr, is the next bottleneck.
- kind on macOS, under nested virtualization, failed the same gate because of the nested stack.

## Questions for maintainers

1. **Where it lives.** Should this live in the runtime, like Workflows, or as an app on Dapr
   actors?
2. **The ACL change.** The PoC changes the internal-actor ACL so that other apps in a namespace
   can call a host's `dapr.internal.<ns>.<appID>.sandbox.*` types under mTLS. Is that
   acceptable, and what owner binding should come with it?
3. **Drivers.** Where should they live: `components-contrib`, or separate repositories?
4. **Privileges.** On Kubernetes the driver container is privileged for `/dev/kvm` until there
   is a KVM device plugin route. Is that acceptable for an alpha?

## Links

- **Proposal:** a dapr/proposals PR (not yet opened). Local draft:
  `proposal/20261028-BRCS-sandbox-building-block.md`.
- **Related:** dapr/dapr#9128 (multi-cluster). Cross-cluster sandboxes are future work.
