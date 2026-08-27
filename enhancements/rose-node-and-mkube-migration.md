# The rose node: rustkube replaces mkube

**Filed:** 2026-08-27, out of the #26/#18/#14 incident post-mortem on mkube.
**Decision (operator):** rustkube + fastetcd + stormboot become the stack;
mkube retires. PXE/BMH/DNS-orchestration and the other auxiliaries become
separate containers (Rust, rustkube-API clients).

## What already exists — the gap is smaller than it looks

- **Control plane: done.** rustkube v0.7.35, phases 0–3 complete, proven
  against kubectl/helm/client-go/Cilium, external fastetcd over the etcd v3
  wire. mkube's kube-apiserver-shim TODO evaporates — rustkube IS the
  apiserver.
- **Device access: stormboot.** All veth/bridge/interface work lives in
  stormboot (interface-made-true-before-container, veth healing, native API
  8728, validated on rose1). On rose it is host networking from the
  kubelet's point of view — no CNI.
- **Storage: integrated.** rustkube already speaks stormblock and
  sbregistry; stormblockmk is deployed; goldens/clones are the image path.
- **DNS: external.** microdns carries the K8s DNS source already.

## What is actually missing

1. **The rose node agent** — thin kubelet: node registration, pod watch,
   probe/status reporting, mapped onto stormboot's ContainerHost trait
   (find/attach/start; image materialization is sbregistry+stormblock's
   job, never the host's). Reuse rustkube-node's kubelet skeleton; the
   CRI/youki/VM runtimes are irrelevant on rose.
2. **ARM64 / MikroTik build verification** (rustkube Phase 4 checkbox).
3. **fastetcd soak on rose-class hardware** — durability under power loss,
   crash recovery, watch fan-out (mkube TODO #14's checklist).
4. **Aux moves** (each its own small container, portable one at a time
   while mkube still runs): BMH/IPMI, PXE, DNS orchestration, gitbackup,
   console, DHCP CRDs. Needed for rustkube compatibility regardless.

## Conformance bar for the node agent — the 2026-08-27 invariants

Paid for in a production outage; each becomes a test before the agent
touches a live device:

- **Ownership by construction**: the agent holds no RouterOS credentials;
  every device mutation goes through stormboot's trait. (The controller
  that could delete bridge ports, did.)
- **Never destroy without a verified replacement** (image sealed golden,
  entrypoint present) — stopped-but-recreatable beats gone.
- **Bounded loops**: every retry has a cap; an unbounded reconcile loop is
  net-destructive on the device.
- **Veths are durable container identities**: create-only, sticky IP/MAC,
  30-day orphan grace, infra veths untouchable (enforced in stormboot).
- **No destructive convergence on unprovable state**: store unreadable or
  registry unhealthy ⇒ read-only cycle.
- **Device parameter contracts are tested against real RouterOS versions**
  (`envlists` not `envlist`, `key` not `name`, env lists bake at container
  creation — all silent failures found 2026-08-27).

## Migration order (strangler, no big-bang)

1. fastetcd soak on rose-class hardware; rustkube control plane up beside
   mkube (its own containers, its own goldens).
2. Aux controllers peel off one at a time onto the rustkube API.
3. Node agent passes the conformance suite against a fake host, then a
   probe container on rose1, then adopts pods pod-by-pod (same veth
   identities — the sticky-identity model makes adoption an attach, not a
   migration).
4. mkube exits; stormboot supervises the floor; a controller update is a
   clone swap (2.2 s measured), not a 4-minute tarball dance.
