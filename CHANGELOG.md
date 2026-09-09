# Changelog

## [Unreleased]

<!-- New unreleased changes go here -->

## [v0.9.1] — 2026-09-09

### Fixed
- **apiserver:** **security** — bootstrap now *removes*
  `clusterrolebindings/system:anonymous-admin` when the dev grant is not in
  effect, instead of merely not creating it (#60). The binding is a stored
  object and the authorizer reads stored bindings, so an apiserver brought up
  once with `--dev-anonymous-admin true` and later restarted without it kept
  answering every anonymous request as cluster-admin — the removed flag read
  as though it had done something. A warning names the binding when it is
  removed; a failure to remove it is logged as an error that says anonymous
  still has cluster-admin.

## [v0.9.0] — 2026-09-09

### Added
- **feat(apiserver):** `authorization.k8s.io/v1` — `SelfSubjectAccessReview`
  ("may I do X on Y?") and `SelfSubjectRulesReview` ("what may I do in
  namespace N?") (#59). Write-only virtual resources answered by the same RBAC
  engine that decides the real request, so a review cannot drift from what the
  request path would do; the access review reports which binding granted it.
  Without them a console had to infer a viewer's access by probing — one list
  per namespace, as them, reading the status code — which is 200 requests per
  viewer on a 200-namespace cluster and answers a question next to the one
  asked.
- **feat(apiserver):** every authenticated identity now carries the
  `system:authenticated` group, and `system:basic-user` is bootstrapped and
  bound to it, so any authenticated user may ask about themselves — as
  upstream does. Bindings written against `system:authenticated` previously
  matched nobody.

### Fixed
- **apiserver:** **security** — an RBAC rule that names no `apiGroups` or
  no `resources` no longer grants every resource request. The bootstrap
  `system:discovery` ClusterRole is a single `nonResourceURLs` rule and
  `system:anonymous` is bound to it, so with `--anonymous-auth=true` anyone
  who could reach the port could GET any resource in the cluster, secrets
  included — which is exactly what the #16 hardening in v0.7.17 was meant to
  prevent. Absent and empty now mean "matches nothing", as upstream does.
  `cluster-admin` is written with explicit `*` entries and is unaffected.
- **apiserver:** `/apis/{group}/{version}/namespaces/{ns}/{resource}` is no
  longer parsed as a subresource of a namespace — it has the same six-segment
  shape as `{resource}/{name}/{sub}` and the generic arm was tried first, so
  every namespaced request in a non-core group asked the authorizer about
  `namespaces/{resource}`, which nothing grants. This is what refused
  cilium-operator its leader-election lease.

## [v0.8.1] — 2026-09-09

### Fixed
- **apiserver:** `pods/log` and `pods/exec` accept an init or ephemeral
  container name, not only one from `spec.containers` (#55, #54). A failed
  init container's log is the only thing that says why it failed, and a
  sidecar — an init container with `restartPolicy: Always` since K8s 1.28 —
  is not in `spec.containers` at all; both were refused with "container X is
  not valid for pod". The error now lists every valid name. The default when
  none is given is unchanged.

## [v0.8.0] — 2026-09-08

### 2026-09-08 — certificates
- **feat(apiserver):** the serving certificate reloads **without a restart**
  (#20 phase 1). rustls asks a resolver for the certificate on every
  handshake, so the apiserver now watches `--tls-cert-file` /
  `--tls-private-key-file` (by content, every 30s) and swaps what the resolver
  hands out. This was the blocker the issue named: the components read their
  certificate once at startup, so a renewed cert on disk did nothing until the
  process restarted — which meant the only way to rotate a ten-year PKI was to
  redeploy the control plane, which is why nobody would. A cert file that is
  unreadable or half-written is kept, not applied: the running certificate is
  known good, and a parse failure would take TLS down at exactly the moment
  someone is touching the PKI. `apiserver_certificate_expiration_seconds` is
  refreshed on reload.
- **feat:** `deploy/renew-certs.sh` — renew the control-plane leaves from the
  cluster CA, in place, for anything expiring within `DAYS_LEFT` (30 by
  default). It preserves the **subject** of a client cert (the subject is the
  identity RBAC binds to, so re-deriving it by hand is how a renewal quietly
  locks a component out) and the **SANs** of a serving cert, and swaps key and
  certificate together, because a moment with the new key and the old
  certificate is a moment where nothing works.
- **fix:** generated certificates have a real lifetime — a year for a leaf,
  ten for a CA. rcgen's default `notAfter` is the year 4096, which made the
  apiserver's own startup line read "valid for 755801 more day(s)": not a
  lifetime, the absence of one, and a rotation path never exercised until it
  is needed in anger. `notBefore` is backdated five minutes, so a cert minted
  on one machine and used on another with a slightly slow clock is not "not
  yet valid".
- **docs:** [docs/certificates.md](docs/certificates.md), including what is
  still missing: CA rotation (a dual-CA trust-bundle rollover), SA signing key
  rotation, an automatic renewer, and cert-manager CRDs.

### 2026-09-08 — metrics
- **feat:** every component exposes the upstream metric names (#51). The names
  are the point: a Kubernetes dashboard, recording rule or alert should work
  here unchanged. `apiserver_request_total` and
  `apiserver_request_duration_seconds` now carry upstream's full label set
  (`verb,group,version,resource,scope[,code]`) with the **Kubernetes** verb
  rather than the HTTP method — a GET of a collection is a `list`, a GET with
  `?watch=true` is a `watch` — and the labels are derived from the path's
  shape rather than a table of known resource names, so custom resources stop
  landing in `other`. `apiserver_current_inflight_requests` gains
  `request_kind`. New: `etcd_request_duration_seconds{operation,type}` around
  every datastore call, and `apiserver_storage_objects`,
  `apiserver_watch_events_total`, `watch_cache_capacity` from the watch cache,
  which already had the numbers.
- **feat:** the `process_*` family — CPU, RSS, virtual size, start time, open
  and max fds — read from `/proc/self` at scrape time. Upstream gets these
  free from the Prometheus Go client, so every Kubernetes dashboard assumes
  them and nothing in Rust provides them. Absent (not zero) on a non-Linux
  build.
- **feat:** `leader_election_master_status{name}` on the controller manager
  and the scheduler, replacing `controller_manager_leader`, which no dashboard
  looks for. It is 1 on the holder, so two instances both reporting 1 is
  visible immediately rather than when they start fighting.
- **feat:** `scheduler_pending_pods`, `scheduler_e2e_scheduling_duration_seconds`
  and honest `scheduler_schedule_attempts_total` results.
- **refactor:** one exporter (`apimachinery::metrics`) instead of three copies
  of the same thirty lines that had already drifted.
- **docs:** [docs/metrics.md](docs/metrics.md), including what is deliberately
  *not* exported: no `workqueue_*`, because these controllers are poll loops
  with no queue and a constant-zero depth is a number that reads as a fact.

### 2026-09-08 — release artifacts
- **build:** static musl binaries and `FROM scratch` images (#50). The
  Distroless base shipped 20 MB of glibc and 5 MB of documentation per
  component — 37.2 MB and 1,282 files for an apiserver that is one file. Built
  for `x86_64-unknown-linux-musl` with no source changes at all, the images
  are **14 MB / 10 MB / 8.1 MB**, one file each, on no base. That is slab
  saved on every node in the fleet, because stormcos carries every golden on
  every node; and a static binary in a golden has one file to be wrong about,
  where a dynamic one starts only if its loader and libraries are exactly
  where it expects them.
- **build:** `deploy/build-release.sh` — the dev-side build, writing tarballs
  and podman-built scratch images to `$OUT` (default `/build`, so pointing it
  at a golden's NVMe mount writes them straight there). It **refuses to
  package a binary that is not statically linked**: that failure works
  perfectly on the build host and only shows up on a node.
- **build:** CI builds the same way, and attaches the bare musl binary tarball
  alongside the image tarball — a golden wants the file, not an image.
- **docs:** [docs/releasing.md](docs/releasing.md).

### 2026-09-08 — exec, attach, port-forward
- **feat(apiserver):** `pods/exec`, `pods/attach` and `pods/portforward`
  (#42) — so `oc exec`, `rsh`, `cp`, `rsync`, `port-forward` and `debug` work,
  and so the Cilium CLI can talk to its agent. Proxied to the kubelet as a
  transparent connection upgrade: nothing here parses SPDY or WebSocket
  frames, the client's headers (including `Sec-WebSocket-Key`, so the accept
  value is the client's) go up verbatim and the kubelet's `101` comes back
  verbatim, which is one implementation for both protocols. Cilium links
  client-go's `FallbackExecutor` — WebSocket first, SPDY second — and both
  paths are verified end to end against a fake kubelet on dev.
- **fix(apiserver):** the exec query is translated, not forwarded. `kubectl`
  speaks `stdin`/`stdout`/`stderr`; the kubelet's endpoint reads
  `input`/`output`/`error`, and a session opened with no streams attached
  hangs rather than fails. `command` is passed through in order (it is argv)
  and `ports=8080,9090` becomes `port=8080&port=9090`.
- **fix(apiserver):** `kubectl logs -f` streams. The body was read to a String,
  which waits for an end that a followed log does not have, so `follow` hung
  until the client timed out and then printed everything at once.
- **SECURITY(apiserver):** subresource requests were **not authorized at
  all**. `parse_authorization_request` could not parse a path with a
  subresource — `pods/log`, `nodes/status`, every CRD `/status` — and an
  unparsed path skipped the RBAC check entirely. With exec now served that is
  a shell in any pod for anyone who can reach the port. Subresources are now
  parsed and matched as `resource/subresource` (so a rule granting `pods` does
  not grant `pods/exec`, `pods/*` grants every subresource of pods, and `*`
  still grants everything), and **an API path the authorizer cannot parse is
  denied rather than waved through**. Verified: a Role granting `pods` gets
  403 on exec and on log; adding `pods/exec` lets exec through.

### 2026-09-08 — garbage collection
- **feat(controller-manager):** foreground and orphan deletion (#43). The
  apiserver has set `foregroundDeletion` and `orphan` finalizers since v0.7.30
  and nothing ever removed them, so `kubectl delete --cascade=foreground` and
  `--cascade=orphan` left the object in `Terminating` forever. Foreground now
  deletes the dependents first (propagating the policy, so a two-level cascade
  stays foreground all the way down) and clears the finalizer once nothing
  with `blockOwnerDeletion` remains; orphan strips the dead owner from each
  dependent's `ownerReferences` and then lets the owner go.
- **fix(controller-manager):** the collector discovers its kinds instead of
  carrying a hardcoded table of eight. It could not see custom resources, and
  a child whose owner is a kind the collector cannot see is indistinguishable
  from a child whose owner is gone — a KubeVirt `VirtualMachineInstance` owns
  its launcher pod, and every pass would have deleted that pod. Ownership is
  now only acted on for kinds actually observed, and an unknown owner means
  leave the child alone.
- **fix(controller-manager):** removals are sent as RFC-7386 merge patches.
  Strategic merge keys `ownerReferences` by `uid`, so orphaning by sending a
  shortened list removed nothing at all — the reference survived, the owner
  went, and the background pass then collected the dependent that had just
  been orphaned.
- **perf(controller-manager):** the collector sweeps again while a sweep is
  still changing things, instead of advancing one level of a cascade per
  30-second interval. A two-level foreground delete took a minute and a half
  and now takes seconds.

### 2026-09-08 — storage
- **feat(controller-manager):** the PV/PVC binder (#56). The API served both
  objects and nothing ever acted on them: a claim stayed `Pending` forever and
  every pod that mounted one hung. `persistentvolume.rs` is upstream's
  `pv_controller` — default StorageClass resolution (written back, so
  tomorrow's default cannot silently become an existing claim's class),
  matching on class, capacity, access modes, volume mode and selector, the
  smallest volume that fits rather than the first, pre-bound volumes winning
  outright, `kubernetes.io/pvc-protection` and `pv-protection` finalizers,
  `Bound`/`Available`/`Released`/`Lost` phases, the claim's real capacity in
  its status, and Events for each decision.
- **feat(controller-manager):** the hand-off to external provisioners. A claim
  under a class with a real provisioner gets
  `volume.kubernetes.io/storage-provisioner` (and the beta spelling), which is
  the *only* signal the upstream `external-provisioner` sidecar acts on —
  without it `stormblock-csi`, or any OpenShift/vendor CSI driver, provisions
  nothing. `WaitForFirstConsumer` holds the hand-off until the scheduler has
  chosen a node, because a StormBlock PVC is a CoW clone of a blank filesystem
  template made local to that node. rustkube provisions nothing itself and
  deletes no backing volume; see [docs/storage.md](docs/storage.md).
- **feat(controller-manager):** attach/detach (`attachdetach.rs`).
  `csi.stormblock.io` declares `attachRequired: true`, and the only thing that
  asks a CSI driver to attach is the existence of a `VolumeAttachment` object,
  which nothing created. Named exactly as upstream names it (`csi-` + SHA-256
  of handle+driver+node), created for every live pod-to-bound-CSI-volume pair,
  deleted when the last pod on that node lets go.
- **feat(scheduler):** volume-aware placement (`volumebinding.rs`). A bound
  volume's `nodeAffinity` now keeps a pod on the node its storage is on; a
  claim that already selected a node is not re-placed; `CSIStorageCapacity` is
  honoured for drivers that publish it, so a pod is not placed where the
  driver has no headroom. A pod with unbound claims has its node written onto
  the claims and is **not** bound until they are.
- **fix(apiserver):** the write that clears the last finalizer is the delete.
  An object with a `deletionTimestamp` was kept until its finalizers emptied —
  and then kept anyway, because nothing removed it when the list became empty.
  Every finalizer in the system was therefore a permanent one: a PVC that
  never went away, a PV that never released. PUT and PATCH (including
  server-side apply) now remove the object on the write that empties the list.
- **refactor(apimachinery):** one quantity parser (`apimachinery::quantity`)
  and one label-selector matcher (`apimachinery::selector`), replacing three
  drifted copies of `parse_memory_bytes` in the scheduler and a
  matchLabels-only selector in the PDB controller that ignored
  `matchExpressions` — which silently guarded a wider set of pods than it
  named.

### 2026-09-08
- **fix(controller-manager, scheduler):** a credential that is still being
  written is "not yet", not a fatal error (#58). Both processes exited with
  status 1 exactly once on every boot of the R230 and came up on the restart
  two seconds later, which is the whole diagnosis: nothing in either binary
  can produce status 1 except a `?` in `main` — the run loops retry forever
  and a panic would be 101 — and the only failing paths the two share are
  reading `--certificate-authority` / `--client-certificate` / `--client-key`
  and parsing them. The control plane starts as one set of processes and the
  PKI under `/etc/kubernetes/pki` is written alongside them, so the read races
  the write and loses often enough to be the normal case. Credential files are
  now waited for — until they exist *and* every PEM `BEGIN` has its `END`, so
  a file caught mid-write is waited out rather than parsed into a fatal error
  — with a bounded `--startup-timeout` (default 120s) and a log line naming
  what is missing. The retry was load-bearing and undocumented; on a slower
  machine it becomes a crash loop with no better explanation than it had.
- **fix(controller-manager, scheduler):** wait for the apiserver to serve
  before starting work, instead of failing leader-election acquires against a
  socket nobody is listening on. Not reachable within the timeout is a warning
  and the existing retry loop carries on — a component that is up and saying
  the apiserver is unreachable is more use than one that has exited.
- **fix(controller-manager, scheduler):** a fatal startup error is now logged
  through `tracing` as well as returned from `main`, so the reason is in the
  console stream next to everything else rather than only in the exit status.
- **feat(scheduler):** `--token-file`, matching kube-controller-manager.

### 2026-09-02 (v0.7.36)
- **fix(apiserver):** a datastore that cannot serve is **503, not 500** (#57).
  A gRPC `Unavailable` from fastetcd fell into the catch-all arm of
  `From<apimachinery::Error> for ApiError` and came back as 500 carrying the
  backend's raw message. The apiserver was healthy and correct — it was
  faithfully reporting a full disk underneath it — but 500 reads as "the
  apiserver is broken", and `client-go`/`kubectl` retry a 503 with backoff
  while treating 500 as terminal for the call. 503 responses now also carry
  `Retry-After`.
- **fix(apiserver):** the backend's internals no longer reach the client.
  `Status.message` carried ~400 characters of openraft `Debug` output —
  `SnapshotSignature { last_log_id: Some(LogId { leader_id: … } } }`, a raw
  u64 node id, `backtrace: None` — with the one useful fact (`No space left on
  device (os error 28)`) at the far end. The cause is condensed to its first
  line and capped; the full backend error is logged server-side, where it is
  searchable and nobody reads it in `kubectl` output.
- **fix(storage):** classify failures while the gRPC status is still
  structured. `etcd_err` stringified every `etcd_client::Error` into
  `Error::Store`, discarding the status code and leaving downstream code
  nothing to tell "the store is down" from "the store rejected this". gRPC
  `Unavailable` / `DeadlineExceeded` / `ResourceExhausted`, and transport and
  IO failures, now map to a new `apimachinery::Error::Unavailable`; genuine
  store errors still go to `Error::Store` and still 500.

### 2026-08-28
- **fix:** taints and tolerations. A not-ready taint was added when a node went
  bad and **never removed when it recovered**, leaving a healthy node
  permanently unschedulable with the reason in `spec` while everyone reads
  `status`. `NoExecute` now evicts rather than only blocking placement — without
  that it was `NoSchedule` with a different spelling — and `tolerationSeconds`
  is honoured, so a pod that tolerates `not-ready` for 300s gets its five
  minutes instead of a blip becoming an outage. The matching rules move to
  `apimachinery::taint`, shared with the scheduler, whose own copy had already
  drifted: it treated an unknown `operator` as `Equal`, so a typo tolerated a
  taint instead of failing closed.
- **feat:** Deployments do **real rolling updates**. The controller created the
  new ReplicaSet at full size and zeroed every old one in the same pass — an
  outage, not a rollout, and it ignored `maxSurge`/`maxUnavailable` entirely.
  The arithmetic is now pure functions in `rollout.rs` holding two tested
  invariants: never more than `replicas + maxSurge` pods, and never fewer than
  `replicas - maxUnavailable` available — counting the new ReplicaSet's
  not-yet-ready pods against the second, which is the classic bug.
- **feat:** revisions and rollback. Every ReplicaSet carries
  `deployment.kubernetes.io/revision`, retained to `revisionHistoryLimit`; a
  rollback takes a *new* revision rather than reclaiming an old number. This is
  what makes `rollout history` and `rollout undo` work at all.
- **feat:** `Recreate` strategy and `spec.paused`.
- **feat:** CronJob rejects a schedule that can never fire (`0 0 30 2 *`), with
  the reason, at admission — it used to be accepted silently and never run.
- **feat:** CronJob lifetime stats and an Event per scheduling decision, naming
  the scheduled time so a catch-up run is distinguishable from an on-time one.
- **feat:** the apiserver applies a directory of manifests at startup
  (`--manifest-dir`), `EnsureExists` by default and `Reconcile` opt-in.
- **feat:** `route.openshift.io/v1` Routes; printers for routes, services,
  ingresses, deployments and daemonsets; custom resources honour their CRD's
  `additionalPrinterColumns`.
- **fix:** cron's day-of-month/day-of-week **OR** rule was implemented as an
  AND, turning `0 0 1 * 1` from "the 1st, and every Monday" into roughly one
  run every seven months. Missed starts were also lost entirely.
- **fix:** status is written through the `/status` subresource. Nine
  controllers PUT the whole object, so a status write reverted whatever spec
  change had happened since that controller's last list — the Deployment
  controller's scale-up lost to the ReplicaSet controller's status write,
  repeatedly.
- **fix:** CRD registration kept only `spec.versions[0]`, so every other served
  version 404'd — half of Cilium's API did not exist.
- **fix:** the Route group served its resources but was absent from `/apis`, so
  discovery never found them.
- **fix:** a CRD declaring its own Age column no longer gets two.
- **fix:** the pod-template hash moves off `DefaultHasher`, which is documented
  as unstable across Rust releases; it names a ReplicaSet, so a change would
  orphan every existing one and its pods.

## [v0.3.0] — 2026-03-19

### Added
- **Status subresource endpoints** — GET/PUT/PATCH `/status` for all resource types
  - Cluster-scoped and namespace-scoped status handlers with merge-patch support
- **Admission webhooks** — mutating and validating webhook chain
  - JSON patch (add, remove, replace) application from webhook responses
  - Rule matching against webhook configurations (apiGroups, resources, operations)
  - Dynamic webhook loading from storage
- **CSI volume support** — Container Storage Interface plugin framework
  - Identity, Node, and Controller service traits
  - Unix socket CSI client for driver communication
  - Volume setup/teardown helpers for kubelet integration
- **NetworkPolicy enforcement** — network policy engine for pod traffic control
  - CIDR matching, pod/namespace selector matching
  - Ingress/egress rule evaluation with port filtering
  - iptables rule generation from NetworkPolicy resources
- **eBPF service proxy** — BPF-based service dispatch (Linux-only, feature-gated)
  - Service map with endpoint tracking, protocol-aware routing
  - BPF map types for O(1) service VIP resolution
- **eBPF CNI encap/decap** — VXLAN overlay with BPF (Linux-only, feature-gated)
  - Peer management, bulk peer updates, VNI configuration
  - Attach to network interfaces for encapsulation/decapsulation
- **DNS upstream forwarder** — forward non-cluster queries to upstream resolvers
  - Round-robin server selection, UDP transport, configurable timeout
- **HPA controller** — Horizontal Pod Autoscaler (15s reconcile interval)
  - Scales Deployments, ReplicaSets, StatefulSets
  - CPU utilization metrics, velocity-limited scaling (doubles/halves)
  - stabilization window for scale-down cooldown
- **Gateway API controller** — Gateway API v1 support
  - GatewayClass, Gateway, HTTPRoute reconciliation
  - Listener status tracking, route-gateway binding
- **Scheduler preemption** — priority-based pod eviction
  - Finds preemption candidates when scheduling fails
  - Minimizes number of victims, respects PDB-like constraints
  - Resource parsing for CPU (millicores) and memory (bytes)
- **API aggregation layer** — external API server registration
  - APIService registry (register, unregister, lookup)
  - Request proxying to aggregated API servers
  - Availability tracking from APIService status conditions
- **Cloud provider framework** — pluggable cloud controller interface
  - CloudProvider trait (node addresses, zones, load balancers, routes)
  - NoopCloudProvider for bare-metal/dev environments
  - CloudControllerManager with reconciliation loops
- New API groups: `autoscaling/v2`, `networking.k8s.io/v1`,
  `admissionregistration.k8s.io/v1`, `gateway.networking.k8s.io/v1`,
  `apiregistration.k8s.io/v1`
- New list kind mappings for all new resource types

### Changed
- Controller manager now runs 12 controllers (was 10)
- ApiError gains `forbidden()`, `unauthorized()` constructors and `Display` impl
- rk-cni gains tokio and anyhow dependencies for async eBPF module

## [v0.2.0] — 2026-03-18

### Added
- Label and field selector parsing and filtering for list/watch operations
  - Label selectors: `=`, `!=`, `in`, `notin`, exists, `!key`
  - Field selectors: `metadata.name`, `metadata.namespace`, `spec.nodeName`, `status.phase`
  - Selectors applied in list handlers and watch event streams
- Authentication middleware (JWT bearer token with HMAC-SHA256 signing)
- RBAC authorization engine
  - Evaluates ClusterRoleBindings and RoleBindings against ClusterRoles and Roles
  - system:masters group always has full access
  - Bootstrap creates cluster-admin role and system:masters binding
  - Dev-mode anonymous admin access for kubectl without certs
- StatefulSet controller — ordered creation/deletion by ordinal, waits for Ready
- DaemonSet controller — one pod per Ready node, bypasses scheduler via nodeName
- Job controller — tracks completions, parallelism, backoff limits, active deadlines
- CronJob controller — 5-field cron parser, concurrency policies (Allow/Forbid/Replace), history limits
- CRD support — dynamic resource registration via CustomResourceDefinition
  - CrdRegistry for in-memory tracking of registered custom resources
  - Catch-all routes serve custom resources through generic storage layer
  - Dynamic API discovery includes CRD groups
- `batch/v1` API group with jobs and cronjobs resources
- `apiextensions.k8s.io/v1` API group for CRD management

### Changed
- Controller manager now runs 10 controllers (was 6)
- API group discovery is now dynamic (includes CRD groups)
- AppState includes CrdRegistry for dynamic resource support
- ApiServerConfig adds `service_account_key` and `anonymous_auth` fields

## [v0.1.0] — 2026-03-17

### Added
- Pod migration controller — runtime-aware pod migration between nodes
  - MigrationService trait with per-runtime strategies (CRIU, live migrate, snapshot, evacuate)
  - CRIU checkpoint/restore for native containers (~100ms downtime)
  - VM live migration helpers: cloud-hypervisor REST API, QEMU QMP, Firecracker snapshot
  - MigrationService implemented for NativeRuntime, VmRuntime, CriClient
  - PodMigration custom resource (rustkube.io/v1alpha1) with state machine controller
  - Migration state machine: Pending -> Checkpointing -> Transferring -> Restoring -> Verifying -> Completed
  - Kubelet migration annotation handling (checkpoint, prepare-target, live-migrate, restore)
  - Node drain helper (creates PodMigration for all non-DaemonSet pods)
  - Non-Linux stubs for macOS development
- rk-store — KvStore trait implementation wrapping stormforce-kv (CRUD, CAS, watch, lease)
- rk-apiserver — Full K8s REST API server (axum 0.8)
  - Discovery: /api, /apis, /version, /healthz, /livez, /readyz
  - Core v1: namespaces, nodes, pods, services, configmaps, secrets, endpoints, serviceaccounts, events, PVs, PVCs
  - Apps v1: deployments, replicasets, statefulsets, daemonsets
  - Coordination v1: leases
  - RBAC v1: roles, clusterroles, bindings
  - Generic CRUD handlers (GET/LIST/POST/PUT/DELETE) with watch streaming
  - ResourceVersion tracking, K8s Status error responses
  - Bootstrap namespaces (default, kube-system, kube-public, kube-node-lease)
- rustkube-apiserver binary with clap CLI
- kubectl verified: get ns/nodes/pods, version, api-resources all working
- rk-controllers — 5 built-in controllers
  - Deployment: creates/manages ReplicaSets, rolling updates, scale up/down
  - ReplicaSet: creates/deletes Pods to maintain replica count
  - Service: creates/updates Endpoints from selector-matched pods
  - Namespace: ensures default ServiceAccount in each namespace
  - Node Lifecycle: monitors Lease heartbeats, marks nodes NotReady
- rk-scheduler — pod scheduling with filter/score framework
  - Filters: NodeReady, Unschedulable, TaintToleration, NodeSelector, ResourceFit
  - Scores: LeastRequested, ImageLocality, NodeAffinity
  - Plugin trait framework for Phase 3 extensibility
- Single-binary control plane (rustkube) — apiserver + controllers + scheduler
- End-to-end verified: Deployment -> ReplicaSet -> 3 Pods -> scheduled to node
- rk-kubelet — Node agent with CRI, pod lifecycle, health probes
  - CRI trait definitions (RuntimeService, ImageService) matching K8s CRI v1
  - Pod lifecycle manager (Pending -> Running -> Succeeded/Failed)
  - Health probes: HTTP GET, TCP connect, exec, gRPC
  - Node registration and Lease heartbeat reporting
- rk-proxy — Service proxy with iptables DNAT
  - Service map tracking ClusterIP -> pod endpoint backends
  - iptables rule generation with probabilistic load balancing
  - NodePort support, session affinity, IP masquerade
- rk-dns — Cluster DNS server (hickory-dns 0.25)
  - A records for ClusterIP services and headless pod IPs
  - SRV records for named service ports
  - PTR records for reverse DNS
  - Pod DNS: `<ip-dashed>.namespace.pod.cluster.local`
- rk-cni — CNI plugins for pod networking
  - CNI v1.0 spec types (config, result, error)
  - Host-local IPAM with disk-persisted allocations
  - Bridge plugin: veth pair, netns, IP assignment, routing
  - VXLAN overlay: VTEP creation, FDB entries, peer routes
- Native container runtime via youki libcontainer
  - NativeRuntime: OCI container lifecycle without containerd/runc
  - Full OCI spec builder (rootfs, process, mounts, cgroups v2)
  - NativeImageService: image pulls via skopeo
  - Architecture: kubelet -> libcontainer -> kernel (no Go)
- VM runtime for microVM-isolated pods
  - VmRuntime: each pod sandbox runs as a microVM with own kernel
  - Supports cloud-hypervisor (Rust-native), Firecracker, QEMU/KVM
  - Per-pod VM config via annotations (rustkube.io/vm-*)
  - virtiofs volume sharing, guest agent exec, SSH fallback
  - Runtime selection: `--runtime=native|vm|cri --vmm=auto|cloud-hypervisor|qemu|firecracker`
- Initial repository setup — Cargo workspace with 10 member crates
