# Metrics

Every component serves Prometheus text format under **upstream Kubernetes'
metric names**. That is the whole design constraint: a dashboard, recording
rule or alert written against Kubernetes should work here unchanged, and an
equivalent metric under a different name is worth much less than the same
metric under the same name (#51).

| component | endpoint |
|---|---|
| kube-apiserver | `/metrics` on the API listener (as upstream) |
| kube-controller-manager | `:10257/metrics` |
| kube-scheduler | `:10259/metrics` |
| rustkube-node | separate repo — see the note at the end |

The exporter is shared (`apimachinery::metrics`); each component adds only
what is its own. It was three copies before, and they had drifted.

## Every component

`process_cpu_seconds_total`, `process_resident_memory_bytes`,
`process_virtual_memory_bytes`, `process_start_time_seconds`,
`process_open_fds`, `process_max_fds` — read from `/proc/self` **at scrape
time**, because a value sampled on a timer is stale by up to the timer.
Upstream gets these free from the Prometheus Go client, so every Kubernetes
dashboard assumes them, and nothing in Rust provides them.

`kubernetes_build_info{gitVersion,component,goVersion}`.

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

`etcd_request_duration_seconds` covers the fastetcd round trip. The store being
slow is the usual reason an apiserver is slow, and this is the metric an alert
for that is written against.

`apiserver_storage_objects` and `apiserver_watch_events_total` come from the
watch cache, which already holds the numbers — no extra LIST, no extra cost.

## controller-manager

```
leader_election_master_status{name="kube-controller-manager"}
controller_reconcile_duration_seconds{controller}
controller_reconcile_errors_total{controller}
```

`leader_election_master_status` is upstream's name and the one that matters:
it is 1 on the holder and 0 elsewhere, so **two instances both reporting 1** is
visible the moment it happens rather than when they start fighting.

There are deliberately **no `workqueue_*` metrics**. These controllers are poll
loops with no queue; exporting `workqueue_depth` as a constant zero would be a
number that reads as a fact. Where the shape differs from upstream, so does the
name.

## scheduler

```
leader_election_master_status{name="kube-scheduler"}
scheduler_pending_pods{queue="active"}
scheduler_schedule_attempts_total{result="scheduled"|"unschedulable"}
scheduler_e2e_scheduling_duration_seconds{result}
```

Upstream splits `scheduler_pending_pods` across `active`, `backoff` and
`unschedulable` queues. This scheduler has one queue — the unscheduled pods it
found this pass — so it reports `active` and nothing else rather than inventing
empty queues.

A pod that is placed but waiting for its volumes to bind counts as
`unschedulable`, which is what it is until the volume exists.

## Where metrics do not live

Not in the datastore. Upstream keeps none of this in etcd; a number sampled
every fifteen seconds forever is the one kind of data a consensus store must
not be asked to hold. A scrape endpoint computes on request and keeps nothing.

## rustkube-node

The kubelet's two families — cAdvisor-shaped container metrics at
`/metrics/cadvisor` and the kubelet's own at `/metrics` — belong to the
`rustkube-node` repo and are tracked there (rustkube-node#26). Container
restart counts are **not** a kubelet metric upstream:
`kube_pod_container_status_restarts_total` comes from kube-state-metrics,
derived from `pod.status.containerStatuses[].restartCount`. Whatever plays that
role here should derive it from the object too, or the two will disagree.
