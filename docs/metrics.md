# Metrics

Every component serves Prometheus text format under **upstream Kubernetes'
metric names**. That is the whole design constraint: a dashboard, recording
rule or alert written against Kubernetes should work here unchanged, and an
equivalent metric under a different name is worth much less than the same
metric under the same name (#51).

Histograms carry `_bucket` series with upstream's buckets (#90); see
[Where this differs from upstream](#where-this-differs-from-upstream) for what
is still not upstream's shape.

| component | endpoint |
|---|---|
| kube-apiserver | `/metrics` on the API listener (`--bind-addr:--secure-port`): authenticated, and RBAC `get` on the non-resource URL `/metrics` (`system:monitoring` has it) |
| kube-controller-manager | `:10257/metrics`: HTTPS with `--tls-cert-file`/`--tls-private-key-file` (plain HTTP without), port fixed |
| kube-scheduler | `:10259/metrics`, the same |
| rustkube-node | separate repo — see the note at the end |

**Who may read them (#90).** The apiserver's `/metrics` is inside its
authentication and RBAC: a principal allowed `get` on `/metrics`. Bootstrap
reconciles upstream's `system:monitoring` ClusterRole (`get` on `/metrics`,
`/metrics/slis` and the health paths) bound to the group `system:monitoring`;
`system:masters` and cluster-admin may too. The controller manager and
scheduler delegate to the apiserver as upstream's do: a `/metrics` request
needs a bearer token the apiserver's TokenReview accepts (401 otherwise) and
whose user a SubjectAccessReview allows `get` on the path (403 otherwise);
answers are cached 10 s. `--authorization-always-allow-paths` (default
`/healthz,/readyz,/livez`; a trailing `*` is a prefix) are served to anyone.
Their serving pair is followed on disk, so a renewed certificate is used
without a restart; the files are waited for if not there yet.

The controller manager and scheduler answer `/healthz`, `/readyz` and
`/livez` (always `ok`) on the same port; the apiserver answers `/healthz`,
`/livez` and `/readyz`, which RBAC lets anyone read. A failed bind of
10257/10259 is logged as a warning and the component carries on without
metrics.

The exporter is shared (`apimachinery::metrics`); each component adds only
what is its own. It was three copies before, and they had drifted.

## Every component

`process_cpu_seconds_total`, `process_resident_memory_bytes`,
`process_virtual_memory_bytes`, `process_start_time_seconds`,
`process_open_fds`, `process_max_fds` — read from `/proc/self` **at scrape
time**, because a value sampled on a timer is stale by up to the timer.
Upstream gets these free from the Prometheus Go client, so every Kubernetes
dashboard assumes them, and nothing in Rust provides them.

`kubernetes_build_info{gitVersion,component,goVersion}` — `gitVersion` is the
crate version without a leading `v` (`0.18.0`), `goVersion` is `rustc`.

On a non-Linux build (a workstation) the `process_*` family is **absent**
rather than zero: a zero would be read as a fact.

## apiserver

```
apiserver_request_total{verb,group,version,resource,scope,code}
apiserver_request_duration_seconds{verb,group,version,resource,scope}
apiserver_write_phase_duration_seconds{phase}
apiserver_current_inflight_requests{request_kind="mutating"|"readOnly"}
etcd_request_duration_seconds{operation,type}
apiserver_storage_objects{resource}
apiserver_watch_events_total{resource,kind}
watch_cache_capacity{resource}
apiserver_certificate_expiration_seconds{name}
```

`verb` is the **Kubernetes** verb, not the HTTP method: a GET of a collection
is a `list`, a GET with `?watch=true` is a `watch`, a DELETE of a collection is
a `deletecollection`. "How many lists are we serving" is a question about load,
and it is unanswerable if every GET looks the same. `scope` is `cluster`,
`namespace` or `resource`. A subresource is labelled as one — `pods/exec`,
`nodes/status`.

Labels are derived from the path's shape, not from a table of known resource
names, so custom resources are visible too (they used to all land in `other`).

`etcd_request_duration_seconds` covers the fastetcd round trip for `get`,
`create`, `update` and `delete`. `list` is timed too: `ApiStorage::list` reads the datastore at the requested
snapshot revision on this branch, so it includes that store call. Watches
are not timed.

`apiserver_storage_objects` and `apiserver_watch_events_total` come from the
watch cache, which already holds the numbers — no extra LIST, no extra cost.
The consequences: a resource nobody has listed or watched since boot has no
series; `apiserver_storage_objects` is set only from a resource-wide cache (#90:
a namespace's cache used to overwrite the total with its share), so a
resource watched only per namespace has no series; `kind` is the event type (`ADDED`, `MODIFIED`, …); and
built-in resources are labelled `deployments`, not upstream's
`deployments.apps`. `watch_cache_capacity` is the constant 1024.

`apiserver_current_inflight_requests` counts a watch for its whole life.

## controller-manager

```
leader_election_master_status{name="kube-controller-manager"}
```

controller_reconcile_duration_seconds{controller}
controller_reconcile_errors_total{controller}
```

are recorded for every object reconcile the indexed workers run (#90): the
time of one pass for one object, and an error when the pass returned one or
an API call in it failed. `controller` is the worker's name (`deployments`,
`gatewayclasses`, …). Upstream has no such pair (its shape is `workqueue_*`),
so the names are rustkube's own.

```

`leader_election_master_status` is upstream's name and the one that matters:
it is 1 on the holder, so **two instances both reporting 1** is visible the
moment it happens rather than when they start fighting. The scheduler sets 0
before it tries to acquire, and the controller manager from the moment its
metrics port is up (#90), so a standby reads 0 rather than nothing.

There are **no `workqueue_*` metrics**. With the turbomode implementation, controllers have
deduplicated per-object queues and bounded workers, but queue depth, queue
delay and worker utilization are not instrumented (#90). Do not interpret
missing series as empty queues or idle workers.

## scheduler

```
leader_election_master_status{name="kube-scheduler"}
scheduler_pending_pods{queue="active"}
scheduler_pending_virtualmachines{queue="active"}
scheduler_schedule_attempts_total{result="scheduled"|"unschedulable"|"error"}
```

VMI outcomes are counted in `scheduler_schedule_attempts_total` too, and so
are migration targets (#184); `scheduler_pending_virtualmachines` includes
running VMIs whose migration waits for a target.
Successful Pod binds, failed bind writes (`error`), volume waits and Pods
with no feasible node (`unschedulable`, #194) increment the counter. An
unschedulable Pod is counted on every retry, so this is not a denominator for
success rates (#90).

`scheduler_e2e_scheduling_duration_seconds{result="scheduled"}` times each
bound Pod from when the scheduler first saw it pending to when its bind write
was acknowledged (#190). Only `scheduled` is recorded.

Upstream splits `scheduler_pending_pods` across `active`, `backoff` and
`unschedulable` queues. With the turbomode implementation, this gauge counts pending keys in the shared placement
state when workload informer events arrive. The queue includes priority ordering
and API retry deadlines, but the gauge does not split those states: it reports
only `active`.

A pod that is placed but waiting for its volumes to bind counts as
`unschedulable`, which is what it is until the volume exists.

### Write phases and slow requests (#191)

`apiserver_write_phase_duration_seconds{phase}` splits every guaranteed
update (PATCH, status PUT/PATCH, apply) into `read` (the datastore GET),
`mutate` (the patch or new body and built-in admission), `webhooks`
(admission webhooks), `write` (the datastore CAS) and `retry_wait` (pauses
between attempts that lost a race), each summed over the attempts. Upstream
has no such metric; it answers where a slow write went.

Two `warn` log lines go with it:

- `slow write` — a guaranteed update over 100 ms: the key, the attempts, and
  the total and each phase in ms;
- `slow request` — any request over 100 ms, with method, path, code and ms.
  Watches, logs, exec/attach/port-forward and proxies are left out: they are
  long by design.

A slow request whose `slow write` is short spent its time outside the
handler: authentication, RBAC, protobuf transcoding, or the client.

## Where this differs from upstream

- **Buckets** (#90): `apiserver_request_duration_seconds` and
  `etcd_request_duration_seconds` use upstream's 0.005 s … 60 s list,
  `scheduler_e2e_scheduling_duration_seconds` upstream's exponential 1 ms ×2
  (15 buckets), every other histogram client_golang's `DefBuckets`. None are
  summaries any more.
- **`process_cpu_seconds_total`** is typed `counter`, as client_golang's; the
  exporter records it as a gauge (its counters are integers) and the TYPE line
  is corrected on render.
- **10257/10259 speak plain HTTP** unless given `--tls-cert-file`; upstream
  generates a self-signed pair instead. Client certificates are not accepted
  for authentication there, only bearer tokens.
- **No `workqueue_*`** (see the controller-manager section).

## Where metrics do not live

Not in the datastore. Upstream keeps none of this in etcd; a number sampled
every fifteen seconds forever is the one kind of data a consensus store must
not be asked to hold. A scrape endpoint computes on request and keeps nothing.

## rustkube-node

The kubelet's two families — cAdvisor-shaped container metrics at
`/metrics/cadvisor` and the kubelet's own at `/metrics` — belong to the
`rustkube-node` repo, which serves both (rustkube-node#36, closed 2026-09-27;
its `docs/metrics.md` lists them). Container
restart counts are **not** a kubelet metric upstream:
`kube_pod_container_status_restarts_total` comes from kube-state-metrics,
derived from `pod.status.containerStatuses[].restartCount`. Whatever plays that
role here should derive it from the object too, or the two will disagree.
