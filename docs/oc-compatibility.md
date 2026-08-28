<!--
Added 2026-08-28 from the user's reference. This is the **compatibility
surface**: what `oc` can ask a cluster to do, and therefore what rustkube has
to answer for before it can claim OpenShift compatibility. It drives
completeness — the list is the specification, not a wish.

Nothing in it is implemented on the strength of being listed. Where a verb is
known to be absent, say so against the verb rather than leaving a reader to
discover it: today `oc logs` works (#54, 2026-08-28) while `oc exec`, `attach`,
`rsh`, `cp`, `rsync`, `port-forward` and `debug` share its proxy plumbing and
none of them are built.
-->

# `oc` Command Reference & StromCOS Verification Runbook

> **Note on "stromcos":** This runbook is written parameterized by name/namespace so it
> applies to whichever Storm-ecosystem component you mean (closest match on file is
> **stormos**, alongside stormblock, stormfs, stormforce, mkube). Once the component is
> confirmed, the `<storm-cr>` placeholders in §6 and the functional smoke test in §12 can
> be filled in with the real CR/API and the whole thing collapsed into a single
> `verify-stromcos.sh` returning a clean exit code.

---

# Part I — `oc` Command Reference

`oc` is `kubectl` plus OpenShift-specific verbs. Grouped the way `oc help` organizes it,
with the deep subcommand trees expanded. `oc <cmd> --help` on the target cluster is always
authoritative — the exact `oc adm` set shifts slightly by cluster version.

## Basic Commands

| Command | Purpose |
|---|---|
| `login` | Authenticate (`-u/-p`, `--token`, `--server`, `--web` for browser flow) |
| `logout` | End session, remove local credentials |
| `new-project` | Request a new project |
| `new-app` | Create app from source/image/template (build + deployment + service) |
| `new-build` | Create a build configuration without deploying |
| `status` | Overview of current project (`-v` adds warnings/suggestions) |
| `project` | Switch active project (context) |
| `projects` | List accessible projects |
| `explain` | Schema docs for a resource (`--recursive` for full field tree) |

## Application Modification (from kubectl)

| Command | Purpose |
|---|---|
| `create` | Create from file/stdin; generators: `secret`, `configmap`, `serviceaccount`, `route`, `deployment`, `job`, `token`, … |
| `apply` | Declarative apply; `edit-last-applied`, `set-last-applied`, `view-last-applied` |
| `get` | Display resources (`-o wide/yaml/json/jsonpath/custom-columns`, `-w` watch) |
| `edit` | Edit a live resource in `$EDITOR` |
| `delete` | Delete by name, label, file, or `--all` |
| `replace` | Full replace from file (`--force` = delete + recreate) |
| `patch` | Strategic/merge/JSON patch of specific fields |
| `label` / `annotate` | Add/update/remove (`key-`) metadata |
| `scale` | Set replica count |
| `autoscale` | Create an HPA |
| `expose` | Generate a service or route from an existing resource |
| `run` | Run an image as a pod/deployment |
| `set` | Mutate object sub-fields (see below) |

**`oc set` subcommands:** `env`, `image`, `resources`, `volumes`, `probe`, `selector`,
`serviceaccount`, `route-backends`, `build-hook`, `build-secret`, `deployment-hook`,
`image-lookup`, `subject`, `data`, `triggers`, `last-applied`.

## Build & Deploy

| Command | Purpose |
|---|---|
| `rollout` | `status`, `history`, `undo`, `pause`, `resume`, `restart`, `latest`, `retry`, `cancel` (Deployment **and** DeploymentConfig) |
| `rollback` | Revert a DeploymentConfig to a prior revision |
| `start-build` | Trigger a build (`--from-dir`, `--from-file`, `--from-repo`, `-F` follow, `--wait`) |
| `cancel-build` | Cancel pending/running/new builds |
| `import-image` | Import/refresh tags into an image stream (`--all`, `--from`, `--confirm`) |
| `tag` | Tag images across image streams (`--scheduled`, `--reference-policy`, `-d` remove) |

## Troubleshooting & Debug

| Command | Purpose |
|---|---|
| `logs` | Pod/container/build/deploy logs (`-f`, `-p` previous, `-c`, `--since`, `--tail`) |
| `exec` | Run a command in a container (`-it` interactive) |
| `rsh` | Open a shell in a container (OpenShift wrapper) |
| `rsync` | Sync files between local FS and a pod |
| `cp` | Copy files/dirs to/from a container |
| `port-forward` | Forward local ports to a pod |
| `proxy` | Local proxy to the API server |
| `attach` | Attach to a running container's streams |
| `debug` | Launch a debug copy of a pod/DC/node (`--as-root`, `node/<name>` for host debug) |
| `wait` | Block until a condition (`--for=condition=…`, `--for=jsonpath=…`, `--for=delete`) |
| `events` | List events (`--for`, `--types`, `-w`) |

## Advanced

| Command | Purpose |
|---|---|
| `process` | Render a template to a resource list (`-p PARAM=val`, `--param-file`) |
| `extract` | Dump secret/configmap keys to disk (`--to=-`, `--keys`, `--confirm`) |
| `observe` | Watch resources and invoke a command on change (experimental) |
| `policy` | Project-level authz (see below) |
| `auth` | `can-i`, `reconcile`, `whoami` |
| `image` | `append`, `extract`, `info`, `mirror` (registry-level ops) |
| `registry` | `info`, `login` (internal registry) |
| `idle` | Idle scalable resources behind a service |
| `api-versions` / `api-resources` | Discovery of server-supported APIs |
| `cluster-info` | Endpoints; `cluster-info dump` for a diagnostic dump |
| `diff` | Live vs. would-be-applied diff |
| `kustomize` | Render a kustomization |
| `plugin list` | Discover `oc-`/`kubectl-` plugins on `$PATH` |

**`oc policy` subcommands:** `add-role-to-user`, `add-role-to-group`,
`remove-role-from-user`, `remove-role-from-group`, `remove-user`, `remove-group`,
`who-can`, `scc-review`, `scc-subject-review`, `add-scc-to-user`, `add-scc-to-group`,
`remove-scc-from-user`, `remove-scc-from-group`.

## Settings

| Command | Purpose |
|---|---|
| `config` | kubeconfig mgmt: `view`, `use-context`, `set-context`, `current-context`, `get-contexts`, `set-cluster`, `set-credentials`, `rename-context`, `delete-context`, `unset` |
| `whoami` | Current user (`-t` token, `-c` context, `--show-server`, `--show-console`) |
| `completion` | Shell completion (`bash`, `zsh`, `fish`, `powershell`) |
| `version` | Client + server version (`-o yaml`, `--client`) |

## `oc adm` — Cluster Administration

**Node lifecycle:** `cordon`, `uncordon`, `drain`, `taint`, `top`
(nodes/pods/images/imagestreams), `node-logs`, `copy-to-node`, `node-image`
(`create`/`monitor`).

**Cluster ops:** `upgrade` (+ `upgrade status`, `upgrade channel`, `upgrade rollback`),
`must-gather`, `inspect`, `wait-for-stability`, `wait-for-node-reboot`.

**Auth/RBAC/security:** `policy` (`add-role-to-user`, `scc-review`, `who-can`, reconcile
variants), `groups` (`new`, `add-users`, `remove-users`, `sync`, `prune`), `certificate`
(`approve`, `deny`), `ocp-certificates` (leaf/CA regen, MCO cert rotation),
`verify-image-signature`.

**Projects/templates:** `new-project`, `create-bootstrap-project-template`,
`create-login-template`, `create-error-template`, `create-provider-selection-template`.

**Maintenance:** `prune` (`builds`, `deployments`, `images`, `groups`, `renderer`),
`migrate` (`storage`, `template-instances`), `release` (`info`, `extract`, `mirror`,
`new`, `--tools`), `catalog` (`mirror`, `build`), `build-chain`, `pod-network` (legacy
SDN: `join-projects`, `isolate-projects`, `make-projects-global`).

**Most relevant to SR-IOV / disconnected work:** `oc adm release mirror` and
`oc adm catalog mirror` for disconnected operator content; `oc debug node/<name>` to drop
into a host and check VFs / driver binding; `oc get sriovnetworknodestate -n
openshift-sriov-network-operator` for the operator's device inventory.

---

# Part II — StromCOS Verification Runbook

Copy-paste ready. Set the vars once; the rest follows.

```bash
export NS=stromcos                 # namespace
export APP=stromcos                # workload / label value
export OPERATOR=stromcos-operator  # if operator-managed; else skip §2
```

## 0. Cluster + identity sanity

```bash
oc whoami                                   # confirm the right subject
oc whoami --show-server --show-console      # confirm the right cluster
oc version -o yaml                          # client + server skew
oc cluster-info                             # API + core endpoints reachable
oc get clusterversion                       # cluster not mid-upgrade / degraded
oc auth can-i '*' '*' -n $NS                # do you have the access to verify?
```

## 1. Namespace + CRD discovery

```bash
oc get project $NS
oc get project $NS -o jsonpath='{.status.phase}{"\n"}'
oc api-resources | grep -i storm            # CRDs registered?
oc get crd | grep -i storm
oc explain <crd>.spec --recursive           # schema sanity for the CR you deploy
```

## 2. Operator / install verification (skip if plain Deployment)

```bash
oc get subscription -n $NS
oc get installplan -n $NS
oc get csv -n $NS                           # PHASE must be Succeeded
oc get csv -n $NS -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.phase}{"\n"}{end}'
oc describe csv $OPERATOR -n $NS            # .status.conditions on failure
oc get pods -n $NS -l name=$OPERATOR       # operator pod Running
```

## 3. Workload presence + rollout state

```bash
oc get all -n $NS                           # broad first pass
oc get deploy,sts,ds,dc -n $NS -l app=$APP
oc rollout status deploy/$APP -n $NS --timeout=120s
oc get deploy $APP -n $NS \
  -o jsonpath='desired={.spec.replicas} ready={.status.readyReplicas} avail={.status.availableReplicas}{"\n"}'
oc rollout history deploy/$APP -n $NS
```

## 4. Pod health

```bash
oc get pods -n $NS -l app=$APP -o wide      # node placement + IPs
oc get pods -n $NS -l app=$APP \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.phase}{"\t"}ready={.status.containerStatuses[*].ready}{"\t"}restarts={.status.containerStatuses[*].restartCount}{"\n"}{end}'
oc wait --for=condition=Ready pod -l app=$APP -n $NS --timeout=180s
oc get pods -n $NS --field-selector=status.phase!=Running,status.phase!=Succeeded
oc describe pod -l app=$APP -n $NS | less   # events, probe failures, OOMKills
```

## 5. Config + secret wiring

```bash
oc get configmap,secret -n $NS
oc get deploy $APP -n $NS -o jsonpath='{.spec.template.spec.containers[*].env[*].name}{"\n"}'
oc set env deploy/$APP -n $NS --list        # resolved env incl. configMap/secret refs
oc extract configmap/<name> -n $NS --to=- --keys=<key>
oc get secret <name> -n $NS -o jsonpath='{.data}' | tr ',' '\n'   # key presence, not values
```

## 6. Custom resource status ("is it actually reconciled")

```bash
oc get <storm-cr> -n $NS
oc get <storm-cr> $APP -n $NS -o jsonpath='{.status.conditions}' | jq .
oc wait --for=condition=Ready <storm-cr>/$APP -n $NS --timeout=180s
oc describe <storm-cr> $APP -n $NS          # reconcile errors surface here
```

## 7. Networking + reachability

```bash
oc get svc,endpoints -n $NS -l app=$APP
oc get endpoints $APP -n $NS -o jsonpath='{.subsets[*].addresses[*].ip}{"\n"}'   # NON-EMPTY
oc get route -n $NS
oc port-forward svc/$APP -n $NS 8080:<port> &
curl -sf http://localhost:8080/healthz && echo OK
oc debug -n $NS deploy/$APP -- curl -sf http://$APP:<port>/healthz   # in-cluster DNS + service path
```

## 8. Storage (Storm engines are storage-heavy — verify explicitly)

```bash
oc get pvc -n $NS
oc get pvc -n $NS -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.phase}{"\n"}{end}'  # all Bound
oc get pv | grep $NS
oc get sc                                   # storageclass present + default marked
oc get pods -n $NS -l app=$APP -o jsonpath='{.items[*].spec.volumes[*]}' | jq .
oc rsh -n $NS deploy/$APP -- sh -c 'lsblk; nvme list 2>/dev/null; df -h'   # NVMe-oF / block paths
```

## 9. Node / SR-IOV / SCC

```bash
oc get scc
oc get pod -l app=$APP -n $NS \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.metadata.annotations.openshift\.io/scc}{"\n"}{end}'
oc get network-attachment-definitions.k8s.cni.cncf.io -n $NS
oc get sriovnetworknodestate -n openshift-sriov-network-operator
oc get sriovnetworknodestate -n openshift-sriov-network-operator \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.syncStatus}{"\n"}{end}'   # Succeeded
oc debug node/<node> -- chroot /host sh -c 'ip -d link show; ls -l /sys/class/net/*/device/virtfn* 2>/dev/null'
```

## 10. RBAC / service account wiring

```bash
oc get sa -n $NS
oc get rolebinding,clusterrolebinding -n $NS
oc policy who-can get <storm-cr> -n $NS
oc auth can-i --list --as=system:serviceaccount:$NS:$APP -n $NS
```

## 11. Events + logs

```bash
oc get events -n $NS --sort-by=.lastTimestamp | tail -30
oc get events -n $NS --field-selector type=Warning
oc logs -n $NS deploy/$APP --tail=100
oc logs -n $NS deploy/$APP --previous --tail=50   # crash-loop history
oc logs -n $NS -l name=$OPERATOR --tail=100       # operator's reconcile view
```

## 12. Functional smoke test (component-specific)

```bash
# Prove the datapath, not just that pods are up. Adapt per component:
oc rsh -n $NS deploy/$APP -- sh -c '<write a test object / provision a volume / run self-check>'
oc rsh -n $NS deploy/$APP -- sh -c '<read it back and diff>'
# Storage-engine baseline:
oc rsh -n $NS deploy/$APP -- sh -c 'fio --name=verify --rw=randwrite --bs=4k --size=256M --numjobs=1 --runtime=10 --time_based --direct=1'
```

## 13. One-shot gate (CI / must-gather)

```bash
oc adm inspect ns/$NS --dest-dir=./inspect-$NS    # full namespace snapshot
oc adm must-gather -- /usr/bin/gather             # cluster-level, if a bug is suspected
```
