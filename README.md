# RustKube

**A Kubernetes control plane in Rust**: `kube-apiserver`,
`kube-controller-manager` and `kube-scheduler`, implementing a subset of the Kubernetes API on
the wire for `kubectl`, `oc`, `helm` and client-go controllers. Compatibility
is operation-specific; this is not a conformant or drop-in replacement yet.

This page was checked against the code on **2026-11-13**: v0.18.0 plus
unreleased changes on main (the workspace version has not been bumped since;
component goldens `golden-rustkube-<digest>` are built from main per issue).
See the [change audit](docs/changes-since-2026-09-18.md) for commits,
verification limits and tracked gaps. Code on main, or in a golden, has not
necessarily shipped in a stormcos release.

It is the control plane only. The datastore, the node agent and DNS are
separate components:

| | component | what it is to rustkube |
|---|---|---|
| datastore | [fastetcd](https://github.com/glennswest/fastetcd) | an etcd v3 wire-compatible store; the apiserver reaches it over gRPC (`--etcd-servers`). It is the only rustkube component that does; stormconsole reads fastetcd's status and metrics too |
| node agent | [rustkube-node](https://github.com/glennswest/rustkube-node) | the kubelet; the apiserver proxies `logs`, `exec`, `attach`, `port-forward`, `nodes/proxy` and the VM console/verbs to it on `:10250` |
| certificates | [stormcert](https://github.com/glennswest/stormcert) | writes the serving cert, the CA and the ServiceAccount keypair the apiserver reads |
| cluster DNS | stormcoredns, deployed from a manifest | rustkube knows nothing about DNS |
| where it runs | [stormcos](https://github.com/glennswest/stormcos) | ships each binary as a stormd golden — see [How it ships](#how-it-ships) |

```
kubectl / oc / client-go ──HTTPS :6443──▶ kube-apiserver ──gRPC──▶ fastetcd :2379
                                              ▲    │
                        kube-controller-manager    └─HTTPS :10250─▶ kubelet (rustkube-node)
                        kube-scheduler              logs, exec, attach, port-forward, nodes/proxy
```

The target is 100–1000+ nodes. The largest run so far is a synthetic
250-node, 3000-pod cluster (#66); nothing has run at that size with real nodes.

## Layout

Upstream-shaped: thin `cmd/<component>` binaries over `pkg/<lib>` libraries.

```
cmd/kube-apiserver           → pkg/apiserver           REST API (axum), auth, RBAC, admission, watch cache
cmd/kube-controller-manager  → pkg/controller-manager  the built-in controllers
cmd/kube-scheduler           → pkg/scheduler           filter / score / bind
                               pkg/apimachinery        errors, the KvStore trait, protobuf codec, metrics, quantities, quota, selectors, cron, TLS reload, reflector/informers/work queues, leases
                               pkg/storage             the KvStore implementation over etcd v3 (etcd-client)
                               pkg/cloud               empty: a doc comment and no code; nothing depends on it
test/                          rustkube-test           the test container (/test short|medium|long), test/README.md
test/e2e, test/conformance                             rigs (a real control plane on fastetcd; suites `rigs`/`rigs-night` of the test container), and the upstream conformance suite
```

## What the apiserver does

**Storage.** Objects are JSON under `/registry/{resource}/…` in fastetcd;
custom resources under `/registry/{group}/{plural}/…` (#76). `resourceVersion`
is the store's `mod_revision`. Every write is a compare-and-swap; a PATCH
without a `resourceVersion` retries the CAS the way upstream's
`GuaranteedUpdate` does (#77). A watch cache sits in front of watches and
feeds RBAC (below). GETs and LISTs follow upstream's cacher (#171): with no
`resourceVersion` they read the shared datastore (one linearizable Range);
with `resourceVersion=0`, or `N` (`NotOlderThan`, the default match), they are
answered from the watch cache of the whole resource once it has reached `N`
(it waits up to 50 ms, then reads the datastore, which is never older);
`resourceVersionMatch=Exact` reads the datastore at `N`; a non-numeric
`resourceVersion` is a 400. So an informer relist storm after a restart is one
seed per resource, not one Range per client;
`apiserver_watch_cache_reads_total{operation,type}` counts what the cache
answered beside `etcd_request_duration_seconds`. Continuation pages request
the first page's revision, even through another API server — a page served
from the cache hands out the same `{revision}:{key}` token, so the next page
is the datastore at the cache's revision. That needs correct datastore snapshots,
which fastetcd has from v1.6.1 ([fastetcd#50](https://github.com/glennswest/fastetcd/issues/50),
fixed); `test/e2e/list-snapshot-race.sh` checks it. The apiserver compacts the
datastore every `--etcd-compaction-interval` (5 m, as upstream; #139): one
apiserver per interval wins a CAS on `compact_rev_key` and compacts to the
revision it recorded an interval earlier, so a snapshot stays readable for
one to two intervals. A continue token whose snapshot is compacted is a 410
`Expired` whose Status carries an "inconsistent" token in
`metadata.continue`, listing the rest from the same key at the current
revision; a continue key outside the list's own prefix is a 400 `invalid
continue token` (the store adapter refuses it too, #350); a LIST at a compacted `Exact` revision and a watch from below the
compaction are 410 `Expired` too. fastetcd compacting on its own (its space
reclaim) is treated as done, not an error. A guaranteed update (PATCH,
every `/status` PUT, `/scale`, `resize`, a delete that only marks the object
terminating) times its phases — read, mutate,
webhooks, write, retry wait — into `apiserver_write_phase_duration_seconds`
and logs a `slow write` warning with the key and the split when it takes over
100 ms; any request over 100 ms, watches and streams aside, logs `slow
request` (#191). Controller and scheduler
leadership uses Kubernetes Leases; watch queues are local, reconstructible
state. Lease expiration uses local elapsed time rather than comparing master
wall clocks. Three-master failure testing remains required before rollout (#149).

**API groups served** (and advertised in `/api`, `/apis`):
`v1`, `apps/v1`, `batch/v1`, `autoscaling/v2`, `autoscaling/v1`
(HorizontalPodAutoscaler as a view of the stored v2 object, converted as
upstream does — `targetCPUUtilizationPercentage` ⇄ the cpu Utilization
metric, the rest in the `autoscaling.alpha.kubernetes.io/*` annotations; a
PATCH applies to the v1 view; list and watch converted; #123), `policy/v1`,
`networking.k8s.io/v1` (NetworkPolicy, Ingress, IngressClass, and ServiceCIDR
with `/status` and IPAddress, #134: a `kubernetes` ServiceCIDR holding
`--service-cidr` is bootstrapped Ready; the ClusterIP allocator does not yet
write IPAddress objects or read other ServiceCIDRs), `discovery.k8s.io/v1`, `events.k8s.io/v1` (translated
to and from stored core/v1 Events), `coordination.k8s.io/v1`,
`rbac.authorization.k8s.io/v1`, `authorization.k8s.io/v1`,
`nodes/{name}/proxy/{path}` (#108: every method passed to the node's kubelet
on :10250 with the apiserver's own token, authorized as `nodes/proxy` —
`kubectl get --raw …/proxy/stats/summary`, and `oc adm node-logs` once the
kubelet serves `/logs/`, rustkube-node#198),
`authentication.k8s.io/v1` SelfSubjectReview (`kubectl auth whoami`: the
caller's username and groups as authenticated, granted to every
authenticated user by `system:basic-user`, #116),
`authorization.openshift.io/v1` (Subject/LocalSubject/Resource/LocalResource
AccessReview — `oc adm policy who-can` and `oc adm new-project`'s check —
answered by the RBAC engine; a ResourceAccessReview lists the users and groups
of every binding whose role permits the request, and an OpenShift kind asked
about in the legacy empty group is answered for its own group too; #106),
`certificates.k8s.io/v1`, `storage.k8s.io/v1`, `admissionregistration.k8s.io/v1` (webhook
configurations, called on writes, below; Validating/MutatingAdmissionPolicy
and their bindings stored and served with `/status`, but **not evaluated** —
a create says so in a `Warning`; #119, #234),
`node.k8s.io/v1` (RuntimeClass, #135; a Pod naming one is admitted against it —
missing class 403, `overhead.podFixed` into `spec.overhead` (a different or
classless overhead 403), the class's `scheduling` nodeSelector and tolerations
merged — and the scheduler counts `spec.overhead` in resource fit),
`flowcontrol.apiserver.k8s.io/v1` (FlowSchema and PriorityLevelConfiguration
with `/status`, upstream's mandatory `exempt`/`catch-all` objects created at
boot; stored and served, **not enforced** — no request is queued or
classified, #118),
`resource.k8s.io/v1` (DRA's DeviceClass, ResourceSlice, ResourceClaim with
`/status`, ResourceClaimTemplate — stored and served, nothing allocates:
#137, #225),
`apiextensions.k8s.io/v1` (CRDs, served dynamically; a version's structural
schema defaults custom resources on write and on read, prunes undeclared
fields, and `fieldValidation=Strict` refuses them (`Warn`, the default,
returns warnings) — #121; the schema's value validations (type, required,
enum, pattern, lengths, bounds, item/property counts, list-type set/map
uniqueness) refuse a write with upstream's 422 field errors, ratcheting an
unchanged value — #240; `format`, `allOf`/`anyOf`/`oneOf`/`not`, CEL
`x-kubernetes-validations` and `/status` validation are not checked, and
there is no conversion), `apiregistration.k8s.io/v1` (APIServices: aggregated APIs, below),
`gateway.networking.k8s.io/v1`, `route.openshift.io/v1` (stored; nothing
routes for it, #70), `project.openshift.io/v1` (Projects, below),
`user.openshift.io/v1` (Groups, Users, Identities and UserIdentityMappings, below),
`apps.openshift.io/v1` (DeploymentConfigs, below),
`build.openshift.io/v1` (Builds and BuildConfigs, below),
`security.openshift.io/v1` (SecurityContextConstraints and the three
PodSecurityPolicy reviews, below),
`rustkube.io/v1alpha1` (PodMigration) and
`subresources.kubevirt.io/v1` (VM console/VNC, proxied to the kubelet; the
VMI verbs `pause`, `unpause`, `softreboot`, `freeze`, `unfreeze` (#141), PUT
proxied to the kubelet's `/vmVerb` (rustkube-node#94) and stormvm, its answer
passed through, a `dryRun` body answered without the call, 409 for a VMI on no
node, granted by `edit`;
`start`/`stop`/`restart`; and `migrate` on a VirtualMachine — what `virtctl
migrate` calls — or a VirtualMachineInstance, which creates a
VirtualMachineInstanceMigration: 404 while that CRD is not installed, 409 for
an instance not `Running` or already migrating; `dryRun` honoured, #184;
`addedNodeSelector` is carried onto the migration's `spec.addedNodeSelector`
and the scheduler adds it to the VMI's node selector for the target — a key
the VMI already selects on keeps the VMI's value, a selector no node meets is
`TargetScheduled=False`, a non-string value 422, #208).

`scheduling.k8s.io/v1` (PriorityClass) and `authentication.k8s.io/v1`
(TokenReview) are served and advertised in `/apis` (#85).

**Custom resources** are served from an in-memory registry of the stored
CRDs. At boot every stored CRD is registered before the API listens (the read
is paged and retried for a minute if the datastore fails it); after that the
registry follows the CRD prefix through the watch cache, so a CRD created or
deleted through another apiserver, or one a slow boot could not read, is
served or unserved within about a second. `/readyz` answers 503 until the
stored CRDs are registered (#185).

**Wire.** JSON and client-go's protobuf (`application/vnd.kubernetes.protobuf`)
in both directions; Table output for `kubectl get` (the create-only reviews —
TokenReview, the access reviews, SelfSubjectRulesReview — have none: an
Accept offering only a Table is 406 `NotAcceptable`, #126); `PartialObjectMetadata`;
watch with bookmarks, `sendInitialEvents` and `timeoutSeconds` (the stream
ends cleanly at that deadline, after what was already sent; absent or 0 is no
deadline (upstream picks a random one from `--min-request-timeout`; that is #246);
not an integer is 400; #165) (a watch with no
`resourceVersion`, or `0`, starts with the current objects as ADDED events;
a DELETED event carries the
object's last state from the watch cache, and selectors apply to it; a watch
opened below the cache's window gets a name-and-namespace tombstone, #100; to
a selector watch, an object that stops matching is DELETED and one that starts
matching is ADDED);
list pagination with `continue`
tokens (every page reports the first page's `resourceVersion`, with
`remainingItemCount`; items carry their own `resourceVersion`); label and
field selectors; aggregated discovery (#107: `/api` and `/apis` answer
`apidiscovery.k8s.io/v2` — or `v2beta1` — `APIGroupDiscoveryList` when
`Accept` asks for it in JSON, every group, version and resource with
subresources folded in, built from the same lists as the legacy documents and
the CRD registry; protobuf is not offered and clients fall to the JSON form);
a resource no built-in group-version advertises is 404 `the
server could not find the requested resource`, not an empty list from a
catch-all route (#110); a missing object is upstream's NotFound
(`deployments.apps "web" not found`, Status `details` with name, group and
kind; #109); `/openapi/v2` and `/openapi/v3` (built-in types as GVK paths
without schemas; every served CRD version published from its schema as
`{reversed group}.{version}.{Kind}` with `apiVersion`/`kind`/`metadata`
(ObjectMeta's fields with upstream's descriptions, from the vendored proto)
and its GVK — boolean `x-kubernetes-*` extensions only when true; v2 without
what Swagger 2.0 cannot say, `x-kubernetes-preserve-unknown-fields` included
— and its group-version in the v3 index, so `kubectl explain` works on custom
resources, #120). `/openapi/v2` is JSON, or gnostic's protobuf
`openapi.v2.Document` (from the vendored `OpenAPIv2.proto`, gnostic-models
v0.7.0) when the Accept asks for
`application/com.github.proto-openapi.spec.v2@v1.0+protobuf`, as client-go's
`OpenAPISchema()` does (or the `…spec.v2.v1.0+protobuf` spelling, which is
the Content-Type it is answered with, as upstream answers) — so `kubectl explain --output=plaintext-openapiv2`
works on custom resources; an Accept offering neither is 406 (#343).

**Bodies and field validation** (#122). A write's body may be JSON,
protobuf or YAML (`application/yaml`, converted to JSON before any handler);
one without a `Content-Type` is JSON, as upstream negotiates. For a built-in
kind, a create (POST) or replacement (PUT, `/status` too) is checked against
its type — the vendored release-1.36 protobuf descriptors' fields — and
`?fieldValidation=Strict` refuses an unknown field or a repeated key with
upstream's `strict decoding error: unknown field "spec.foo", duplicate field
"spec.replicas"` (a repeated YAML key: `line N: key "x" already set in map`),
`Warn` (the default) names them in `Warning` headers, `Ignore` says nothing.
Unlike upstream, a field accepted under `Warn`/`Ignore` is stored as sent,
not dropped. PATCH bodies are not checked here, nor CustomResourceDefinitions;
custom resources follow their schema (#121).

**Writes.** Create (a body's empty `metadata.namespace` is the URL's; a
different one is a 400), update, delete (with `DeleteOptions`: preconditions,
`dryRun`, grace period, `propagationPolicy`), `deletecollection` with label
and field selectors on every generic collection path and for custom resources
(not across namespaces, and not for namespaces), JSON Patch, merge patch,
strategic merge patch (fixed merge-key table; a Service's ports merge on port
and protocol, a container's on containerPort, #150) and
server-side apply with `managedFields` ownership and conflicts. The `/status`
subresource; pod `eviction` gated by PodDisruptionBudgets as upstream (one
budget per pod; a terminating, finished or Pending pod needs none; a
not-Ready one goes under `IfHealthyBudget` when the budget is met, or
`AlwaysAllow`; otherwise the disruption is reserved in the PDB's status —
`disruptionsAllowed` decremented and the pod recorded in `disruptedPods` — by
one compare-and-swap before the pod is deleted, so concurrent evictions on
any apiserver cannot spend one disruption twice; 429 when none is left,
#360); pod `resize`
(in-place resize, #136: GET/PUT/PATCH change only containers' and sidecars'
`resources`/`resizePolicy`, validated as upstream — QoS class kept, cpu and
memory only, nothing removed, non-sidecar init containers fixed; a Pod with a
running container whose status reports no `resources` is refused, so until
the kubelet resizes (rustkube-node#192) only not-yet-running Pods resize); namespace
`/finalize` (a body `uid` that is not the namespace's, or a `resourceVersion`
that is not its current one, is a 409 and changes nothing; the update, or the
delete of a terminating namespace left with no finalizers, is conditional on
the revision read, #366); CSR `/approval`. A PUT to `/status` (and `/approval`) is
conditional on the body's `resourceVersion`: stale is a 409 and nothing is
written; none is an unconditional update (#78). A `/status` write changes
only status (and labels/annotations), never spec. For a custom resource whose
CRD version enables `subresources.status`, status belongs to `/status`, as
upstream: a create through the main resource drops the body's status, and
PUT, PATCH and server-side apply through it keep the stored status (#128).
Without the subresource, status is an ordinary field there. Built-in objects'
main writes still store the status they are sent. A custom resource's
`metadata.generation` is kept as upstream keeps it (#198): `1` on create, then
+1 on each main-resource write that changes `spec` (with the status
subresource) or anything outside `metadata` (without); `/status` and
metadata-only writes leave it, and a value the client sends is ignored, so a
controller can report `status.observedGeneration`. Built-in objects follow
upstream's strategies (#285): Pods (1.35), Deployments, ReplicaSets,
StatefulSets, DaemonSets, ReplicationControllers, Jobs, CronJobs,
PodDisruptionBudgets, NetworkPolicies, Ingresses, CRDs, admission policies
and their bindings, FlowSchemas, PriorityLevelConfigurations, DeviceClasses
and ResourceSlices get `1` on create and +1 when an update through the main
resource changes `spec` (a Deployment also its annotations; webhook
configurations their `webhooks`; a Pod's `spec.nodeName` — the scheduler's
bind — does not count), whether by PUT, PATCH, apply or after a mutating
webhook. A `/status` write keeps it, whatever labels or annotations it carries (the Deployment controller's revision annotation rides on its status write, as upstream's). Other kinds keep what is stored and take none from a client. The
update that starts a deletion moves a positive generation on, as upstream.

**`/scale`** (#86), `autoscaling/v1` Scale, GET/PUT/PATCH, for
Deployments, ReplicaSets and StatefulSets (`spec.replicas`,
`status.replicas`, `spec.selector` as a string), and for custom resources
whose CRD version declares `subresources.scale` (its
`specReplicasPath`/`statusReplicasPath`/`labelSelectorPath`). A write
changes only the replicas, conditional on the Scale's `resourceVersion`
when it carries one (409 when stale), negative replicas 422; it is admitted
as an update of the object. Discovery lists each `…/scale` (and a CRD's
`…/status`); `kubectl scale` and `oc scale` work.

**Proxied to the kubelet** (`https://<node>:10250`, authenticated with a token
the apiserver mints for itself as `system:kube-apiserver`): `pods/log`
(including `follow`), and `pods/exec`, `pods/attach`, `pods/portforward` as a
transparent connection upgrade, so SPDY and WebSocket both pass through. The
kubelet end of exec/attach/port-forward does not exist yet in rustkube-node
(rustkube-node#56), so those three answer with the kubelet's 404 today.

**Authentication**, first match wins:
1. an x509 client certificate verified against `--client-ca-file`, and not
   revoked by a `--client-crl-file` (#260) — CN is the user, each O a group;
2. a bearer token: first a static token from `--token-auth-file`
   (`token,user,uid[,"group1,group2"]`, upstream's format; compared in
   constant time; the file is re-read every 5 s, so it may appear late,
   change, or be removed to revoke — a malformed rewrite keeps the last good
   set; #188). stormcos writes install-config's `apiToken` there as
   `system:admin` in `system:masters` (stormpump#78). Then a **bootstrap
   token** `<id>.<secret>` (#264, upstream's): the Secret
   `kube-system/bootstrap-token-<id>`, `type: bootstrap.kubernetes.io/token`,
   `usage-bootstrap-authentication: "true"`, not past its `expiration`,
   makes it `system:bootstrap:<id>` in `system:bootstrappers` (+ any
   `auth-extra-groups` under `system:bootstrappers:`); deleting the Secret
   revokes it; TokenReview answers it too. Otherwise a JWT signed
   with the ServiceAccount key (RS256 with `--service-account-*-file`,
   otherwise an ephemeral HS256 key). A ServiceAccount's groups come from its
   name. What an offline-minted token must carry is in
   [docs/certificates.md](docs/certificates.md);
3. a bearer token that neither accepts is 401 (a `Status`, reason
   `Unauthorized`), whatever `--anonymous-auth` says, as upstream (#115);
4. a request with no credentials (no client certificate, no `Authorization`,
   an empty `Bearer ` token, or another scheme) is `system:anonymous` if
   `--anonymous-auth` is true; else 401.

An authenticated user named in a Group's `users` is also in that Group, as
OpenShift's group cache adds it (#372); TokenReview reports it too.

**DeploymentConfigs** (`apps.openshift.io/v1` `deploymentconfigs`, short
name `dc`, #375) are OpenShift's, with `instantiate` (`oc rollout latest`),
`rollback` (`oc rollback`, `oc rollout undo`: the rolled-back config is
returned, the client stores it), `log`, `scale` (autoscaling/v1) and
`status`. Writes get OpenShift's defaults (a ConfigChange trigger,
Rolling with 25%/25%, timeouts 600 s, `activeDeadlineSeconds` 21600,
`revisionHistoryLimit` 10, a lone container as every hook's) and validation;
a spec write keeps the status but for a `latestVersion` one ahead, a
`/status` write keeps the spec. Refused, because nothing here can run them:
the Custom strategy, `tagImages` hooks and ImageChange triggers (no
ImageStreams). Each version is a ReplicationController `<dc>-<version>`,
made by the controller-manager with OpenShift's annotations (phase, version,
the encoded config), so `oc rollout history|status|cancel|retry` and `oc adm
prune deployments` read them as OpenShift's. **There is no deployer pod**: the
controller-manager runs each rollout itself (Rolling: kubectl's rolling
updater, surge and readiness; Recreate: down to zero, up through one
accepted pod), the strategy's `pre`/`mid`/`post` hooks as pods
`<rc>-hook-<pre|mid|post>` from the named container's image with
`failurePolicy` applied (`oc set deployment-hook`), and records the log the
deployer would print on the ReplicationController
(`deploy.rustkube.io/log`, its last 64 KiB) — that is what `oc logs dc/…`
shows, a hook pod's own output included. A cancelled version is marked
Failed and its hook pods deleted; a failed one leaves the last complete
version running. The rollouts restart from their records when a new
controller-manager leader takes over.

**Builds and BuildConfigs** (`build.openshift.io/v1` `builds`,
`buildconfigs` (`bc`), #374) are OpenShift's, with `buildconfigs/instantiate`
(`oc start-build`), `builds/clone` (`--from-build`), `builds/log` (`oc logs
build/…|bc/…`, the init containers' logs then the builder's) and
`builds/details` (the builder records its revision and output). Writes get
OpenShift's defaults (Serial run policy, history limits 5 and 5, the v1
`type` fields) and validation; a build's spec is immutable but for its push
secret, and a finished phase stays. Creating a build or BuildConfig, or
starting one, needs `create` on `builds/<strategy>` (`docker`, `source`,
`custom`: OpenShift's BuildByStrategy); `system:build-strategy-docker` and
`-source` are bound to `system:authenticated`, `-custom` is not. Refused,
because nothing here can run them: the JenkinsPipeline strategy, Binary
sources (`instantiatebinary` is not served), webhook triggers (`webhooks` is
not served), ImageChange triggers (no ImageStreams, #385/#386) and a Custom
build's `exposeDockerSocket`; an ImageStreamTag `from` fails as OpenShift's
does when the stream is missing, and an ImageStreamTag output gets
InvalidOutputReference. The controller-manager runs them: a BuildConfig's
ConfigChange trigger starts its first build; a build runs when its run
policy lets it, in OpenShift's build pod `<build>-build` — **OpenShift's
builder image** (`--build-image`), given the build as `BUILD`, with
`git-clone`, `extract-image-content` and `manage-dockerfile` init containers
and `docker-build` / `sti-build` (a Custom build runs its own image). The
builder clones, builds, runs the post-commit hook (`oc set build-hook`) and
pushes itself, with the secrets `oc set build-secret` names mounted at
OpenShift's paths. Build pods are privileged (buildah), so the
controller-manager may `use` the `privileged` SCC (that SCC only). On a
stormcos node the paths OpenShift mounts from the node are emptyDirs, and
`--build-insecure-registries` lets a build push to a node registry over
HTTP. The phase, start, completion, duration and a failed build's log
snippet follow the pod; `oc cancel-build` deletes it; finished builds past a
BuildConfig's history limits are pruned. Every namespace has a `builder`
ServiceAccount (the controller-manager's) bound to `system:image-builder` by
the RoleBinding `system:image-builders` (the apiserver's: it records
`builds/details` and pushes to the project's registry).

**Groups** (`user.openshift.io/v1` `groups`, cluster-scoped, #372) are what
`oc adm groups new|add-users|remove-users|sync|prune` and `oc adm prune
groups` act on: stored as written (CRUD, watch, RBAC as any cluster-scoped
kind; `users: null` when there are none, as OpenShift writes it; `oc get
groups` prints NAME and USERS). Validated as OpenShift's: the name is a path
segment with no `:` and is not `~`, so no Group can put anyone into a
`system:` group; each user is non-empty and of the same shape, except that a
ServiceAccount's user name `system:serviceaccount:<namespace>:<name>` may be
listed (OpenShift refuses it; StormCOS's users are ServiceAccounts). A
request from a listed user carries the Group (read from the watch cache,
rebuilt when a Group changes), so a RoleBinding to the Group grants its
members; impersonation carries only the groups it names.

**Users, Identities and UserIdentityMappings** (`user.openshift.io/v1`,
#390) are what `oc create user|identity|useridentitymapping` write. Ported
from OpenShift's oauth-apiserver. `users` and `identities` are cluster-scoped
and stored as written (every verb, RBAC as any cluster-scoped kind;
cluster-admin only), with OpenShift's strategies and validation: a User's
`groups` is cleared on every write and written `null`, its name a path
segment with no `:` unless it begins `b64:` and not `~` (so there is no User
for a ServiceAccount, nor any need of one), each of its `identities` a
`<provider>:<login>` name; an Identity is named `<providerName>:<providerUserName>`
by the server whatever the body says, both fixed after create, and its
`user` is `{}` until mapped. `oc get users` prints NAME, UID, FULL NAME,
IDENTITIES and `oc get identities` NAME, IDP NAME, IDP USER NAME, USER NAME,
USER UID, as OpenShift's printers. **`useridentitymappings`** are not stored
(create, get, update and delete; no list or watch): creating `p:login` → `u1`
adds `p:login` to u1's `identities` and sets the Identity's `user` to u1's
name and uid; get reads that link back (both halves must agree; the
mapping's name, uid and resourceVersion are the Identity's); update (PUT)
moves the Identity to another User, which the old one stops listing; delete
undoes the link and leaves both objects. A second create of a mapped Identity
is AlreadyExists; an Identity or User that does not exist is a 422
(`referenced user does not exist`); `?dryRun=All` writes nothing.
**`users/~`** is the requester (`oc whoami`): the stored User of that name
with the request's groups in `groups`, or one made from the request alone
when there is none or the name cannot be a User's (a ServiceAccount); every
authenticated user may `get` it (the `basic-user` ClusterRole, reconciled at
boot). Nothing authenticates through an Identity (there is no OAuth server
or identity provider here): Users and Identities are records, and a User's
name is what RBAC subjects of kind User match anyway.

**API aggregation** (#83). An `apiregistration.k8s.io/v1` APIService with
a `service` makes its group/version an aggregated API, as with upstream's
kube-aggregator:
- every apiserver follows the stored APIServices (within about a second) and
  checks each one's backend every 5 s, writing its `Available` condition when
  the answer changes: `Local` without a service, `ServiceNotFound`, or a GET
  of `/apis/{group}/{version}` on the backend — `Passed` on a 2xx,
  `FailedDiscoveryCheck` otherwise;
- a request under `/apis/{group}/{version}` is proxied once authentication
  and RBAC here have passed (a backend may authorize again). The backend is
  dialled at the Service's ClusterIP with TLS verified for
  `<service>.<namespace>.svc` against `caBundle` (none: the system roots;
  `insecureSkipTLSVerify` verifies nothing), presenting
  `--proxy-client-cert-file`. The caller is named in `X-Remote-User` and
  `X-Remote-Group`; the caller's own `Authorization`, `Impersonate-*` and
  `X-Remote-*` headers are not forwarded. Responses stream (watches work);
  bodies and protobuf are passed as they are. Not `Available`: 503.
  Connection upgrades (WebSocket, SPDY) go to the backend over HTTP/1.1
  with their negotiation headers; its `101` is handed back and the two
  connections spliced, any other answer passed back as it came (#322);
- aggregated groups are in `/apis` and `/apis/{group}`; `/apis/{group}/{version}`
  is the backend's own answer; in aggregated discovery their versions are
  listed without resources, `freshness: Stale` (#107);
- a built-in group is never proxied, whatever an APIService says;
- `kube-system/extension-apiserver-authentication` publishes `client-ca-file`,
  `requestheader-client-ca-file` (`--requestheader-client-ca-file`),
  `requestheader-allowed-names` and the `X-Remote-User`/`X-Remote-Group`/
  `X-Remote-Extra-` header names, rewritten when a CA file changes; the
  `system:auth-delegator` ClusterRole and the kube-system Role
  `extension-apiserver-authentication-reader` are bootstrapped for backends to
  bind to, as upstream's are.

**Impersonation** (`Impersonate-User`, `-Group`, `-Uid`; `kubectl --as`): the
caller needs the `impersonate` verb on the `users` (or `serviceaccounts`),
`groups` and `uids` it names, and the request is then authorized as that
identity, plus `system:authenticated`. `Impersonate-Extra-*` is ignored.

TokenRequest (`serviceaccounts/{name}/token`) mints upstream's token shape
(#182): `iss` (`--service-account-issuer`), `aud` from `spec.audiences`
(default `--api-audiences`, which defaults to the issuer), `nbf`, `jti`, and
the `kubernetes.io` claim naming the ServiceAccount and, with
`spec.boundObjectRef`, the Pod, Secret or Node it is bound to (uid checked;
a Pod must run as that ServiceAccount, and its node is named too).
`expirationSeconds` is honoured from 600 s to 2^32 s; left out, a token
lasts an hour, as upstream's (#206; it was 24 h until every release's
kubelet asked for its own and refreshed, rustkube-node#122, stormcos 11.88). A pod-bound request for 3607 s for the
API audiences — a projected token volume's — gets a year with `warnafter`
at 3607 s, as upstream (`--service-account-extend-token-expiration`).
A JWT authenticates to the apiserver only if its `aud` (when it has one)
names an API audience, its `iss` (when it has one) is ours, and — for a
token with a `kubernetes.io` claim — its ServiceAccount and bound object
still exist with the uids it was issued for (a deleted pod's token lasts a
minute past its `deletionTimestamp`). Those objects are read from the watch
cache, and a refusal is confirmed from the datastore. TokenReview checks
`spec.audiences` the same way, answers static tokens too, and reports
`status.user.uid` and the pod in `status.user.extra`
(`authentication.kubernetes.io/pod-name`, `pod-uid`, `node-name`,
`node-uid`, `credential-id`).

**Authorization** is RBAC against the stored Roles and Bindings, plus
SelfSubjectAccessReview, SelfSubjectRulesReview, SubjectAccessReview and
LocalSubjectAccessReview. A request is checked against in-memory copies of
the ClusterRoleBindings, ClusterRoles, RoleBindings and Roles, kept current by
the watch cache (#177), so authorizing costs no datastore read. Only an allow
is taken from memory: a request the copies would refuse is checked again
against the datastore, so a grant applies to the very next request, while a
revocation applies once its watch event arrives (milliseconds, or up to the
watch cache's 35 s stall window if its watch silently stalls). A refusal
the datastore could not confirm — a binding or role list/read failed for any
reason but NotFound — is **503 `ServiceUnavailable` with `Retry-After: 1`**,
not 403, so clients retry instead of taking a missing grant (#293).
`test/e2e/get-latency.sh` measures it: one datastore read per authorized GET,
and a ServiceAccount's GET as fast as system:masters'. Under write load every
read is one linearizable fastetcd Range; before fastetcd v1.9.0 that Range
queued behind writes for up to seconds (fastetcd#71). On v1.9.0 the rig's
GET p99 under 40-client Lease-renewal load is about 70 ms.

**Escalation prevention** (#98), as upstream: writing a Role or ClusterRole
needs the `escalate` verb on it, or every rule it grants already held by the
caller (in the role's namespace; cluster-wide for a ClusterRole); one with an
`aggregationRule` needs `escalate`. Writing a RoleBinding or
ClusterRoleBinding needs the `bind` verb on the referenced role (by name, in
the binding's namespace), or every rule of that role held in the binding's
scope. Otherwise the write is 403 `attempting to grant RBAC permissions not
currently held`, listing what is missing. Held means upstream's rule
coverage: `*` holds anything, a `*` in the new rule needs a `*`, `*/status`
and `pods/*` hold the subresources they name, resourceNames hold only those
names. `system:masters` is never checked, nor an update that changes only
ownerReferences or finalizers. It runs on every create/update path (POST,
PUT, PATCH, server-side apply), after the mutating admission webhooks. So
is a Job's `spec.managedBy` (valid on create, unchanged on update): a
mutating webhook cannot set an invalid manager or move a Job to another
controller (#366).

**A new object's `uid` and `creationTimestamp` are the server's** on every
API create (POST, server-side apply create, custom resources,
`events.k8s.io`), whatever the body or a mutating webhook says, as upstream's
`FillObjectMetaSystemFields`: recreating a deleted Pod or ServiceAccount
with its old uid does not bring back the tokens bound to it (#366).

**Projects** (`project.openshift.io/v1`, #97) are Namespaces with owners.
Nothing is stored as a Project: each is the Namespace of the same name,
translated on the way out, so deleting a project deletes its namespace and
the namespace cascade takes everything in it.

- `oc new-project` (a ProjectRequest) is open to every authenticated user
  through the `self-provisioners` ClusterRoleBinding. It creates the Namespace
  annotated `openshift.io/requester` (from the authenticated identity, never
  the request), `openshift.io/display-name` and `openshift.io/description`,
  and a RoleBinding `admin` giving the requester the `admin` ClusterRole in
  it. `default`, `openshift` and names starting `kube-` or `openshift-` may
  not be requested. To turn self-service off, empty the subjects of
  `self-provisioners` (a boot does not restore them).
- `oc projects` / `oc get projects` list only the namespaces the caller holds
  a RoleBinding in — any role — unless the caller may `list namespaces`
  cluster-wide, who sees all. Watching projects is limited to the latter.
- `get`, `update` and `delete` of `projects/{name}` are authorized in the
  project's own namespace, so a project's `admin` can delete it and nobody
  else's. So is `get namespaces/{name}`, as upstream does. A project update
  changes only its display name and description; the Namespace's labels
  (`pod-security.kubernetes.io/enforce` among them) stay cluster-scoped to
  write. A project admin can share only what `admin` holds: binding
  cluster-admin, or writing a Role beyond `admin`, is refused (#98).
- **Requester stamp on `storage.storm.io` objects** (#210): on create of any
  object in that group, the apiserver sets `storage.storm.io/requester` (the
  authenticated username) and `storage.storm.io/requester-groups` (its
  groups, comma-joined) over whatever the client sent. Every update (PUT,
  PATCH, server-side apply, `/status`) keeps the stored values, so they keep
  naming the creator; an object stored without them cannot gain them. It runs
  after the mutating webhooks. stormdrive's controller reads it to
  SubjectAccessReview a `DriveOperation`'s creator (stormdrive#45).
- Sharing is RBAC: `oc adm policy add-role-to-user edit bob -n demo` binds
  one of `admin` (edit + roles/rolebindings + delete the project), `edit`
  (write workloads, read secrets, exec/attach/port-forward, VM start/stop)
  or `view` (read all but secrets).

**Secrets**: `stringData` is folded into `data` (base64, overwriting a
same-named key) on create, PUT, PATCH and from startup manifests, and never
stored or returned, as upstream; Secrets stored with it before are folded at
boot (#101).

**Quantities** are strings (#292): a resource quantity sent as a JSON number
(`cpu: 1` in unquoted YAML) is stored and returned as upstream's canonical
string (`"1"`, `0.5` → `"500m"`, `1000` → `"1k"`), as kube-apiserver's
`resource.Quantity` does; typed clients (k8s-openapi, kube-rs) refuse a
number. It applies to every write of the built-ins that carry quantities
(Pods and pod templates, Nodes, PVCs/PVs, ResourceQuota, LimitRange,
RuntimeClass, HPA targets, CSIStorageCapacity, DRA capacity) — from clients,
controllers, startup manifests and webhook patches — and objects stored with
a number before are rewritten at boot. Quantity strings are kept as written
(upstream also re-canonicalizes those, `1000m` → `1`; rustkube does not).
Custom resources and CRDs keep their numbers, and so does a DRA driver's
opaque data (`opaque.parameters`, a claim's `status.devices[].data`) and any
integer that is not a quantity, such as a device request's `count` (#367).
Every reader of a quantity takes the canonical forms back (`"1k"` CPUs is
1000 CPUs, #365).

**Admission**, on create: NamespaceLifecycle (a missing namespace is 404;
a Terminating one is 403 with upstream's `NamespaceTerminating` cause, which
client-go's event recorder drops quietly, #282), namespace defaults (`Active`,
the `kubernetes` finalizer), Service port defaults and ClusterIP allocation
from `--service-cidr` (each address an atomic claim key; a dynamic one is a
random free address from the watch cache's view of the claims, above the
bottom band upstream keeps for fixed addresses — `min(max(16, size/16),
256)` — until that is full, so a create is one claim whatever the number of
Services, #113) and NodePort allocation from `--service-node-port-range` (#132:
a port per Service port for `NodePort`, and `LoadBalancer` unless
`allocateLoadBalancerNodePorts: false`; a named port kept, or 422 when taken
or out of range; one number may serve TCP and UDP; each port an atomic claim
key like the addresses; a `ClusterIP`/`ExternalName` Service naming one is
422). **On a Service update** the allocations follow the type, as upstream:
a ClusterIP or node port the body leaves empty keeps the stored one, a set
ClusterIP never changes (422), a change to `ClusterIP` drops and frees the
node ports, to `ExternalName` the ClusterIP too, and from `ExternalName` a
ClusterIP (and node ports) are allocated; new claims are given back if the
write fails, freed ones released once it lands. Delete frees both. Then the
pod's default ServiceAccount and its projected
`kube-api-access-*` token volume (unless the pod or ServiceAccount sets
`automountServiceAccountToken: false`), the not-ready /
unreachable tolerations, priority from its PriorityClass, OpenShift's
security context constraints (#321, below), a subset of
PodSecurity keyed on the namespace's `pod-security.kubernetes.io/enforce`
label — on create, and on a significant pod update (an image, a container
added, an ephemeral container, a seccomp annotation) against the
namespace's current level, as upstream; subresource writes are not checked,
#361 — CronJob schedule validation, PVC access-mode validation
(`ReadWriteOncePod` may not be combined with another mode), ConfigMap and
Secret data-key validation, pod sysctl-name validation, a pod's
`status.qosClass` (a container with a limit and no request gets the limit as
its request first, as upstream defaults it), ResourceQuota (#124: a create checked against every quota of its namespace
whose scopes cover it — 403 `exceeded quota`, `must specify <resource>`, or
`status unknown` before the controller's first pass — and charged to
`status.used` by compare-and-swap, so concurrent creates cannot overrun it),
LimitRanger (#131: a
namespace's LimitRanges fill `defaultRequest`/`default` into containers that
set none and annotate `kubernetes.io/limit-ranger`, then refuse — 403 in
upstream's words — a Container, Pod or PersistentVolumeClaim outside
`min`/`max`/`maxLimitRequestRatio`; on create only), and `status.phase: Pending` for a new Pod,
PersistentVolumeClaim or PersistentVolume. **On update**: immutable
ConfigMaps/Secrets and PriorityClass `value`; a PersistentVolumeClaim's
spec is immutable but for growing `resources.requests.storage` on a Bound
claim whose StorageClass has `allowVolumeExpansion` (403 otherwise; shrinking
only back to `status.capacity`) — volume expansion, docs/storage.md. A
client's update of a Pod (PUT, PATCH, apply, and again after mutating
webhooks) changes only what upstream's pod validation allows: container and
init-container `image`, `activeDeadlineSeconds` set or lowered, tolerations
added, scheduling gates removed, `nodeName` set where there was none; a
container's `resources`/`resizePolicy` only through `pods/resize`; the
`kubernetes.io/config.mirror` annotation never — 422 `spec: Forbidden: pod
updates may not change fields …` otherwise, so the security context SCC
and PodSecurity admitted on create stays (#350).

**Security context constraints, restricted-v2 (#321).** OpenShift manifests —
no `runAsUser`, no `fsGroup`, no root — run unchanged. Every Namespace gets
`openshift.io/sa.scc.uid-range` and `openshift.io/sa.scc.supplemental-groups`
(`<start>/10000`, the same block for both, from OpenShift's pool
1000000000–1999999999) when it is created, at boot if it was stored without
them, or at its first pod's admission; blocks come from a counter at
`/registry/rangeallocations/scc-uid` (compare-and-swap, never reused, resumed
after the highest block a namespace holds if the record is missing). A create
of a namespace name already taken is refused before a block is taken, and a
block taken for a create that then fails with a 4xx (an admission webhook's
refusal, the name taken in a race) is given back — it was never on a
namespace — and handed out first; a 5xx, which may have written it, keeps it
(#377). A
namespace created with valid ranges keeps them. A pod created in a governed
namespace — every namespace except `kube-*`, `openshift*`, `storm-*` and
`*-system`, unless its `pod-security.kubernetes.io/enforce` label says
`restricted`/`baseline` (governed) or `privileged` (not) — is admitted by
OpenShift's SCC admission over the stored SecurityContextConstraints (#373,
ported from apiserver-library-go): each SCC becomes a provider for the
namespace (a `MustRunAsRange` without a range takes the namespace's UID block,
an `fsGroup` `MustRunAs` its first group, `supplementalGroups` `MustRunAs`
its blocks); they are tried by `priority` (null is 0), then OpenShift's
restrictiveness points, then name; one is usable when it lists the pod's
ServiceAccount or the requester (or one of their groups) in `users`/`groups`,
or RBAC grants `use` on `security.openshift.io` `securitycontextconstraints`
by name — the ServiceAccount is asked first — and the first usable one the
pod satisfies admits it, with what it fills in. A pod annotated
`openshift.io/required-scc` is tried against that SCC alone. While one of the
three built-ins is missing, pods are refused (`… cache is missing anyuid`),
as OpenShift refuses while its standard set is incomplete. The built-ins,
created at boot when missing and otherwise left as an administrator made them
(`release.openshift.io/create-only`): `anyuid` (priority 10, any UID, drops
`MKNOD`), `restricted-v2` (granted to `system:authenticated` by the
create-only ClusterRoleBinding `system:openshift:scc:restricted-v2`) and
`privileged` (its `users`/`groups` name `system:admin`, `system:masters`,
`system:nodes`, `system:cluster-admins`). `restricted-v2` sets what the pod
leaves unset — each container's `runAsUser` (the range's first UID) and
`runAsNonRoot: true`, `allowPrivilegeEscalation: false`,
`capabilities.drop: [ALL]`; the pod's `fsGroup` (the first supplemental group,
the only one allowed) and `seccompProfile: RuntimeDefault` with OpenShift's
`seccomp.security.alpha.kubernetes.io/pod: runtime/default` annotation — and
refuses a UID outside the range, privileged containers, added capabilities
other than `NET_BIND_SERVICE`, host network/PID/IPC, host ports, unsafe
sysctls, other seccomp profiles and volume types other than configMap, csi,
downwardAPI, emptyDir, ephemeral, image, persistentVolumeClaim, projected and
secret, in OpenShift's words and field paths: 403 `pods "x" is forbidden:
unable to validate against any security context constraint: [provider
"anyuid": Forbidden: not usable by user or serviceaccount, provider
restricted-v2: .containers[0].runAsUser: Invalid value: 0: must be in the
ranges: [1000680000, 1000689999], …]`. The pod is annotated `openshift.io/scc`
and `security.openshift.io/validated-scc-subject-type` (`serviceaccount` or
`user`). So a pod a controller creates for a Deployment gets `restricted-v2`,
while one `cluster-admin` or `system:masters` creates directly is admitted
under `anyuid`, as on OpenShift. More than restricted-v2 for a
ServiceAccount: a RoleBinding to the bootstrapped ClusterRole
`system:openshift:scc:anyuid` (or `…:privileged`), or an SCC naming it. A
mirror pod its node creates (`kubernetes.io/config.mirror`, requester
`system:node:<n>` in `system:nodes`, `spec.nodeName` `<n>`) and the
apiserver's own writes (manifests) are not governed — the annotation from
anyone else exempts nothing (#350). SELinux levels are not assigned (no
`openshift.io/sa.scc.mcs`): a `MustRunAs` SELinux strategy is enforced only
when the SCC names a level itself. The kubelet applying `fsGroup` to volumes
is rustkube-node#171.

**SecurityContextConstraints and the PodSecurityPolicy reviews (#373).**
`security.openshift.io/v1` serves `securitycontextconstraints` (short name
`scc`, cluster-scoped, every verb): written SCCs get OpenShift's defaulting
(`RunAsAny` group strategies, `users`/`groups` `[]`,
`allowPrivilegeEscalation: true`, the volume list resolved — `*` without
`allowHostDirVolumePlugin` is every type but `hostPath`) and validation (422
for a negative priority, an unknown strategy, a capability both allowed and
required-dropped, `none` beside other volumes, overlapping sysctl patterns, …);
`oc get scc` prints the SCC CRD's columns (PRIV, CAPS, SELINUX, RUNASUSER,
FSGROUP, SUPGROUP, PRIORITY, READONLYROOTFS, VOLUMES). The three reviews are
served namespaced, create only, answered by the same providers:
`podsecuritypolicysubjectreviews` (for `spec.user` and `spec.groups` as given,
plus the template's ServiceAccount — `oc adm policy scc-subject-review -u|-z`),
`podsecuritypolicyselfsubjectreviews` (the requester — `scc-subject-review`
without `-u`) and `podsecuritypolicyreviews` (each ServiceAccount named, else
the template's, else `default`, each of which must exist; every SCC that
admits the template for that ServiceAccount alone — `scc-review -z`). OpenShift's
quirks are kept: a ServiceAccount alone is not in `system:authenticated`, so
`restricted-v2` does not admit for it in a review; a subject review keeps
`reason: CantAssignSecurityContextConstraintProvider` from an SCC that refused
before the one that allowed; `status.template` carries the assigned spec only
when the template names a ServiceAccount. `admin` may create the reviews.
OpenShift's `rangeallocations` are not served.

**Admission webhooks** (#82) run after the built-in admission, on create
(built-in and custom resources, server-side apply's upsert, `pods/eviction`),
update (PUT, PATCH, and every `/status` write as subresource `status`), delete
and each object of a deletecollection: mutating `MutatingWebhookConfiguration`
webhooks in configuration-name order, then `ValidatingWebhookConfiguration`
webhooks in parallel. Honoured: `rules` (operations, groups, versions,
resources with subresources and wildcards, scope), `namespaceSelector`,
`objectSelector`, `failurePolicy` (default `Fail`: a call that fails is a 500
"failed calling webhook"), `timeoutSeconds` (default 10, 1–30), `sideEffects`
against a dry-run delete, `reinvocationPolicy: IfNeeded`, JSONPatch, and the
webhook's `warnings` as `Warning` headers; a refusal is upstream's "admission
webhook \"…\" denied the request: …" with the webhook's code (≥ 400). The
review carries the requesting user and groups, kind, resource, name,
namespace, object and oldObject. A webhook is reached at `clientConfig.url`,
or at a `service` through its ClusterIP with TLS verified for
`<name>.<namespace>.svc` against `caBundle` (so the apiserver's host must
reach ClusterIPs, as upstream's default). Not honoured: CEL `matchConditions`
(the webhook is called as if they matched, #217), AdmissionReview `v1beta1`, and
client certificates to the webhook. Not admitted: `admissionregistration.k8s.io`
objects (upstream exempts them), `events.k8s.io` writes, the kubevirt
start/stop/restart and migrate verbs, ProjectRequest, TokenRequest and
`namespaces/finalize`. Configurations are read from the watch cache, so one
applies within milliseconds of being stored.

**At boot**, idempotently: waits for the datastore, for as long as it takes
(ERROR once a minute; nothing is served until it answers, #293); creates the `default`,
`kube-system`, `kube-public` and `kube-node-lease` namespaces and backfills
older namespaces' phase and finalizer; migrates pre-#76 custom-resource keys;
creates the bootstrap RBAC (below); registers itself in the `default/kubernetes`
Service and Endpoints; then applies `--manifest-dir` (any built-in a client
can create and the API stores as written — LimitRange, ResourceQuota, PDB,
PriorityClass, … — or a custom resource, #158). A manifest is admitted
as the same object would be through the API (#158): a create runs the
built-in admission (namespace lifecycle and defaults, Service ClusterIP and
node ports, the Pod defaults, LimitRanger, QoS, PodSecurity, the
validations) or the CRD's strategy (schema defaults, `generation`), and a
`Reconcile` update runs the update checks of PUT/PATCH (immutable fields,
PVC and Service rules; a Namespace keeps its `spec.finalizers`, which only
`/finalize` changes, as upstream) and is not written when the result equals
the stored object (#290); a refused manifest is logged (ERROR) and skipped. A
manifest the datastore could not read or write, or whose kind is not served
yet, is retried in the background in filename order (1 s doubling to 30 s)
until it applies — a kind still unserved after 10 min is given up at ERROR;
the pass's summary line names the failures by cause (#293). A document in a
Namespace that is waiting for a retry is not attempted but retried behind it
(#368), so the namespace's absence is not taken for a refusal. Not RBAC,
webhooks or the requester stamp: these are the apiserver's own writes.
**What left the directory is pruned (#236)**, as upstream's addon-manager
does: the applier records the `Reconcile` documents it applied in ConfigMap
`kube-system/rustkube-manifests` (the directory and each object's
apiVersion/kind/namespace/name), and at the next start deletes those no file
names any more — a removed file and a dangling symlink alike (stormcos's
other network flavour, #261) — logging `manifests: pruned <kind> <ns>/<name>`.
Only while the stored object still says `Reconcile` (an operator who sets
`EnsureExists` keeps it); `EnsureExists` objects and objects the applier
never recorded (applied before this, or made by others) are never pruned.
The delete is a plain DELETE's: finalizers set `deletionTimestamp`, a
Namespace goes Terminating, a Service gives back its addresses, dependents go
by the GC. Pruning waits until every document in the directory applied
(#368): at the end of the pass, or once its background retries have all
landed; a refused document, or a retry given up, prunes nothing that start
(the inventory keeps the old objects beside the new), so a network-flavour
switch whose new DaemonSet did not apply keeps the old one. Nothing is pruned
when a file cannot be read or parsed, or the directory is absent, cannot be
listed, or holds only dangling manifest links (its pallet not mounted); an
existing directory whose last manifest was removed prunes everything recorded
(#378). The record is the cluster's, not the path's: a moved `--manifest-dir`
keeps it, and prunes what the old directory named and the new one does not
(#378). A prune the datastore refused is owed to the next start.
Each boot phase logs its duration as it ends (`boot phase datastore_wait:
33.7s` — connect, datastore_wait, backfill, bootstrap, crd_load, manifests,
claims, auth, tls), and one summary line when the server starts accepting
(`boot: serving 102.0s after the process started (…)`, #290).

Bootstrap RBAC: `cluster-admin`; `system:masters` and `system:nodes` bound
to it (a Node authorizer for kubelets is #228);
`system:kube-controller-manager` and `system:kube-scheduler` bound to their
own ClusterRoles of those names (#176), reconciled at every boot, and a
binding left on `cluster-admin` by an earlier release repointed:
the controller-manager reads, patches, updates and deletes everything (the
garbage collector and namespace deletion walk everything) but creates only
what its controllers create, and holds no token requests, RBAC creates,
`bind`, `escalate` or `impersonate`; the scheduler reads what placement needs
and writes Pods (the bind), `pods/status`, claims, VMI and migration status,
its Lease and Events. `pods/exec`, `attach` and `portforward` are authorized
as `create` whatever the method (upstream ≥ 1.31);
`system:node-bootstrapper` (CSR create) for `system:bootstrappers` —
narrowed by CSR admission (#264) to `storm.io/forge-node` CSRs and reading
only its own (GET, or a WATCH of its name) — except that one in
`system:bootstrappers:stormcentral` may also file a
`kubernetes.io/kube-apiserver-client` CSR for exactly `CN=stormcentral`
(`O` only `stormcentral:forge`, no SANs, client usages; #388);
`system:bootstrap-signer-clusterinfo` (kube-public Role + RoleBinding):
`system:anonymous` may `get` the ConfigMap `cluster-info`, nothing else (#388); `system:storm:forge-node-signer`
(reconciled, **unbound**: stormcos binds stormcert to it) — CSRs read,
`/approval` and `/status`, `approve`/`sign` on `storm.io/forge-node`,
`machines.storm.io`, `enrollmentpolicies.storm.io` and creating those CRDs.
A CSR's `status` is emptied on create (a forged `Approved` is never signed,
#350), and a server-side apply that creates an object refuses a body naming
another name or namespace than its URL (400, #357). A CSR's
`spec.username`/`groups` are the requester's on create, whatever the
body says, and `spec` never changes after; `/approval` needs `approve` and
writing `status.certificate` needs `sign` on `signers/<signerName>` (or
`<domain>/*`), as upstream;
`system:basic-user` (self-reviews) for `system:authenticated`;
`system:discovery` for `system:anonymous`; the project roles `admin`,
`edit`, `view`, `basic-user` (list one's projects) and `self-provisioner`,
the latter two bound to `system:authenticated`; the registry roles (#325),
as OpenShift's, bound per project with
`oc adm policy add-role-to-user <role> <user> -n <project>`:
`registry-viewer` (read image streams and images, `get`
`imagestreams/layers` = pull), `registry-editor` (everything on them,
`update` `imagestreams/layers` = push), `system:image-puller` (pull only, for
another project's service accounts), `system:image-pusher` and
`system:registry` (a node registry's own identity in its project), over
`image.openshift.io` and its legacy core-group spelling, and over the
registry's own object, the namespaced CRD `registry.storm.io/v1 Image`
(#333): `registry-viewer` and `view` read `images`, `registry-editor` also
creates, changes and deletes them, and `system:registry` reads, creates and
changes them and writes `images/status` (each node its own `status.nodes`
line) but deletes none; the CRD and the `kube-system/registry` binding come
from stormcos. `view` reads image
streams but pulls nothing; `edit` (so `admin`) holds every registry role, so a
project admin may bind them. A node's registry checks a login with a
TokenReview and each pull or push with a SubjectAccessReview in the
repository's project (stormblock-registry#188); and the ServiceAccount
`kube-system/node-admin` bound to `cluster-admin` for a node's ssh login
(#79). The project roles are reconciled at every boot — a stored copy's
rules are brought up to date unless it is annotated
`rbac.authorization.kubernetes.io/autoupdate: "false"`; everything else is
created once. With `--dev-anonymous-admin` it also binds `system:anonymous` to
`cluster-admin`; without it, a binding left from an earlier boot is removed.

## What the controller manager runs

One process, leader-elected on the Lease `kube-system/kube-controller-manager`.
Collection LIST/WATCH streams enqueue deduplicated work as
state changes. Controllers run concurrently on Tokio; a change during a
reconcile queues another pass. Successful idle passes have no poll interval:
watch heartbeats and routine reconnects wake nothing — a watch reaching the
reflector's own 330 s deadline resumes from its revision, synchronized, with
no warning (`test/e2e/watch-deadline.sh`, #207) — and an idle control
plane makes no API requests (`test/e2e/deadlines.sh`). The KubeVirt
controllers (VirtualMachine, VMI launcher, VMI migration) run only while
their CRDs are Established, followed from the shared CRD feed: on a cluster
without KubeVirt none runs and nothing polls the unserved API; each starts
when its CRDs are installed and stops when one is deleted (#172).
Timers remain for semantic deadlines (cron, heartbeat expiry, backoff, job/VM
and migration deadlines, Event TTL), API recovery and leader Leases.

All controller families use bounded per-object workers with shared LIST/WATCH
feeds and inverse dependency indexes. Most pools allow eight distinct keys;
the PV binder's claim workers run eight at once too, with only the choice of
an unbound volume serialized (#147); discovered GC collections use two.
Successful writes remain visible locally until observed by the watch or a
later consistent snapshot. Failed feeds block dependent workers. Ambiguous
creates retain their original name until resolved; destructive actions carry
observed UID/revision preconditions. GC and namespace finalization retain
authoritative absence checks before destructive cleanup.
Every controller/scheduler mutation checks a monotonic leadership deadline;
controller-manager renewal follows the same 10 s retry deadline as the
scheduler's.
See [the event-driven design](docs/event-driven-design.md) for the execution
model and verified unit/API-rig cases. The multi-master failure matrix that
fits on one host passes (`test/e2e/multi-master.sh`, #149: three members,
three apiservers, paused/killed/partitioned leaders, quorum loss); live scale,
latency, separate-machine multi-master and runtime/storage acceptance remain
tracked in #147 and with the hardware (#162). Safe snapshots require
the datastore correction in **fastetcd v1.6.1**
([fastetcd#50](https://github.com/glennswest/fastetcd/issues/50)), or an
equivalent correct etcd implementation. The pinned API rig verifies
concurrent complete/paginated LISTs and exact WATCH replay after them; this
does not upgrade any deployed datastore. The historical intermittent rig
failures #153/#154 were closed unreproduced.

Pods made by the workload controllers (ReplicaSet, ReplicationController,
StatefulSet, DaemonSet, Job) carry their template's labels, annotations and
finalizers, as upstream's `GetPodFromTemplate` (#256) — so
`kubectl.kubernetes.io/restartedAt` from `rollout restart`, scrape hints and
an operator's own template markers are on the pod.

Deployment (rolling updates; old ReplicaSets scaled down by upstream's
`reconcileOldReplicaSets` arithmetic, so `maxUnavailable` lets an old pod
go while the surge pod is still pending, #266), ReplicaSet (a refused Pod
create or delete sets `ReplicaFailure` True with reason `FailedCreate` /
`FailedDelete` and the API's message, kept until a pass is not refused or the
count is reached, as upstream; RCs too, #125; each pass first claims its
Pods as upstream's `ClaimPods`: a matching orphan Pod not being deleted is
adopted, after an uncached read of the owner, and a Pod it controls whose
labels stopped matching is released and replaced, #295), ReplicationController (the
ReplicaSet reconcile over core/v1, Pods owned by kind `ReplicationController`;
create defaults selector, labels and replicas from the template; `/scale`
served, #125), StatefulSet (`OrderedReady`: the lowest missing ordinal is
made once every lower Pod is Ready, the highest beyond `replicas` removed
when none is missing, a finished Pod deleted and made again, #363), DaemonSet (every
eligible node, Ready or not; places pods itself; each pod labelled
`controller-revision-hash` — the template's FNV hash — and
`pod-template-generation`. Under `updateStrategy` `RollingUpdate`, the default,
a pod of an older template, or one with no hash label, is replaced as
upstream's `rollingUpdate`: without `maxSurge` at most `maxUnavailable`
(default 1; a percentage of eligible nodes, rounded up) nodes are without a
Ready pod at once, and an old pod not Ready is replaced at once; with
`maxSurge` the new pod starts beside the old, which is deleted once the new is
Ready. `OnDelete` leaves old pods until they are deleted.
`updatedNumberScheduled` counts the nodes running a current-template pod; a
pod counts as available once Ready — `minReadySeconds` is not read, #379; #369), Job (one whose `spec.managedBy` names another controller is left to it, #285; the field is validated and immutable), CronJob (a run's Job is named `<cronjob>-<unix minutes of the scheduled time>`, as upstream; `lastScheduleTime` advances only once that Job is created, or already exists owned by the CronJob — a failed create fails the pass and is retried, #364), ephemeral volumes (a Pod's generic ephemeral volume gets its
`<pod>-<volume>` PVC from `volumeClaimTemplate`, owned by the Pod; a claim of
that name the Pod does not own is left alone with a Warning Event, #94),
ResourceQuota (`status.hard` and `status.used` per quota from
its namespace's objects — Pods' compute and counts, Services, Secrets,
ConfigMaps, PVCs and per-class storage, RCs, quotas, `count/<resource>` —
honouring scopes; recomputed on change and every 5 minutes, #124), Service (Endpoints and EndpointSlices from the selected Pods, as upstream:
a named `targetPort` resolved per Pod against its container ports and
protocol — a Pod without it backs no such port — one Endpoints subset and
one EndpointSlice per resolved port set; a Pod not Ready is a
`notReadyAddresses` entry and `ready: false` unless
`publishNotReadyAddresses`; a terminating one is in the slices only, as
`terminating`; #362; for a Service with no
selector, EndpointSlices mirrored from its hand-written Endpoints, as
upstream's EndpointSliceMirroring does — one per address type and port set,
owned by the Endpoints, `managed-by: endpointslicemirroring-controller.k8s.io`,
skipped for `endpointslice.kubernetes.io/skip-mirror: "true"`, which the
controller's own Endpoints carry, and so do the apiserver's `default/kubernetes`
Endpoints — it writes that Service's slice itself, #272; a mirrored slice
wakes its Service by its `kubernetes.io/service-name` label, never as the
Endpoints' owner, #133), Namespace (default
ServiceAccount, deletion cascade — Pods first, and nothing else while a Pod is left, as upstream's ordered namespace deletion, #285), node lifecycle (a Lease stale or
missing past 40 s — also one that died before the controller started; with
no Lease, the Ready heartbeat — sets every condition `Unknown`, keeping
`lastHeartbeatTime`, and the `unreachable` NoSchedule/NoExecute taints;
a fresh heartbeat with Ready=False gets `not-ready`; Ready=True clears both;
pods are evicted when their toleration, 300 s by default, runs out; #267), PodDisruptionBudget status (a pod an eviction recorded in
`disruptedPods` counts as not healthy until it goes or two minutes pass;
terminating pods are not healthy, #360), garbage collection
(background, foreground and orphan, driven by discovery), PersistentVolume
(binding, phases, protection finalizers, reclaim), attach/detach
(VolumeAttachment), the stormblock provisioner for the in-kubelet blank-clone
path (claims whose StorageClass's provisioner is `stormblock.storm.io` or
`stormblock.storm.io/in-kubelet`, #92; the PV it writes takes the claim's
`volumeMode`, #201; docs/storage.md), the root CA publisher (`kube-root-ca.crt` in every namespace), the bootstrap signer (`kube-public/cluster-info`: a kubeconfig naming `--apiserver` and that CA bundle, plus `jws-kubeconfig-<id>` — a detached HS256 JWS over it, as kubeadm verifies — for each bootstrap token with `usage-bootstrap-signing: "true"`, not expired; runs when the root CA publisher does; #388), CSR approval and signing (auto-approves only a node renewing its own
`kubernetes.io/kube-apiserver-client-kubelet` certificate — requested by
`system:node:<n>` in `system:nodes` for exactly `CN=system:node:<n>,
O=system:nodes`, no SANs, client-auth usages; anything else waits for a person; signs only with
`--cluster-signing-*-file`; signs only upstream's `kubernetes.io/*` signers, never another signerName, and leaves `--csr-external-signer-names` to that signer, #199), PodMigration, and VirtualMachine
(`start`/`stop`/`restart`; a failed VMI is recreated with backoff under
`Always`/`RerunOnFailure`/`running: true` and left under `Once`/`Manual`,
the VM reading `CrashLoopBackOff` or `Failed` with the VMI's message on a
`Failure` condition, #104; a start the kubelet is retrying itself — VMI
`Pending` with `reason: FailedStart` — reads `CrashLoopBackOff` with that
attempt's message, and the VMI is left to the kubelet, #209; a template
edit on a running VM is written into its VMI when the node can apply it —
`cpu.sockets` up to the VMI's `maxSockets`, `memory.guest` up to `maxGuest`,
a `bridge` NIC on a non-pod network added or removed, a virtio/SCSI disk
added, a hot-added one removed — and anything else, or a change the kubelet
refused, is the condition `RestartRequired` naming the fields, #269), VMI launcher Pods (#203, below), and
VirtualMachineInstanceMigration (#184, below).
Events are emitted for creates, deletes and scaling, and expired ones are
deleted.

**VMI launcher Pods** (#203; the kubelet half is rustkube-node#88). A VMI
on the pod network — an interface whose network is `pod: {}`, unless a
`storm.io/bridge` or `storm.io/bridge.<interface>` annotation puts it on a
host bridge, as stormvm reads the spec — gets KubeVirt's `virt-launcher`
Pod once it has `status.nodeName`, because Cilium (endpoint identity, and so
NetworkPolicy) and Services read a Pod, not a VMI. The Pod is
`virt-launcher-<vmi>-<5 random>` in the VMI's namespace, carries the VMI's
labels plus `kubevirt.io: virt-launcher`, `kubevirt.io/created-by: <vmi uid>`
and `vm.kubevirt.io/name`, is owned by the VMI (`controller`,
`blockOwnerDeletion`: deleting the VMI deletes it), and has `spec.nodeName`
set (never scheduled separately), one placeholder container `compute` and no
resource requests (the VMI is charged; the Pod takes its node's pod slot).
Its status is the kubelet's: it adopts the Pod instead of running it, names
it in the CNI ADD, writes `phase`, `podIP`/`podIPs` and Ready, and confirms
its deletion once the machine is gone. One per VMI; a launcher deleted from
under a live VMI is replaced, one that ended is left. During a live
migration the target node gets its own, labelled
`kubevirt.io/migrationJobUID`, as upstream's target Pod; when the VMI moves
the source's is deleted, and when the migration fails the target's is. A
VMI that finished (`Failed`/`Succeeded`) gets nothing new.

**VMI live migration** (#184) is upstream KubeVirt's shape, coordinated
through the VMI's `status.migrationState`; the CRD
`virtualmachineinstancemigrations.kubevirt.io` ships with stormcos's KubeVirt
manifest (stormcos#288), and the transfer itself is the kubelets'
(rustkube-node#40). The controller claims the VMI (`migrationUid`,
`sourceNode`, `mode: PreCopy`) once it is `Running` on a node, one migration
per VMI at a time (a second waits in `Pending`); the scheduler writes
`targetNode`; the target kubelet writes `targetNodeAddress`; the source
kubelet writes `startTimestamp`, then `completed` or `failed`/`failureReason`.
The migration's phase follows: `Pending → Scheduling → Scheduled →
PreparingTarget → TargetReady → Running → Succeeded | Failed`, recorded in
`status.phaseTransitionTimestamps`, with the VMI's `migrationState` mirrored
into its own status. On success the controller moves the VMI's
`status.nodeName` (and a `kubevirt.io/nodeName` label, if it has one).
Scheduling unfinished after 5 min, or not sending 15 min after creation:
`Failed`, and the VMI's `migrationState` is marked failed so the target
tears down. A missing or stopped VMI fails it. Deleting an unfinished
migration aborts it (finalizer `kubevirt.io/migrationJobFinalize`): before
sending, the controller marks the VMI's migration failed with
`abortRequested`/`abortStatus: Succeeded`; while sending, it sets
`abortRequested` and waits up to 5 min for the source to answer. Progress
and timeouts of a running transfer are the source's.

**HPA** (#89) is upstream's algorithm on `metrics.k8s.io`, which the
apiserver serves from each node's cadvisor (below). Every 15 s and on a
target change it computes each `Resource` metric's count (`cpu`/`memory`,
`Utilization` of requests or `AverageValue`; none listed: cpu 80 %), with
upstream's 0.1 tolerance, missing Pods counted as 100 % on the way down and
0 on the way up, unready Pods as 0 for a cpu scale-up; takes the highest;
applies `spec.behavior` or upstream's default (up by max(100 %, 4 Pods) per
15 s; down 100 % per 15 s after 300 s of stabilisation) and min/max; and
writes the Deployment's, ReplicaSet's or StatefulSet's `spec.replicas`.
Status: `currentMetrics`, `AbleToScale`, `ScalingActive`, `ScalingLimited`.
Without pod metrics it changes nothing (`ScalingActive=False`,
`FailedGetResourceMetric`, the message naming why). `Pods`, `Object`,
`External` and `ContainerResource` metrics are not evaluated.

**`metrics.k8s.io/v1beta1`** (NodeMetrics, PodMetrics; `kubectl top`) is
served by the apiserver itself from each node's cadvisor
(`/api/v1.3/subcontainers/` at the node's InternalIP,
`--cadvisor-scheme`/`--cadvisor-port` 9096/`--cadvisor-ca-file`/
`--cadvisor-token-file`), cached 10 s per node: a node's usage is its root
cgroup's (CPU rate over the last two samples, memory working set), a Pod's
the sum of the containers carrying upstream's `io.kubernetes.pod.namespace`
/ `io.kubernetes.pod.name` / `io.kubernetes.container.name` labels.
containerd and CRI-O containers carry them; stormpump's do not until
cadvisor attributes them (cadvisor#3), so on stormcos today `kubectl top
node` works and a Pod has no metrics (404). The group is built in: an
APIService cannot take it over. `view` (and so `edit`/`admin`) may read pod
metrics. **Gateway API**
is status only, with no data plane (#70). It acts only on GatewayClasses
whose `controllerName` is `rustkube.io/gateway-controller`, their Gateways,
and its own entries in HTTPRoutes' `status.parents`; other controllers'
classes, Gateways and route entries are left alone (#91). Its Gateways are
`Accepted` and `Programmed=False` (`Pending`) with no `status.addresses`,
since nothing listens.

It has no ServiceAccount-token, TTL-after-finished or
node-IPAM controller. Serving a resource object does not implement its controller.

## What the scheduler does

Leader-elected on `kube-system/kube-scheduler`. Dependency watch events wake
placement of Pods with no `spec.nodeName` and unplaced VMIs. Placement remains
serialized through one priority-ordered Pod/VMI object queue. Incremental
accounting includes adopted workloads, successful binds and outstanding
volume/bind reservations before the next placement. A Pod's bind write (a
conditional PUT of `spec.nodeName`) is reserved first and then written in the
background, up to 16 at once, so the next Pod is placed without waiting for
it (#190). A bound Pod carries `storm.io/scheduled-at`, the bind time to the
microsecond; `PodScheduled`'s time is whole seconds, as upstream's is. Storage dependencies use
informer indexes. Lease renewal runs independently (every 2 s); a failed or
hung attempt is retried, and the term ends — cancelling the scheduling worker
and its reservations — once no renewal has succeeded for 10 s from its start
(upstream `renewDeadline`), before a standby can take the 15 s lease. A new
term rebuilds reservations from bound Pods/VMIs. A Pod waiting on a missing
claim is charged no capacity and is not pinned to a node until its claim
exists.

- **Filters:** node Ready, not unschedulable, taints/tolerations,
  `nodeSelector`, required node affinity (which is how `kubernetes.io/arch`
  is enforced), `nodeName`, inter-pod affinity and anti-affinity, topology
  spread (`DoNotSchedule`), resource fit (pod-level requests replace the
  container total for the resources they name — cpu, memory, hugepages —
  and only those, #73, #365; a regular container or sidecar is charged the
  largest of its spec request, its status `resources.requests` and its
  `allocatedResources`, status matched by name, so a shrink not yet
  actuated still holds the larger size, #365; quantities read as strings or
  JSON numbers;
  containers, init containers and sidecars added as upstream's PodRequests,
  #285 — a `restartPolicy: Always` init container adds to the pod, an
  ordinary one runs beside the sidecars before it; the same total for
  ResourceQuota usage and LimitRanger's Pod limits; every resource beyond
  cpu and memory — `ephemeral-storage`, `hugepages-*`, extended resources
  such as `nvidia.com/gpu` — is charged to the node too and refused as
  `Insufficient <name>` when it exceeds what is free, an extended resource
  the node does not advertise counting as 0 and `ephemeral-storage` checked
  only on a node that reports it, #297; preemption makes room by the same rule),
  Pod count: at most `allocatable.pods` (a quantity: `1k` is 1000, #365)
  non-terminal Pods per node, bound or
  with a bind in flight (#194); a VMI takes one through its launcher Pod
  (#203), not itself,
  and volume binding: PV node affinity, `CSIStorageCapacity`,
  `ReadWriteOncePod`, and `selected-node` for `WaitForFirstConsumer` claims.
- **Scores**, summed: least requested, image locality, preferred node
  affinity, preferred pod affinity, and topology spread (`ScheduleAnyway`).
- VirtualMachineInstances are scheduled too (#72), and so is a migrating
  VMI's target (#184): the same filters and scores, the source node
  excluded, written as `status.migrationState.targetNode`. The target is
  charged from when it is chosen until the migration fails, or succeeds and
  the VMI has been moved there — so for the whole migration the machine
  holds capacity on both nodes. No node for it: condition
  `TargetScheduled=False` (reason `Unschedulable`, each node's reason) on the
  migration, `True` once one is found.
- A Pod no node will take gets `PodScheduled=False`, reason `Unschedulable`,
  message as upstream's (`0/1 nodes are available: 1 Too many pods.`),
  written only when it changes, with a `FailedScheduling` Warning Event of
  the same message at the same moments (so one per distinct reason, not per
  retry). A bind records a `Scheduled` Event, "Successfully assigned
  ns/pod to node". Both come from `default-scheduler` (#138).

**Preemption** (#84), as upstream's DefaultPreemption: a Pod that fits on no
node (and has no `preemptionPolicy: Never`) looks for the node where removing
lower-priority Pods would let it pass every filter; victims are reprieved,
PodDisruptionBudget-protected ones first and highest priority first, while it
still fits; the node chosen breaks the fewest budgets, then has the lowest
highest-victim priority, lowest priority sum and fewest victims. The Pod is
nominated there (`status.nominatedNodeName`, which holds the room against
Pods of no higher priority), the victims are evicted through the Eviction API
(so a budget can still refuse one) with a `Preempted` Event, and the Pod binds
once they are gone. A nominated Pod whose victims are still terminating waits
rather than preempting again; binding clears `nominatedNodeName`. A Pod with a non-empty `spec.schedulingGates` is not placed: it reads
`PodScheduled=False/SchedulingGated` ("Scheduling is blocked due to
non-empty scheduling gates", no Event) and is placed once the last gate is
removed; the apiserver refuses an update that adds a gate (422, "only
deletion is allowed", as upstream) (#87). There is no upstream
framework/profile parity. Pending keys
are ordered by priority and creation time; failed API operations back off,
and dependency events wake infeasible keys. `plugins.rs` defines plugin
traits the loop does not use.

## Configuration

Every binary logs through `RUST_LOG` (default `info`). Configuration is CLI
flags plus the environment variables listed below; there is no TOML/YAML
configuration loader. Tables come from `cmd/*/src/main.rs`. Explicit flags
override environment values.

### kube-apiserver

| flag | env | default | |
|---|---|---|---|
| `--etcd-servers` | `ETCD_SERVERS` | **required** | fastetcd endpoints, comma-separated |
| `--etcd-cacert`, `--etcd-cert`, `--etcd-key` | `ETCD_CACERT`, `ETCD_CERT`, `ETCD_KEY` | — | TLS / mutual TLS to fastetcd |
| `--etcd-compaction-interval` | | `5m0s` | compact the datastore's history this often (Go duration, `0` off); one apiserver per interval, coordinated on `compact_rev_key` (#139) |
| `--bind-addr` | | `0.0.0.0` | |
| `--secure-port` | | `6443` | |
| `--tls-cert-file`, `--tls-private-key-file` | | — | serving cert; **reloaded when the files change**, no restart; a key that is not the cert's is refused (#93) |
| `--tls` | | off | serve a self-signed cert kept in `--data-dir` (`apiserver.crt`/`.key`, key 0600): written on first start, reused after a restart while valid (> 30 days left, key matches, SANs current), else regenerated; in memory, with a warning, if the dir cannot be written. DNS SANs `kubernetes`, `kubernetes.default`, `kubernetes.default.svc`, `kubernetes.default.svc.<cluster-domain>`, `localhost`; no IP SANs (#88) |
| `--insecure` | | `false` | allow plain HTTP when no TLS is configured; without it the server refuses to start |
| `--client-ca-file` | | — | enables x509 client-certificate authentication; waited for at start (120 s) until it is a complete PEM (#286); **reloaded when the file changes**, for new connections (#105) |
| `--client-crl-file` | | — | CRL, PEM or DER, one per signer (repeatable or comma-separated); a client certificate whose serial it lists is refused at the handshake. End-entity only; a CA no CRL covers is not refused for that; a CRL past nextUpdate still applies. Waited for at start (120 s) and must parse; re-read every 30 s, a missing or bad file keeps the last good one, logged. Needs `--client-ca-file` (#260, stormcert#61) |
| `--cadvisor-scheme`, `--cadvisor-port` | | `http`, `9096` | where each node's cadvisor answers, for `metrics.k8s.io` (#89) |
| `--cadvisor-ca-file`, `--cadvisor-token-file` | | — | CA for an https cadvisor, and the bearer token sent to it |
| `--proxy-client-cert-file`, `--proxy-client-key-file` | | — | the client certificate presented to aggregated API servers (#83); followed on disk |
| `--requestheader-client-ca-file` | | — | the CA of `--proxy-client-cert-file`, published in `kube-system/extension-apiserver-authentication` (#83) |
| `--requestheader-allowed-names` | | — | front-proxy certificate common names, comma-separated, published with it; empty: any |
| `--anonymous-auth` | | `true` | `false` answers 401 to requests without credentials; a rejected token is 401 either way (#115) |
| `--dev-anonymous-admin` | | `false` | **dev only**: anonymous is `cluster-admin` (needs `--anonymous-auth true`) |
| `--service-account-signing-key-file` | | — | private key (PEM; RSA → RS256, ECDSA P-256 → ES256) that signs tokens |
| `--service-account-key-file` | | — | public key file(s) tokens are verified against (SPKI PEM, RSA or P-256): repeatable or comma-separated, a file may hold several keys, a token signed by any of them verifies — rotate by adding the new public key, switching the signing key, then dropping the old one (#223). Both files are re-read every 30 s and a change applies without a restart; when the signing key itself was replaced (its tokens verify against none of the new public keys), its public key keeps verifying for 24 h after that reload and no longer; a public key removed while the signing key stays stops verifying at once, which is how a key is revoked (#371); each load logs the keys' `saPin`s and any replaced signing key's deadline (#298). Both flags are needed; with either absent the code falls back to an ephemeral HS256 key that dies with the process |
| `--token-auth-file` | | — | static bearer tokens, `token,user,uid[,"groups"]` per line; followed for changes, a missing file is no tokens (#188) |
| `--service-account-issuer` | | `https://kubernetes.default.svc` | `iss` of minted tokens; a token naming another issuer is refused (#182) |
| `--api-audiences` | | the issuer | audiences a token must be for to authenticate here, comma-separated; TokenRequest's default `aud` |
| `--service-account-extend-token-expiration` | | `true` | a pod-bound 3607 s TokenRequest gets a year with `warnafter` at 3607 s |
| `--advertise-address` | | `--bind-addr` if concrete | the address put in `default/kubernetes` Endpoints |
| `--service-cidr` | | `10.96.0.0/12` | ClusterIP range; `.1` is the `kubernetes` Service |
| `--service-node-port-range` | | `30000-32767` | NodePort range, inclusive (#132) |
| `--manifest-dir` | `MANIFEST_DIR` | — | YAML/JSON applied once at start, in filename order, through the built-in admission (#158); created if absent, overwritten if labelled (upstream's place, #299) or annotated `addonmanager.kubernetes.io/mode: Reconcile`, and pruned at a later start once no file names it and every document applied (#236, #368) — an existing empty directory prunes all, an absent one or one of dangling links only prunes nothing, and a moved directory keeps the record (#378); what the datastore refused retried until it applies (#293) |
| `--data-dir` | | `/var/lib/kubernetes` | where the `--tls` self-signed pair is kept (#88); nothing else |
| `--cluster-domain` | | `cluster.local` | the `--tls` certificate's `kubernetes.default.svc.<domain>` SAN (#88); nothing else |

### kube-controller-manager and kube-scheduler

| flag | env | default | |
|---|---|---|---|
| `--apiserver` | `APISERVER_URL` | `http://127.0.0.1:6443` | an `https://` URL turns on TLS |
| `--certificate-authority` | | — | CA bundle for the apiserver |
| `--client-certificate`, `--client-key` | | — | mutual TLS identity; **reloaded when the files change**, no restart; a key that is not the cert's is refused (#105) |
| `--token` | `APISERVER_TOKEN` | — | bearer token |
| `--token-file` | | — | bearer token from a file |
| `--insecure-skip-tls-verify` | | off | |
| `--leader-elect` | | `true` | |
| `--startup-timeout` | `STARTUP_TIMEOUT` | `120` | seconds to wait for credential files and for the apiserver |
| `--cluster-signing-cert-file`, `--cluster-signing-key-file` | | — | controller manager only: the CA the CSR controller signs with; without them CSRs are approved but not signed |
| `--csr-external-signer-names` | | — | controller manager only: signerNames an external signer (stormcert) handles; CSRs naming one are neither approved nor signed here (#199). Comma-separated or repeated |
| `--build-image` | | origin-docker-builder 5.0.0 (`quay.io/openshift/origin-docker-builder@sha256:a11e3b12…0572`) | controller manager only: OpenShift's builder image, which build pods run (#374) |
| `--build-insecure-registries` | | — | controller manager only: registries build pods reach over plain HTTP (a node registry, `host:5100`), written into each build's `registries.conf` (#374). Comma-separated or repeated |
| `--root-ca-file` | | `--certificate-authority` | controller manager only: the CA bundle published as `kube-root-ca.crt` in every namespace and in `kube-public/cluster-info`; with neither, nothing is published |
| `--tls-cert-file`, `--tls-private-key-file` | | — | serving pair for the metrics port (10257/10259): HTTPS only with it, plain HTTP without; renewed in place (#90) |
| `--authorization-always-allow-paths` | | `/healthz,/readyz,/livez` | metrics-port paths served without authorization; the rest need a bearer token the apiserver's TokenReview accepts and SubjectAccessReview allows (#90) |

`--token` (including `APISERVER_TOKEN`) takes precedence over `--token-file`.
Boolean value flags (`--leader-elect`, `--anonymous-auth`, `--insecure`,
`--dev-anonymous-admin`) take `true`/`false`; `--tls` and
`--insecure-skip-tls-verify` are presence switches.
Neither takes `--kubeconfig`. A credential file that does not exist yet is
waited for, not treated as an error, because the whole control plane starts
at once (#58).

## Ports and endpoints

| binary | port | protocol | paths |
|---|---|---|---|
| kube-apiserver | `--secure-port` (6443) | HTTPS; HTTP only with `--insecure true` and no TLS pair/`--tls` | the API; `/healthz`, `/livez`, `/readyz`, `/version`, `/metrics` |
| kube-controller-manager | 10257, fixed | HTTPS with `--tls-cert-file`, else plain HTTP, on `0.0.0.0` | `/metrics` (delegated authn/authz), `/healthz`, `/readyz`, `/livez` |
| kube-scheduler | 10259, fixed | the same | the same |

Every `/metrics` needs a principal allowed `get` on it (#90): the
apiserver's through its own authentication and RBAC (`system:monitoring`),
the controller manager's and scheduler's through the apiserver's
TokenReview and SubjectAccessReview. Without `--tls-cert-file` those two
still speak plain HTTP, so a scraper's token crosses the wire in clear. Metric names
follow upstream's; the list, and where they differ from upstream, is in
[docs/metrics.md](docs/metrics.md).

## Build and test

Builds and unit tests run through `sc-build` on a stormcentral build host,
never on a workstation and never as root. Push first, then:

```bash
sc-build                              # cargo build && cargo test, at the pushed commit
sc-build 'cargo test -p apiserver'    # any command at the repo root
```

`sc-build` fetches the pushed commit onto a private build volume as the
`stormbuild` user (checkout, target dir, HOME and TMPDIR all on it), runs the
command, and deletes the volume. A failing build is filed as a
`build-failure` issue here. `protoc` is needed (`pkg/apimachinery/build.rs`); the minimum Rust is 1.82 (`rust-version`, #354).

Release artifacts — static musl binaries (x86_64 and aarch64, #68),
tarballs and `FROM scratch` images from `deploy/build-release.sh` — are
described in [docs/releasing.md](docs/releasing.md); nothing in stormcos
consumes them. There is no GitHub workflow: it was removed and Actions is
disabled by owner decision (#114).

**On a node**, rustkube is tested by its test container, `test/`, which
stormcentral runs as a Job on every test machine per its test standard:
`stormcentral test run rustkube short|medium|long`. `short` (under two
minutes) proves the control plane is up and does its job; `medium` its API
semantics, controllers and GC end to end; `long` overnight waves with a
latency, memory and residue trend. See [test/README.md](test/README.md). The
same binary runs against a real apiserver, controller-manager and scheduler on
fastetcd in `test/e2e/test-container.sh`.

The `test/e2e/*.sh` rigs (each a real apiserver, controller-manager and
scheduler on a pinned fastetcd, many with stand-in kubelets) are suites of the
same test image, never run in a build slot (#173): `stormcentral test run
rustkube rigs --tag <machine> --commit <sha>` (the functional rigs, ≤ 30 min)
and `rigs-night` (the slow ones, on a pve VM at night). Which rig is in which
suite is `test/rigs.sh`.

## How it ships

In stormcos each binary is its own **stormd golden**: a container whose PID 1
is stormd, which runs the binary from a config baked into the golden. The
goldens are `rustkube-apiserver`, `rustkube-controller-manager` and
`rustkube-scheduler`, started by stormpump from `/etc/stormpump/boot.d/30-kube`
on profiles that enable the control plane. stormcos's `deploy/build-goldens.sh`
(its stage mode) builds the binaries **from source**, from a clean rustkube
checkout whose commit it records in the image manifest
(`cargo build --release --locked --target x86_64-unknown-linux-musl`), not
from a release.

The **component golden** — what `stormcentral component build rustkube`
produces, `golden-rustkube-<digest>`, and what a stormcos release request
names — carries the same three binaries, but no release mounts it yet:
breaking the binaries out of the composite stormd goldens is stormcos#62.
Until then a node runs whatever the stage build compiled into
`rustkube-apiserver` and its two siblings; their manifests name the commit.
The apiserver's `/version` (`v1.36.0-rustkube+<workspace version>`) tells
releases apart but not two unreleased commits of the same version.

The following is the **build configuration**, checked against stormcos
[`73091eac`](https://github.com/glennswest/stormcos/blob/73091eacb73cdbbb55a4530a48a8b5b0f360b699/deploy/build-goldens.sh) (2026-10-09),
not proof of the version installed on any node:

- **kube-apiserver:** `--etcd-servers https://127.0.0.1:2379` with mutual TLS
  (`--etcd-cacert /data/stormcert/ca.crt`, `--etcd-cert`/`--etcd-key`
  `/data/stormcert/apiserver-etcd-client.{crt,key}`), `--bind-addr 0.0.0.0`,
  `--advertise-address ${NODE_IP}`, the serving pair
  `/data/stormcert/apiserver.{crt,key}`, `--token-auth-file
  /state/config/token-auth.csv` (install-config's `apiToken`, #188), the
  ServiceAccount pair `/data/stormcert/sa-token.{key,pub}`,
  `--client-ca-file /data/stormcert/client-ca.crt` and `--client-crl-file
  /data/stormcert/client.crl` (written by `stormcert-forge-trust`, stormcos
  #572), and
  `--manifest-dir /etc/kubernetes/manifests.d` (`deploy/manifests` there:
  Cilium or flowsdn — linked into the network plugin's pallet, so only the
  active one's resolve — stormcoredns, the storage class and CSI driver, the
  VMI and snapshot CRDs, the platform services' CRDs, identities and RBAC; a
  manifest removed from the directory is not removed from the cluster,
  #236). The `sno` and `bastion` profiles add `--dev-anonymous-admin
  true`. Liveness is `https://127.0.0.1:6443/healthz`. Not passed: the
  cadvisor, aggregation (`--proxy-client-*`, `--requestheader-*`),
  compaction and NodePort flags, so their defaults apply; with no proxy
  client certificate an aggregated API's backend sees no client identity.
- **kube-controller-manager, kube-scheduler:**
  `--apiserver https://${NODE_IP}:6443 --certificate-authority /data/stormcert/ca.crt`,
  the client pair `/data/stormcert/kube-<component>.{crt,key}`
  (`controller-manager` or `scheduler`; CN `system:kube-<component>`, bound
  to rustkube's least-privilege roles of that name, #176), and the metrics
  port's serving pair `/data/stormcert/kube-<component>-serving.{crt,key}`,
  so 10257/10259 are HTTPS; ironprom scrapes them with the
  `kube-system/ironprom` token (stormcos#384). Liveness is
  `https://127.0.0.1:<port>/healthz`. No cluster-signing flags and no
  `--csr-external-signer-names` are supplied: CSRs are approved where the
  controller approves them, never signed, and publishing the root CA does
  not imply signing.

The files under `/data/stormcert` are written by stormcert-agent one-shots
in the apiserver golden before the apiserver starts: `apiserver.crt`/`.key`
(CN `apiserver`; SANs the `kubernetes…` names, `localhost`, `10.96.0.1`, the
node IP and `127.0.0.1`), `ca.crt`, `sa-token.key`/`.pub`, the client and
serving pairs above, `node-admin.token`, and ServiceAccount tokens for the
platform's own services. `stormcert-agent renew` runs beside the apiserver
and re-issues a pair past 80 % of its life; the apiserver and the two
controllers follow their files without a restart (#93, #105). Components wait
for their credential files at startup.

## Further reading

| | |
|---|---|
| [docs/presentation.md](docs/presentation.md) | a 12-slide overview (Marp): purpose, place in stormcos, what works, what is planned |
| [docs/certificates.md](docs/certificates.md) | TLS, reload, renewal, offline-minted tokens |
| [docs/storage.md](docs/storage.md) | who does what to a PVC — rustkube, stormblock, stormblock-csi |
| [docs/metrics.md](docs/metrics.md) | every metric and what it answers |
| [docs/scale.md](docs/scale.md) | planned control-plane scale measurements and execution prerequisites (#66) |
| [docs/conformance.md](docs/conformance.md) | running the Kubernetes conformance suite, and what it found |
| [docs/releasing.md](docs/releasing.md) | release artifacts |
| [docs/oc-compatibility.md](docs/oc-compatibility.md) | what `oc` can ask, and what is answered |
| [docs/upstream-feature-inventory.md](docs/upstream-feature-inventory.md) | upstream feature by feature |
| [docs/scheduler-research.md](docs/scheduler-research.md), [docs/scheduler-upstream.md](docs/scheduler-upstream.md), [docs/draining-multiarch-research.md](docs/draining-multiarch-research.md) | research on upstream — design input, not a description of this code |
| [CHANGELOG.md](CHANGELOG.md) | what changed, by release |

## License

Apache-2.0
