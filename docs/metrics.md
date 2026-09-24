# Metrics

Every component serves Prometheus text format under **upstream Kubernetes'
metric names**. That is the whole design constraint: a dashboard, recording
rule or alert written against Kubernetes should work here unchanged, and an
equivalent metric under a different name is worth much less than the same
metric under the same name (#51).

That promise holds for **names** and not yet for **shapes**: see
[Where this differs from upstream](#where-this-differs-from-upstream).

| component | endpoint |
|---|---|
| kube-apiserver | `/metrics` on the API listener (`--bind-addr:--secure-port`), **unauthenticated** (#90) |
| kube-controller-manager | `http://0.0.0.0:10257/metrics`, plain HTTP, no auth, port fixed |
| kube-scheduler | `http://0.0.0.0:10259/metrics`, plain HTTP, no auth, port fixed |
| rustkube-node | separate repo — see the note at the end |

The controller manager and scheduler also answer `/healthz` (always `ok`) on
the same port; the apiserver answers `/healthz`, `/livez` and `/readyz`, which
RBAC lets anyone read. A failed bind of 10257/10259 is logged as a warning and
the component carries on without metrics.

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
crate version without a leading `v` (`0.14.1`), `goVersion` is `rustc`.

On a non-Linux build (a workstation) the `process_*` family is **absent**
rather than zero: a zero would be read as a fact.

## apiserver

```
apiserver_request_total{verb,group,version,resource,scope,code}
apiserver_request_duration_seconds{verb,group,version,resource,scope}
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
`create`, `update` and `delete`. `list` is timed too, but a list is answered
from the in-memory watch cache, so it measures that, not the store. Watches
are not timed.

`apiserver_storage_objects` and `apiserver_watch_events_total` come from the
watch cache, which already holds the numbers — no extra LIST, no extra cost.
The consequences: a resource nobody has listed or watched since boot has no
series; the cache is per requested prefix, so a namespaced and a cluster-wide
cache for the same resource share one `resource` label and the gauge shows
whichever fired last; `kind` is the event type (`ADDED`, `MODIFIED`, …); and
built-in resources are labelled `deployments`, not upstream's
`deployments.apps`. `watch_cache_capacity` is the constant 1024.

`apiserver_current_inflight_requests` counts a watch for its whole life.

## controller-manager

```
leader_election_master_status{name="kube-controller-manager"}
```

`controller_reconcile_duration_seconds{controller}` and
`controller_reconcile_errors_total{controller}` are declared but **never
emitted**: nothing calls `record_reconcile` (#90).

`leader_election_master_status` is upstream's name and the one that matters:
it is 1 on the holder, so **two instances both reporting 1** is visible the
moment it happens rather than when they start fighting. The scheduler sets 0
before it tries to acquire; the controller manager sets 0 only after losing
the lease, so a standby that has never led has no series.

There are deliberately **no `workqueue_*` metrics**. These controllers are poll
loops with no queue; exporting `workqueue_depth` as a constant zero would be a
number that reads as a fact. Where the shape differs from upstream, so does the
name.

## scheduler

```
leader_election_master_status{name="kube-scheduler"}
scheduler_pending_pods{queue="active"}
scheduler_pending_virtualmachines{queue="active"}
scheduler_schedule_attempts_total{result="scheduled"|"unschedulable"|"error"}
scheduler_e2e_scheduling_duration_seconds{result}
```

VMI placements are counted in `scheduler_schedule_attempts_total` too.
`scheduler_e2e_scheduling_duration_seconds` is recorded only for a pod that is
scheduled (`result="scheduled"`), and times one scheduling attempt inside a
pass — not, as upstream, from first seen to bound.

Upstream splits `scheduler_pending_pods` across `active`, `backoff` and
`unschedulable` queues. This scheduler has one queue — the unscheduled pods it
found this pass — so it reports `active` and nothing else rather than inventing
empty queues.

A pod that is placed but waiting for its volumes to bind counts as
`unschedulable`, which is what it is until the volume exists.

## Where this differs from upstream

- **Histograms render as summaries.** No buckets are configured, so every
  `_duration_seconds` metric is emitted as quantiles with no `_bucket` series,
  and `histogram_quantile(…_bucket…)` — what upstream dashboards use — returns
  nothing (#90).
- **`process_cpu_seconds_total` is typed gauge**, not counter; `rate()` still
  works on the values, but a type-checking tool will complain.
- **No authentication on any `/metrics`.** Upstream requires a principal that
  may `get` the non-resource URL `/metrics`, and serves 10257/10259 over
  HTTPS.

## Where metrics do not live

Not in the datastore. Upstream keeps none of this in etcd; a number sampled
every fifteen seconds forever is the one kind of data a consensus store must
not be asked to hold. A scrape endpoint computes on request and keeps nothing.

## rustkube-node

The kubelet's two families — cAdvisor-shaped container metrics at
`/metrics/cadvisor` and the kubelet's own at `/metrics` — belong to the
`rustkube-node` repo and are tracked there (rustkube-node#36). Container
restart counts are **not** a kubelet metric upstream:
`kube_pod_container_status_restarts_total` comes from kube-state-metrics,
derived from `pod.status.containerStatuses[].restartCount`. Whatever plays that
role here should derive it from the object too, or the two will disagree.
