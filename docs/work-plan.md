# Work plan — moved out of CLAUDE.md

Kept here whole, newest notes as they were. Write new intentions here.

## Work Plan

### Image triggers (#386, sc#162) — CODE DONE, WAITING on rigs 838045bbdd 2026-11-25
Ported from openshift-controller-manager `pkg/image/trigger` (annotations,
deploymentconfigs, buildconfigs), `pkg/apps/deploymentconfig`
(`triggerActivated`), openshift-apiserver `registry/deployconfig`
(`processTriggers`, `canTrigger`, rollback), `apis/apps/validation`,
`apis/build/validation`, `buildgenerator` (`updateImageTriggers`) and
`pkg/build/apiserver/webhook` (github, gitlab, bitbucket, generic), library-go
`imageutil` (read 2026-11-25). No owner decision needed: the design choices
below are engineering.
- [x] controller-manager `imagetrigger`: runs while `v1.image.openshift.io`
  is Available; its own ImageStream reflector (`reflector::run_events`, keyed
  by ns/name — the registry's push-only streams have no uid, which the
  indexed informers require); annotation triggers on Deployments,
  DaemonSets, StatefulSets, CronJobs (Jobs not: immutable template, as
  upstream's sources); DC automatic ImageChange → containers +
  `lastTriggeredImage`; BC ImageChange → instantiate with `triggeredByImage`
  (skipped while its ConfigChange build has not run).
- [x] DC: validation lifted (ImageChange, `tagImages`), defaults,
  `triggerActivated` in the DC controller, `processTriggers`/`canTrigger` in
  `instantiate`, rollback turns image triggers manual, the deployer runs
  `tagImages` (PUT the ImageStreamTag, POST on 404; `create imagestreamtags`
  added to the controller-manager role).
- [x] BC: ImageChange + GitHub/Generic/GitLab/Bitbucket validated and run;
  ImageStreamTag/Image `from` resolved through the aggregator
  (`image_policy::fetch`); `updateImageTriggers` (spec + status);
  `buildconfigs/<n>/webhooks/<secret>/<type>`, `system:webhook` +
  `system:webhooks` (system:authenticated, as OpenShift 4.16); RBAC path
  parse takes segments after a subresource as its own.
- [x] c29eb39: sc-build-7593f28cce — workspace build; controller-manager
  176/176, DC units 11/11, `apimachinery::imagestream` 3/3. 6086f20:
  sc-build-47b4142dd0 — workspace build, `cargo test --workspace` all pass
  (apiserver 481, controller-manager 176, apimachinery 142, scheduler 79).
- [x] Rig `image-triggers` (suite `rigs`); rigs `builds`, `deploymentconfigs`
  updated for what is no longer refused; README, oc-compatibility,
  test/README, CHANGELOG (66f8fff).
- [x] clippy (workspace, all targets) at 7bbad9d, sc-build-4c0a6c88f0: 42
  warnings, as before; none in a file #386 changed.
- [ ] Rig `image-triggers` on a test machine: 2026-11-25 pvetest1|2|3 refuse
  (400, no known release; stormcluster#128 open). Parked
  `--after-install stormcluster>=f1de3f6@any:pve` (item 5e042215104c).
  Close #386 when it passes. 2026-11-26: `rigs` cb87060b01 queued on
  pvetest2 at main 455564b (carries 7bbad9d); held `--after-run cb87060b01`.
  2026-11-26 (night): cb87060b01 was interrupted (stormcentral restarted at
  'wait for the apiserver'); no test ran. Held `--after-run 0e2114d89b`
  (`rigs`, pvetest2, main 9a61694, carries 7bbad9d).
- [x] 2026-11-27: `rigs` 0e2114d89b (pvetest2) errored before any test:
  pvetest2's apiserver did not answer /readyz in 10 min, as every component's
  runs there now (flowsdn, stormvm, rustkube-node). Re-queued `rigs`
  838045bbdd on pvetest1 at main f002db9 (waits behind the night runs);
  held `--after-run 838045bbdd`.
- [ ] The issue's check against the real registry (stormblock-registry
  v0.93+) on a node: `oc set triggers deploy/web --from-image=mystream:latest
  -c web`, push, the Deployment rolls to the new digest.

### image.openshift.io ImageStreams + image policy admission (#385, sc#162) — BUILT 2026-11-25 (stormblock-registry#258 closed in its v0.93.0)
Master decided option A (2026-11-25): the registry serves `image.openshift.io/v1`
(stormblock-registry#235, APIService by stormcos#1557) and will serve
ImageStreams as full objects: create, `spec.lookupPolicy`, watch
(stormblock-registry#258, filed 2026-11-25). rustkube serves no image objects.
- [x] Registry gap filed: stormblock-registry#258. #385 proposed
  `--after stormblock-registry#258`. (Proposing #258 itself is the registry's or
  the master's: the rustkube token is refused.)
- [x] Mutating image policy admission, as OpenShift's `imagepolicy`: resolve a
  container's `image: <stream>:<tag>` to the tag's `dockerImageReference` by
  digest, for pods in a namespace whose stream has `lookupPolicy.local`, and
  every pod template of a workload with
  `alpha.image.policy.openshift.io/resolve-names: "*"`. Streams are read through
  the aggregator. **Registry unreachable → the image is left unchanged; the pod
  is never refused.**
- [x] Rig (the issue's three checks), README, CHANGELOG, oc-compatibility.
- 2026-11-25 (build): `pkg/apiserver/src/image_policy.rs`, called first in
  `admission::admit` (create/update with an object). Ported from
  apiserver-library-go `imagepolicy` (read 2026-11-25): under the default
  config every rule is `DoNotAttempt`, so only local names (`LocalRewrite`)
  are ever rewritten; controller-owned objects and subresources skipped;
  Jobs not on update; unchanged images on update skipped unless the
  resolve-names annotation changed (`resolveAnnotationChanged`). Lookups by
  `Aggregator::get_own` (X-Remote-User `system:apiserver`, groups
  `system:masters`, `system:authenticated`; 2 s). Builds not covered (#386).
- [x] Found on the way: aggregated discovery said `Stale` for every
  aggregated version, and client-go drops Stale GVs (`aggregated_discovery.go`
  `convertAPIGroup`), so `oc set image-lookup <stream>` could never map
  `imagestreams`. The availability check now keeps the backend's
  `APIResourceList`; discovery serves it Current.
- Rig `image-policy` (in `rigs`): a Python stand-in for the registry's
  image.openshift.io (as the aggregation rig's stub), real `oc set
  image-lookup` on the stream and `deploy/web`, `--enabled=false`, the
  registry killed (never refused). Waits for a test-machine run like the
  other rigs (stormcluster#128).
- [x] 991e8ad: sc-build-c932ce0d8f (fresh build VM) — workspace build;
  apiserver 474 tests pass (463 + 11 new). 819914a: sc-build-f6ef955b95 —
  the 11 by name (10 `image_policy`, 1 discovery); clippy: no warning in
  `image_policy.rs`, `aggregation.rs`, `discovery.rs`.
- 2026-11-25 (later): stormblock-registry#258 still open. Noted for the build:
  the hook is `admission::admit` before the mutating webhooks (it has the
  state, the operation and the old object on every write path), as
  OpenShift's built-in `imagepolicy` runs before webhooks; the stream read is a
  GET through `Aggregator` with a short timeout, as `system:apiserver`.

### route.openshift.io `routes/status` (#397, stormcos#1491, stormlb#48) — DONE 2026-11-25
Discovery listed `routes/status`, `server.rs` had no route: a router's
status PATCH was a 404.
- [x] apiserver: `…/routes/{name}/status` get/put/patch (the shared status
  handlers), in `route_openshift_routes()`; Route's status strategy
  (`builtin_admission::route_status`: dropped on create, kept on a write of
  the Route). README, oc-compatibility, CHANGELOG.
- [x] Unit `server::tests::routes_status_is_served_and_owns_status` (HTTP
  through the router, with admission attributes).
- [x] 9cc4a3c: sc-build-3be44868d4 (fresh build VM) — workspace build;
  apiserver 463 tests pass incl. the new one; clippy: no warning on a
  changed line.
- No new rig: the unit drives the real handlers; rigs wait on
  stormcluster#128 anyway. stormcos's `deploy/test/prometheus-route.sh`
  is the end-to-end check.

### quota.openshift.io ClusterResourceQuotas (#391, sc#50) — CODE DONE, RIG BLOCKED 2026-11-24
Ported from openshift-apiserver `pkg/quota` (validation, strategy,
`appliedclusterresourcequota/filter.go`, printer), library-go
`pkg/quota/clusterquotamapping` (`GetObjectMatcher`), apiserver-library-go
`pkg/admission/quota/clusterresourcequota` (admission + accessor) and
cluster-policy-controller `clusterquotareconciliation` (read 2026-11-24). In
OpenShift 4 the kind is a CRD on kube-apiserver; here it is built in.
- [x] apiserver: `clusterresourcequotas` (+ `/status`) with the strategies and
  validation; quota admission charges every selecting CRQ in one plan with
  the namespace's ResourceQuotas; `appliedclusterresourcequotas` get/list;
  view/edit/admin read them; printer, discovery, apply table, quantities.
- [x] controller-manager `clusterresourcequota`: `status.namespaces[]` +
  `status.total` from `resourcequota::usage_in` (shared now).
- [x] Side fix: a single GET answers a Table when asked (`oc get <kind>
  <name>` printed NAME/AGE only). Affects the #372/#373/#390 rigs' checks.
- [x] f8d4ef9: sc-build-9dc150625c — workspace build; apimachinery 139,
  apiserver 447, controller-manager 158, all pass.
- [x] Rig `cluster-quota` (suite `rigs`), README, oc-compatibility,
  test/README, CHANGELOG.
- [x] clippy (workspace, all targets) at 25b4f82: 42 warnings, none in a
  changed file or line.
- [ ] Rig `cluster-quota` on a test machine: 2026-11-24 `stormcentral test run
  rustkube rigs --tag pvetest1|2|3` refused (400, stormcluster#128 open), as
  #372/#373/#388/#390. Held with `stormcentral propose --after-install
  'stormcluster>=f1de3f6@any:pve'`. Close #391 when it passes.
- Not ported: OpenShift's admission also charges *updates* through the
  evaluator; rustkube's quota admission (#124) checks creates only, and the
  CRQ path follows it.
- [x] 2026-11-26: rig `cluster-quota` rides `rigs` cb87060b01 (pvetest2, main
  455564b, carries 25b4f82); held `--after-run cb87060b01`. Close on its pass.

### user.openshift.io Users, Identities, UserIdentityMappings (#390, sc#50) — CODE DONE, RIG BLOCKED 2026-11-24
Ported from openshift/oauth-apiserver `pkg/user` (validation, strategies,
`registry/user/etcd` `~`, `registry/useridentitymapping/rest.go`, printers)
and apiserver-library-go `ValidateUserName` (read 2026-11-24; the user
registry moved there from openshift-apiserver).
- [x] `users`, `identities` stored by the generic cluster handlers, with the
  strategies (User `groups` null, Identity named provider:login, `user: {}`)
  and validation; `useridentitymappings` create/get/update/delete as the
  link, not stored; `users/~`; basic-user `get users ~`; printers; discovery.
- [x] 7bfdb60 (code + units), rig `oc-users` (suite `rigs`), README,
  oc-compatibility, test/README, CHANGELOG.
- [x] sc-build-0b4777d71b at 7bfdb60: apiserver build + 441/441 tests (the
  10 `user_identities` units, the table and basic-user units). At feef4d0:
  workspace build and `cargo test --workspace` all pass (apiserver 441,
  controller-manager 156); clippy: no warning on a changed line (the 24 are
  older). That job exited 1 only on `command -v shellcheck` (none on the VM)
  and sc-build filed #394 for it — not a failure, to be closed.
- [ ] Rig `oc-users` (suite `rigs`) on a test machine: 2026-11-24
  `stormcentral test run rustkube rigs --tag pvetest1|2|3` refused (400, the
  machine's apiserver not answering; stormcluster#128 open), as #372/#373/#388.
  Held with `stormcentral propose --after-install 'stormcluster>=f1de3f6@any:pve'`
  (item 6772095bd62d). Close #390 when `oc-users` passes.
  2026-11-26: `rigs` cb87060b01 queued on pvetest2 at main 455564b (carries
  feef4d0); held `--after-run cb87060b01`.
- Not done (optional in the issue): an Identity's user from authentication —
  rustkube has no identity provider / OAuth server to map a login.
- Noticed, not changed: oauth-apiserver's `ValidateGroup` now allows `b64:`
  user names in a Group; `user_groups` (#372, from the older validation)
  refuses them.

### Secret scan findings (#393, stormcentral#1679) — DONE 2026-11-23
Scan of 7e517dc61971: `generic-api-key` at `bootstrap_token.rs:140,182`, `private-key`
at `pkg/apiserver/testdata/sa-test.key:1`. All test fixtures, not secrets; fixed per
the test-secrets rule (generate keys in tests, plain placeholders), no allowlist.
- [x] 4b385c0: SA test RSA keypair generated once per run (`rsa` dev-dep),
  `testdata/` removed; bootstrap token test secret `example000000000` (units + rigs
  `bootstrap-token`, `cluster-info`).
- [x] ba4c5a3: Cargo.lock adds rsa 0.9.10 and its deps only (nothing bumped).
- [x] sc-build-ffc1a45209 at ba4c5a3: `cargo build --locked && cargo test --locked`
  passed (apiserver 430, incl. the auth and bootstrap_token tests).
- [ ] The next nightly scan closes #393 (gitleaks not run here; no PEM private key
  left in the tree, the new value's entropy 2.09 < the rule's 3.5).
- The two rigs changed only in the value; they run with the next `rigs` suite.

### minReadySeconds in the workload controllers (#379, from #369) — CODE DONE, WAITING on rigs 838045bbdd 2026-11-25
Upstream's `podutil.IsPodAvailable` as `controller-manager::availability`, with
a requeue at the moment the next Ready pod becomes available (the indexed
runner's `reactor::requeue_after`, which #379 thought missing, exists).
- [x] ReplicaSet/RC `availableReplicas`; DaemonSet `numberAvailable` and
  `plan_rollout` (an availability predicate); StatefulSet
  `availableReplicas` and OrderedReady `plan`; Deployment copies
  `minReadySeconds` to its new ReplicaSet (and patches the current one).
- [x] Units: availability, DaemonSet rollout, StatefulSet plan.
- [x] Rig `min-ready` (suite `rigs`), README, test/README, CHANGELOG.
- [x] sc-build at d12cbe4 (sc-build-605e2d3338): workspace build, every
  crate's tests (controller-manager 171/171, apiserver 462/462); no new
  clippy finding (deployment.rs's items-after-test-module is older).
- [ ] Run the rig `min-ready` (suite `rigs`): parked
  `--after-install stormcluster>=f1de3f6@any:pve` (the #128 fix).
  2026-11-26: `rigs` cb87060b01 queued on pvetest2 at main 455564b (carries
  d12cbe4; `min-ready` is in the day list); held `--after-run cb87060b01`.
  2026-11-26 (night): cb87060b01 was interrupted (stormcentral restarted at
  'wait for the apiserver'); no test ran. Held `--after-run 0e2114d89b`
  (`rigs`, pvetest2, main 9a61694, carries d12cbe4).
- [x] 2026-11-27: `rigs` 0e2114d89b (pvetest2) errored before any test:
  pvetest2's apiserver did not answer /readyz in 10 min, as every component's
  runs there now (flowsdn, stormvm, rustkube-node). Re-queued `rigs`
  838045bbdd on pvetest1 at main f002db9 (waits behind the night runs);
  held `--after-run 838045bbdd`.

### DeploymentConfig Custom strategy (#392, from #375) — CODE DONE, WAITING on rigs 838045bbdd 2026-11-25
Ported from openshift-controller-manager `pkg/apps/deployer`
(`makeDeployerPod`, `makeDeployerContainer`, the pod-phase → version-phase
map) and bootstrappolicy (`system:deployer`, `system:deployers`).
- [x] apiserver: Custom admitted; `customParams` required with an image (no
  default deployer image here); environment names validated; `customParams`
  on Rolling/Recreate refused (no deployer image to swap); hooks in a Custom
  strategy's rolling/recreate params validated.
- [x] apiserver: `system:deployer` ClusterRole at boot; `system:deployers`
  per namespace by `default_rolebindings` (moved out of `builds`, now both
  bindings); `deploymentconfigs/log` serves a Custom version's deployer pod
  log, else the copy on the RC.
- [x] controller-manager: `deployer` SA per namespace; the Deployer runs a
  Custom version as `<rc>-deploy` (SA deployer, owned by the RC), phase
  follows the pod, output copied onto the RC, deadline, a missing pod fails it.
- [x] Rig `deploymentconfigs` section 8 (the script plays the image with a
  `deployer` token), README, oc-compatibility, test/README, CHANGELOG.
- [x] sc-build at b6c446d (sc-build-03cc42fddb): workspace build, apiserver
  462/462, controller-manager 166/166; clippy clean in the changed files
  (one type_complexity in default_rolebindings fixed at b6c446d). At 3a8937d
  (sc-build-cba5fea44a) the whole workspace's tests passed too.
- [ ] Run the rig `deploymentconfigs` (suite `rigs`): parked
  `--after-install stormcluster>=f1de3f6@any:pve` (the #128 fix).
  2026-11-26: that fix is installed, but `rigs` is refused on all three pve
  machines (pvetest1/pvetest3 14.08 installs failed; pvetest2 sbregistry
  :5100 failing). Held --after-install stormcos>=f7f7ccb@any:pve, with
  #96/#295/#343/#388.
  2026-11-26 (night): `rigs` cb87060b01 (pvetest2) was interrupted
  (stormcentral restarted at 'wait for the apiserver'); no test ran. Held
  `--after-run 0e2114d89b` (`rigs`, pvetest2, main 9a61694, carries b6c446d;
  shared with #295/#343/#388).
- [x] 2026-11-27: `rigs` 0e2114d89b (pvetest2) errored before any test:
  pvetest2's apiserver did not answer /readyz in 10 min, as every component's
  runs there now (flowsdn, stormvm, rustkube-node). Re-queued `rigs`
  838045bbdd on pvetest1 at main f002db9 (waits behind the night runs);
  held `--after-run 838045bbdd`.
- [ ] The issue's Check with a real image on a node: a Custom image that
  scales new up and old down, and `oc logs dc/x` shows its output.

### apps.openshift.io DeploymentConfigs (#375, sc#151, sc#162) — CODE DONE, WAITING on rigs 838045bbdd 2026-11-23
Option A of the question on #375 (stormcentral's owner queue refused it:
"a gap: session work — build it", #1680): rollouts run in the
controller-manager, no deployer pod or image. Ported from
openshift-apiserver `pkg/apps`, openshift-controller-manager `pkg/apps` and
oc `pkg/cli/deployer` (read 2026-11-23).
- [x] API: defaults, validation, spec/status strategies, instantiate,
  rollback, log (from the RC's recorded log), scale, `oc get dc`, RBAC
  (admin/edit/view; controller-manager creates RCs).
- [x] Controller-manager: `deploymentconfig` (versions as RCs, ConfigChange,
  cancel older, settle, history limit, status/conditions) and `deployer`
  (Rolling, Recreate, pre/mid/post hook pods with failurePolicy, deadline,
  cancel/retry, log on the RC).
- [x] 21dcdfd: sc-build — workspace build, apiserver 430/430,
  controller-manager 156/156, clippy clean in the new files.
- [x] Rig `deploymentconfigs` (suite `rigs`), docs, CHANGELOG.
- [x] Gaps: Custom strategy filed as #392 (built 2026-11-25); `tagImages` hooks and
  ImageChange triggers belong to #385 (ImageStreams) and #386 (image
  triggers) — noted there to lift the refusals.
- [ ] Run the rig: every pve test machine refuses runs until one installs the
  stormcluster#128 fix (as #372, #373, #388).
- [x] 2026-11-25: held with `stormcentral propose --after-install
  'stormcluster>=f1de3f6@any:pve'` (item b5ec48cd2449), as #372/#373/#388.
- [x] 2026-11-26: `rigs` cb87060b01 queued on pvetest2 at main 455564b
  (carries 86fef2d); held `--after-run cb87060b01`. Close on `deploymentconfigs`.
- [x] 2026-11-26 (night): cb87060b01 was interrupted (stormcentral restarted
  at 'wait for the apiserver'); no test ran. Held `--after-run 0e2114d89b`
  (`rigs`, pvetest2, main 9a61694, carries 86fef2d).
- [x] 2026-11-27: `rigs` 0e2114d89b (pvetest2) errored before any test:
  pvetest2's apiserver did not answer /readyz in 10 min, as every component's
  runs there now (flowsdn, stormvm, rustkube-node). Re-queued `rigs`
  838045bbdd on pvetest1 at main f002db9 (waits behind the night runs);
  held `--after-run 838045bbdd`.

### build.openshift.io Builds and BuildConfigs (#374, sc#151, sc#162) — ON MAIN, CHECK + RIG BLOCKED 2026-11-24
Ported from openshift-apiserver and openshift-controller-manager `pkg/build`
(read 2026-11-23). The issue left "what runs the build in the pod, and how
it reaches the registry" to us; the privileged-pod question went to option A
(the owner queue: a gap's design question is session work, #1680).
- [x] The builder is OpenShift's builder image (`--build-image`, default
  origin-docker-builder 5.0.0 by digest), in OpenShift's build pod; node
  paths are emptyDirs on stormcos; `--build-insecure-registries` into the
  build's registries.conf for the node registry (`:5100`, HTTP).
- [x] apiserver: API, defaults (incl. history limits 5/5), validation,
  instantiate/clone/details/log, BuildByStrategy, RBAC,
  `system:image-builders` per namespace (apiserver watch; the
  controller-manager creates no RBAC objects, #176).
- [x] controller-manager: `build` (run policies, pods per strategy,
  sys-config ConfigMap, phase/completion/log snippet, cancel) and
  `buildconfig` (ConfigChange first build, history pruning); `builder` SA
  per namespace; `use` on the `privileged` SCC and `create`
  `buildconfigs/instantiate` in its role.
- [x] Refused, gaps elsewhere: JenkinsPipeline, Binary (`instantiatebinary`),
  webhook triggers (`webhooks`), ImageChange (#385/#386), Custom
  `exposeDockerSocket`.
- [x] sc-build on branch `issue-374-v2`: workspace build, apiserver 443/443,
  controller-manager 163/163; no clippy findings in the new files.
- [x] Rig `builds` (suite `rigs`), README (+ the two flags), oc-compatibility,
  CHANGELOG, test/README.
- [ ] stormcos#1558: ship the builder image as a golden and pass
  `--build-image` / `--build-insecure-registries`; then the issue's check, a
  real Docker build that pushes, on a node.
- [x] Onto main: the owner allowed it 2026-11-24; `main` fast-forwarded
  6163f9a → bde4c1d (`issue-374-v2`); 0fd9d7b records it. sc-build-170bf484ee
  at 0fd9d7b on main: workspace build, apiserver 460/460, controller-manager
  165/165, scheduler 79/79, apimachinery 139/139. History of the wait:
  6f26d68 (2026-11-23) merged main (#393) into the branch, so the
  fast-forward stays clean; sc-build-VM at 6f26d68: workspace build,
  apiserver 443/443, controller-manager 163/163. Asked again on #374.
  2026-11-24: 342559c merged main (#390) into the branch (conflicts were list
  unions: apply table, apiVersion map, rigs DAY list); sc-build-a16fdee363 at
  342559c: workspace build, apiserver 454/454, controller-manager 163/163.
  `main` → branch is still a fast-forward. The merge is still the owner's.
  2026-11-24 (later): 75b75a6 merged main (#391) in, list unions only;
  sc-build-b18cc2b8cb at 75b75a6: workspace build, apiserver 460/460,
  controller-manager 165/165. Asked the owner again.
- The issue stays open until its check (a real Docker build pushes, then
  `oc adm prune builds --keep-complete=0 --confirm` removes it) passes.
- [ ] Rig `builds`: pve test machines refuse runs until the stormcluster#128
  fix is installed (as #372, #373, #375, #388).
  2026-11-25: item parked `--after-install stormcluster>=f1de3f6@any:pve`
  (the #128 fix). `--after stormcos#1558` is refused as circular, because
  #1558's item waits for #374 to close. Raised with the master and on
  stormcos#1558: what #1558 needs, #374 on main, is done.
  2026-11-26: rig `builds` rides `rigs` cb87060b01 (pvetest2, main 455564b,
  carries bde4c1d); item held `--after-run cb87060b01`. The cycle stands
  (stormcentral refused `--after stormcos#1558` again); raised again in a
  master note. The Check still needs #1558's builder image.

### Zero-touch onboarding: cluster-info + stormcentral's CSR (#388, stormcentral#1676) — CODE DONE, WAITING on rigs 838045bbdd 2026-11-26
- [x] controller-manager `bootstrapsigner`: `kube-public/cluster-info`
  `kubeconfig` (server = `--apiserver`, CA = the `kube-root-ca.crt` bundle;
  kept, a changed one put back) and `jws-kubeconfig-<id>` per token with
  `usage-bootstrap-signing: "true"` (detached HS256 JWS, header
  `{"alg":"HS256","kid":"<id>"}`, string-equal to kubeadm's recompute);
  stale/expired signatures removed, requeued at the earliest expiration.
  Runs when there is a CA bundle (as the root CA publisher).
- [x] apiserver bootstrap Role + RoleBinding `system:bootstrap-signer-clusterinfo`
  in kube-public: `system:anonymous` may `get` cluster-info only.
- [x] `csr_admission`: a bootstrapper in `system:bootstrappers:stormcentral`
  may also create a `kubernetes.io/kube-apiserver-client` CSR with subject
  exactly `CN=stormcentral` (+ `O=stormcentral:forge` or no O), no SANs,
  usages ⊆ client auth / digital signature / key encipherment incl. client
  auth. Reads stay own-only (`guard_read`, unchanged).
- [x] Units (HMAC RFC 4231, JWS vs an independent Python computation, token
  rules, desired data; anonymous role; CSR subject/SAN/usage/group matrix);
  rig `cluster-info` (suite `rigs`).
- [x] sc-build at 5b67427 (build VM 69d7274f43): workspace build; apiserver
  424/424, controller-manager 151/151 (the 8 new tests by name, job on the
  same commit); clippy silent on the touched files.
- [ ] Rig `cluster-info` (suite `rigs`) on a test machine. 2026-11-23:
  `stormcentral test run rustkube rigs --tag pvetest1|2|3` is refused (400:
  the machine's apiserver not answering, stormcluster API down since
  2026-11-21/22, stormcluster#128), as for #372/#373. Then run it at main and
  close #388 when `cluster-info` passes.
- [x] 2026-11-23: held with `stormcentral propose --after-install
  'stormcluster>=f1de3f6@any:pve'` (item fa8d247ee3a7), after the service's
  parked notice: it returns once a pve test machine passes an install of a
  release carrying the stormcluster#128 fix, which is when the rig can run.
- [x] 2026-11-26: run be3c77b765 (8a356d3, pvetest1) errored on forge's 502
  before any test. pvetest2 then took a run again: `rigs` cb87060b01 at main
  455564b on pvetest2, and the item is held `--after-run cb87060b01`. Close on
  `cluster-info`.
- [x] 2026-11-26 (night): cb87060b01 was interrupted (stormcentral restarted
  at 'wait for the apiserver'); no test ran. Held `--after-run 0e2114d89b`
  (`rigs`, pvetest2, main 9a61694, carries 5b67427; queued for #343). Close
  on `cluster-info`.
- [x] 2026-11-27: `rigs` 0e2114d89b (pvetest2) errored before any test:
  pvetest2's apiserver did not answer /readyz in 10 min, as every component's
  runs there now (flowsdn, stormvm, rustkube-node). Re-queued `rigs`
  838045bbdd on pvetest1 at main f002db9 (waits behind the night runs);
  held `--after-run 838045bbdd`.
- [x] 2026-11-26: handed back (install hold met). pvetest1 held by the image
  gate (stormcentral#1601), pvetest2 refuses runs (sbregistry :5100 failing),
  pvetest3 in a gate test. `rigs` be3c77b765 at 8a356d3 (carries 5b67427, and
  #343's 1c0b6bb) waits in pvetest1's line; item held `--after-run be3c77b765`.
- 2026-11-26 (later): `rigs` be3c77b765 errored — infrastructure (pvetest1's
  sbregistry got 502 from forge pulling the test image). Then no pve machine
  takes a run: pvetest1 and pvetest3's 14.08 installs failed (no known
  release); pvetest2 refuses (sbregistry :5100 failing). Held
  `--after-install 'stormcos>=f7f7ccb@any:pve'` (a fresh passed pve install),
  then rigs at main.
- Not ours: stormpump writes the token Secret, stormcert approves by policy
  (both filed). Done-when (stormcentral's fake-free test) is stormcentral's.
- 2026-11-26 22:15 (owner's order via the master): until Tuesday 2026-12-01
  only forge1's path runs on stormcos; on rustkube that is #388 alone. Code
  and units are done (5b67427); what is left is the `cluster-info` rig,
  which rides `rigs` 838045bbdd (pvetest1, main f002db9). pvetest2 and
  pvetest3 fail /readyz for every component, and rigs never run in a build
  slot (#173), so no faster path exists. Close on `cluster-info` passing.

### security.openshift.io SCCs and the PodSecurityPolicy reviews (#373, sc#151) — CODE DONE, WAITING on rigs 838045bbdd 2026-11-21
Ported from openshift/apiserver-library-go `sccmatching`, `sccadmission`,
`sccdefaults`, `util/sort` and openshift-apiserver's three review REST
handlers (read 2026-11-21).
- [x] `security.openshift.io/v1` `securitycontextconstraints` (short name
  `scc`), cluster-scoped, generic handlers; OpenShift's defaulting
  (`SetDefaults_SCC`) and validation on every write; the CRD's printer
  columns for `oc get scc`.
- [x] `anyuid`, `restricted-v2`, `privileged` bootstrapped create-only
  (`release.openshift.io/create-only`), OpenShift's fields (volumes keep
  rustkube's `image`). ClusterRoleBinding `system:openshift:scc:restricted-v2`
  → `system:authenticated`, create-only, as OpenShift grants it.
- [x] Admission runs on the stored SCCs: providers from each SCC with the
  namespace's preallocated UID / group ranges, sorted by priority, then
  restrictiveness points, then name; SA first, then requester, by the SCC's
  users/groups or RBAC `use`; first that admits wins; `openshift.io/scc` +
  `security.openshift.io/validated-scc-subject-type`; `openshift.io/required-scc`.
  The strategies (runAsUser, fsGroup, supplementalGroups, capabilities,
  seccomp incl. the pod annotation, sysctls, volumes, host*, readOnlyRootFS,
  privilege escalation) as upstream, messages and field paths included.
  Deviation kept from #321: no MCS levels — a `MustRunAs` SELinux strategy is
  enforced only when the SCC names a level itself.
- [x] `podsecuritypolicyreviews`, `…selfsubjectreviews`, `…subjectreviews`
  (namespaced, create only), answered by the same providers, with
  OpenShift's quirks (subject review keeps the failure reason; template only
  when the pod names a ServiceAccount). `admin` may create them.
- [x] README, docs/oc-compatibility.md, CHANGELOG (fbf75dd).
- [x] 6386b61 + fixes (compile, dead code, clippy) to a2e5883. sc-build-1f54b46fff:
  workspace build + every test (apiserver 422, incl. 21 new); clippy clean
  in the touched files.
- [ ] Rigs `scc-restricted` (custom SCC by priority, invalid SCC 422, subject
  review) and `oc-adm` (`get scc`, `describe scc`, `scc-subject-review
  -u/-g/self`, `scc-review -z`), suite `rigs`, at a2e5883. Runs 45e67c79d8
  (pvetest2) and 0d66141b1e (pvetest3) ended `infrastructure`, as pvetest1
  for #372 (stormcluster/stormlb APIs down on every pve test machine,
  stormcluster#128). Then `stormcentral test run rustkube rigs --tag
  <machine> --commit <main>`; close #373 when both rigs pass.
- 2026-11-23: main 9c84433 (which carries #373 plus #388) passed sc-build
  c5c5f4e065: workspace build and `cargo test --workspace` (apiserver 424/424,
  every SCC unit among them). Rigs still blocked: `stormcentral test run
  rustkube rigs --tag pvetest1|2|3` is refused (400, the machine's apiserver
  not answering); stormcluster#128 open, f1de3f6 not installed.
- [x] Held with `stormcentral propose --after-install
  'stormcluster>=f1de3f6@any:pve'` (item 233220744ec5): it returns once a pve
  test machine passes an install carrying the stormcluster#128 fix.
- [x] 2026-11-26: `rigs` cb87060b01 queued on pvetest2 at main 455564b
  (carries a2e5883); held `--after-run cb87060b01`. Close when both rigs pass.
- [x] 2026-11-26 (night): cb87060b01 was interrupted (stormcentral restarted
  at 'wait for the apiserver'); no test ran. Held `--after-run 0e2114d89b`
  (`rigs`, pvetest2, main 9a61694, carries a2e5883).
- [x] 2026-11-27: `rigs` 0e2114d89b (pvetest2) errored before any test:
  pvetest2's apiserver did not answer /readyz in 10 min, as every component's
  runs there now (flowsdn, stormvm, rustkube-node). Re-queued `rigs`
  838045bbdd on pvetest1 at main f002db9 (waits behind the night runs);
  held `--after-run 838045bbdd`.

### user.openshift.io Groups (#372, sc#151) — CODE DONE, WAITING on rigs 838045bbdd 2026-11-21
- [x] Serve `user.openshift.io/v1` `groups` (cluster-scoped) through the generic
  cluster handlers: discovery (`/apis` + the group-version), list kind
  `GroupList`, apiVersion `user.openshift.io/v1`, apply table, table printer
  (NAME, USERS), `users: null` when absent, as OpenShift serializes it.
- [x] Validation as openshift/apiserver `ValidateGroup`: name a path segment,
  no `:`, not `~`; each user non-empty, a path segment, no `:` — except a
  ServiceAccount user name `system:serviceaccount:<ns>:<name>` (the issue:
  sc's users are `storm-users/<name>` SAs and must be listable).
- [x] Authentication adds a Group's name to the groups of every user it lists
  (OpenShift's group cache), from a watch-cache view of `/registry/groups/`.
  So a RoleBinding to the Group grants its members; SSAR shows it.
- [x] Side fix found on the way: Route lists said kind `routesList`,
  apiVersion `v1` (routes missing from the list-kind tables).
- [x] Rig `oc-adm` (not a new rig): `oc adm groups new/add-users/remove-users`,
  output as OpenShift's; a binding to the group grants its member (SSAR).
- [x] docs/oc-compatibility.md, README, CHANGELOG.
- [x] 67f863a, 2538deb, a709666. sc-build-6dcbb0f04d at a709666: one unit
  failure, the 2538deb apiVersion check applied to `pods/eviction` in
  policy/v1 (a subresource; #380). The check now covers top-level resources only.
- [x] d9c470e: sc-build-228a972e55 passed — workspace build and every test
  (sc-build closed #380 itself).
- [ ] Rig `oc-adm` (suite `rigs`): run f4c348536f (pvetest1, d9c470e) ended
  `infrastructure` — stormcluster/stormlb APIs down, as on every pve test
  machine since 2026-11-20 13:21Z (stormcluster#128, fix f1de3f6 not
  installed). Then `stormcentral test run rustkube rigs --tag <that machine>
  --commit <main>`; close #372 when oc-adm's Groups checks pass.
- 2026-11-23: re-built at main 9c84433 (sc-build c5c5f4e065): workspace
  build and every test pass, the Groups units by name (`user_groups`,
  `auth::a_group_listing_the_user_is_among_its_groups`,
  `table::groups_print_their_users`). The rig is still blocked:
  `stormcentral test run rustkube rigs --tag pvetest1|2|3` is refused (400,
  the machine's apiserver not answering); stormcluster#128 open, f1de3f6
  not installed.
- [x] Held with `stormcentral propose --after-install
  'stormcluster>=f1de3f6@any:pve'` (item 0ea8542924f5): it returns once a pve
  test machine passes an install carrying the stormcluster#128 fix.
- [x] 2026-11-26: `rigs` cb87060b01 queued on pvetest2 at main 455564b
  (carries d9c470e; `oc-adm` in the day list); held `--after-run cb87060b01`.
- [x] 2026-11-26 (night): cb87060b01 was interrupted (stormcentral restarted
  at 'wait for the apiserver'); no test ran. Held `--after-run 0e2114d89b`
  (`rigs`, pvetest2, main 9a61694, carries d9c470e).
  Close on oc-adm's Groups checks.
- [x] 2026-11-27: `rigs` 0e2114d89b (pvetest2) errored before any test:
  pvetest2's apiserver did not answer /readyz in 10 min, as every component's
  runs there now (flowsdn, stormvm, rustkube-node). Re-queued `rigs`
  838045bbdd on pvetest1 at main f002db9 (waits behind the night runs);
  held `--after-run 838045bbdd`.

### manifests.rs review findings (#378: 96a66790f5c4, f25b22881436) — DONE 2026-11-20
- [x] 96a66790f5c4: the inventory is not keyed by the directory's path; a
  moved `--manifest-dir` keeps the previous entries and prunes them once the
  new directory is complete (the stored path is informational).
- [x] f25b22881436: an existing directory with no manifests prunes what the
  inventory recorded; an absent or unreadable directory, or one whose every
  manifest is a dangling link (its pallet not mounted), prunes nothing.
  An empty directory with no inventory writes none.
- [x] 62d6dbf + 2e clippy rename. sc-build-1c0cf7a236: workspace build +
  all tests passed (apiserver 398), manifests 15/15; sc-build-846e5f636f:
  15/15, clippy nothing in manifests.rs. README/help/CHANGELOG.

### scc.rs review finding (#377: 5a63ec20536a) — DONE 2026-11-20
- [x] Namespace create admission refuses an existing name before taking a
  block (`scc::ranges_for_new_namespace`).
- [x] A block taken for a create that then fails with a 4xx is given back
  (`scc::create_failed` → `release_block`: `next` steps back, else the
  record's `released` list, handed out first). 5xx keeps it.
- [x] Call sites: cluster POST, ProjectRequest, manifests, lazy backfill
  (CAS conflict). Units `an_existing_name_takes_no_block`,
  `a_failed_create_gives_its_block_back`. sc-build 9ab99c4 passed; clippy:
  nothing in the changed lines (older lints: #376, #370). README/CHANGELOG.
- [ ] Rigs (`projects`, `scc-restricted`) at 9ab99c4: runs 44f39d98cf /
  58c7131e18 errored at the same machine API-health gate as #371's.

### auth.rs review finding (#371: c6cb83387661) — DONE 2026-11-20
- [x] A reload retires only the previous *signing* key's verifier, and only
  when the signing key changed (its tokens verify against none of the new
  public keys), for 24 h (`RETIRED_SIGNER_OVERLAP_SECS`). Every other key
  dropped from the files stops verifying at once. Unexpired retired keys
  carry over; expired ones go; the log names each kept key's deadline.
- [x] Units `a_removed_verification_key_is_revoked_at_once`,
  `a_signing_key_change_keeps_only_the_replaced_signer`, overlap end in
  `a_replaced_pair_is_used_without_a_restart`. sc-build 1988068: build + all
  tests passed, clippy nothing in auth.rs.
- [x] Rig `sa-key-reload` rewritten (added key works, replaced signer kept,
  removed key 401). README/CHANGELOG. rustkube-node#462 (refresh after 24 h,
  as upstream; the overlap relies on it).
- [ ] Rig runs b5e196e1c7 (pvetest1) / e27515ec26 (pvetest2) at 1988068
  errored at the machines' API-health gate (stormcluster :9102, stormlb
  :9111 — stormcluster#56, stormlb#30). Rerun `stormcentral test run
  rustkube rigs --tag pvetest1 --commit <main>` when a machine answers; it
  covers `sa-key-reload` (#371) and `daemonset-nodes` (#369).

### DaemonSet rolling update (#369, stormcos#811) — SHIPPED (proof pending) 2026-11-20
- [x] Pods carry `controller-revision-hash` (FNV of `spec.template`, the
  Deployment's hash) and `pod-template-generation`.
- [x] `RollingUpdate` (default): old-hash pods replaced, at most
  `maxUnavailable` (default 1, % rounds up) nodes unavailable; unavailable
  old pods replaced freely; `maxSurge` > 0 creates the new pod beside the
  old and deletes the old once the new is Ready. `OnDelete` leaves them.
- [x] `updatedNumberScheduled` counts nodes with a current-hash pod; status
  counts per node.
- [x] Units on the pure plan (8 new); rig `daemonset-nodes` gains a
  template change (Ready pods, one at a time; OnDelete keeps its pod).
- [x] sc-build 0d1c85c: build + all tests passed, no clippy finding in
  daemonset.rs. README/CHANGELOG. minReadySeconds gap filed: #379.
- [ ] Rig run: refused on pvetest1 and pvetest2 (node API-health gate:
  stormcluster#56, stormlb#30 — not rustkube). Rerun
  `stormcentral test run rustkube rigs --tag pvetest1 --commit 0d1c85c9549f`
  once a machine answers. Shipped with a node proof (pods labelled).

### manifests.rs review findings (#368: de2ea73c2f21, 89bec8a64dff) — DONE 2026-11-20
- [x] de2ea73c2f21: a document in a Namespace queued for retry is deferred
  behind it (kept in the retry queue, not attempted), so NamespaceLifecycle's
  NotFound cannot drop it. Unit with `MemStore::fail_next_put` on the
  Namespace.
- [x] 89bec8a64dff: prune only after every document applied (first pass, or
  the background retries ending with all applied, none refused); otherwise
  the inventory records previous ∪ named and deletes nothing.
- [x] sc-build 9f0d7e6: build + all tests passed; 12/12 manifests units by
  name; clippy: no warnings in the changed files (the older apiserver ones
  are #376, #370). Staged. Closed.

### quantity.rs review findings (#367: 4de4faf81751, 986d36e631b7) — DONE 2026-11-20
- [x] 4de4faf81751: `parse_cpu_millis` takes SI suffixes — fixed in 3c7a66b
  (#365); added the quota round trip (`cpu: 1000` stored `"1k"`, quota and
  Pod usage 1,000,000m) beside #365's scheduler one.
- [x] 986d36e631b7: `numbers_to_strings` never enters `opaque.parameters`
  or a claim's `status.devices[].data`; array items sit under `[]`, not the
  array's name (a DRA request's `count` stayed under "requests").
- [x] sc-build 4c20724 passed (build, all tests, clippy -D warnings on
  apimachinery). Staged. Closed.

### resource.rs review findings (#366: f8ad6d24b615, fc9c16b6b9e0, f33be8cc42cb) — DONE 2026-11-20
- [x] Job `managedBy` re-checked in `admission::after_mutating` (create,
  update; not `/status`). Unit + rig `admission-webhook` (/jobbad, /jobmove).
- [x] Server-assigned `uid`/`creationTimestamp` on every API create (POST,
  apply-create, CR create, events.k8s.io), kept across admission; token
  regression (recreate with the old uid does not revive the token).
- [x] `/finalize`: body uid/resourceVersion preconditions (409), the observed
  revision as CAS for the update and the delete; the namespace controller
  returns the finalize error so it is retried.
- [x] sc-build 9052ddb: build + all tests passed (the new units included);
  clippy -D warnings on apiserver/controller-manager fails on older lints
  only (#376, #370). Rigs `admission-webhook`, `bound-token`,
  `namespace-finalize` (day suite) extended/added, not yet run on a test
  machine. Staged. Closed.

### Scheduler request accounting (#365, review 93be2afaffe2, aaa4c9a1bf6c, cc73c69e0514, a5b072e00d6c) — DONE 2026-11-20
- [x] filter.rs: `pod_request_map` — statuses matched by name,
  `allocatedResources` read as the map, per-container max of
  spec/actuated/allocated aggregated via `pod_resources::aggregate`;
  pod-level requests override only cpu/memory/hugepages keys they name;
  JSON numbers read (also node allocatable and score.rs).
- [x] quantity.rs: `parse_quantity`, `cpu_millis`, `base_units`;
  `parse_cpu_millis` takes SI suffixes (`1k`).
- [x] pod_count_filter reads `allocatable.pods` as a quantity (`1k`).
- [x] Units (`request_tests`, `canonical_forms_and_numbers_read_back`);
  sc-build 3c7a66b passed (build, tests, clippy -D warnings on scheduler +
  apimachinery). Staged. Closed.

### CronJob failed Job create consumes the run (#364, review f522089c5539) — DONE 2026-11-20
- [x] cronjob.rs: `create_job` returns the error; AlreadyExists accepted
  only for a Job owned by this CronJob; Job named `<cj>-<unix minutes>` of
  the scheduled time; `lastScheduleTime` written only after.
- [x] Units against a fake apiserver (503 then 201; 409 ours; 409 not ours).
- [x] sc-build 031a035 passed (5 cronjob units, workspace tests); no clippy
  lint in cronjob.rs (the crate's others are #370). Staged. Closed.

### StatefulSet lowest missing ordinal (#363, review c1b3be631b68) — DONE 2026-11-20
- [x] statefulset.rs: `plan` (Create lowest missing / WaitFor unready lower /
  Delete highest condemned / Settled), `ordinal_of` (`<sts>-N` only), a
  finished Pod deleted first.
- [x] Units; rig `statefulset-ordinals` (day suite).
- [x] sc-build 3c86abc passed; no lint in statefulset.rs. Staged. Closed;
  `statefulset-ordinals` runs with the next `rigs` run.

### Service endpoints: named ports, readiness (#362, review 7a974852b70e, 7d20cd929fba, 7ada61477e9f) — DONE 2026-11-20
- [x] service.rs: `backends` (upstream inclusion, readiness, per-pod
  `resolve_port`, groups by port set); Endpoints subsets with
  notReadyAddresses; a slice per port set (first keeps the Service's name,
  others `<svc>-<fnv8>`), stale ones deleted; empty placeholder slice.
- [x] Units; rig `service-endpoints` (day suite).
- [x] sc-build 245004c passed; no lint in service.rs. Staged. Closed;
  `service-endpoints` runs with the next `rigs` run.

### PodSecurity on pod updates (#361, review 6fcd683fb638) — DONE 2026-11-20
Reviewed at ee19cd5, before #350's `pod_update` (which already freezes every
field `pod_security` reads). Added upstream's update rule anyway:
`pod_security_update` in `builtin_update` (no subresource) when
`pod_security_significant` (images, container count, ephemeral containers,
seccomp annotations), against the namespace's current enforce level.
- [x] Code + units (`pod_security_update_tests`), rig `scc-restricted`.
- [x] sc-build d2c87f1 passed; no new lint. Stage 73c690bfefaa. Closed.

### Concurrent evictions vs PDB (#360, review 6d98e18d1259 + b8d6824b0231) — DONE 2026-11-20
- [x] eviction.rs: reserve in PDB status by `guaranteed_update` CAS
  (`reserve_in`: processed? allowed > 0? decrement + disruptedPods[pod]);
  release on a failed delete; upstream's canIgnorePDB, one-PDB rule,
  unhealthy-pod policy, matchExpressions.
- [x] pdb.rs: `live_disruptions` — recorded pods not healthy until gone or
  120 s; terminating pods not healthy; requeue at expiry.
- [x] Units (reserve/release, unprocessed, multi-thread race through
  `create_eviction` for budgets 1 and 3, two-PDB 500, unready path; controller
  expiry). Rig `pdb-eviction` (day suite).
- [x] sc-build 46622a0 passed; no lint in eviction.rs/pdb.rs. Stage
  80835fce5908. Closed; `pdb-eviction` runs with the next `rigs` run.

### A refused write's reason in events (#340) — DONE 2026-11-20
- [x] `apimachinery::api_status`: `check(response)` → `ApiStatusError` (code,
  Status reason/message, URL; Display = the message); `status_of` for code
  checks (runner's create, replicaset's `gone`).
- [x] Replaced every `error_for_status()`: controller-manager runner, gc,
  persistentvolume, service; scheduler client; reflector LIST/WATCH.
- [x] ReplicaSet FailedCreate event: `Error creating: <reason>` (upstream's).
- [x] sc-build 3bf3ae1 passed (all tests; no new controller-manager lint).
  Stage b43516a11f2f. Closed.

### RS/RC adoption and release (#295, P2) — WAITING on rigs 838045bbdd (2026-11-20)
- [x] `replicaset::claim` (keep / release / adopt) and `claim_pods` (merge
  patches with the Pod's resourceVersion; uncached owner read before adopting;
  a refused write fails the pass rather than making a replacement).
- [x] Pods dependency routed by `pod_membership`; `pod_selector` covers
  ReplicaSet and ReplicationController (RC selector defaults to template labels).
- [x] Units (claim, selectors), rig `replication-controller` (RC + RS).
- [x] sc-build caa709b passed (all tests); a1ffe89 (a clippy lint, #356)
  controller-manager tests passed. Stage 1931fbd6b23b.
- [ ] Rig `replication-controller` with the next `rigs` run. Proposed
  `--after-install stormcluster>=f1de3f6`; then run `rigs` on main and close
  (#297 waits on the same run).
  2026-11-26: stormcluster's fix is installed, but `rigs` is refused on all
  three pve machines (pvetest1/pvetest3 14.08 installs failed; pvetest2
  sbregistry :5100 failing). Held --after-install stormcos>=f7f7ccb@any:pve
  with #343/#388/#96; one `rigs` run at main covers #295, #343 and #388.
  2026-11-26 (night): `rigs` cb87060b01 (pvetest2) was interrupted
  (stormcentral restarted at 'wait for the apiserver'); no test ran. Held
  `--after-run 0e2114d89b` (`rigs`, pvetest2, main 9a61694, carries caa709b;
  shared with #343/#388). Close on `replication-controller`.
- [x] 2026-11-27: `rigs` 0e2114d89b (pvetest2) errored before any test:
  pvetest2's apiserver did not answer /readyz in 10 min, as every component's
  runs there now (flowsdn, stormvm, rustkube-node). Re-queued `rigs`
  838045bbdd on pvetest1 at main f002db9 (waits behind the night runs);
  held `--after-run 838045bbdd`.

### Pod updates around SCC (#359, P0, review 198fa7c65883) — DONE 2026-11-20
The review ran at 112d7e1 (2026-11-18), before #350's fix 3cef5d0
(`builtin_admission::pod_update`, upstream's `ValidatePodUpdate`, in
`builtin_update` for every client write and again after mutating webhooks).
Audited: `/status` PUT and PATCH put the stored spec back; `/resize` may
change only resources/resizePolicy; `pods/ephemeralcontainers` is not served
and `ephemeralContainers` through PUT/PATCH is a spec change, refused.
AppArmor annotations (mutable here, immutable upstream) cannot escalate:
rustkube-node applies no profile and refuses a pod asking for one. Added
update checks to the `scc-restricted` rig.

### Continue tokens outside the prefix (#358, P0, review e59512281f8c) — DONE 2026-11-20
The review ran at ee19cd5, before #350's fix 7e729d2 (adapter `list_at` and
apiserver `list_pinned` refuse a continue key not starting with the prefix:
`Invalid` / 400). Audited the other token paths: the watch cache's `list`
holds one resource; `read_page` is called with no key and stops at the
sub-prefix; every store read goes through the adapter's check. Unit tests
passed in sc-build dfcd941. Added a `compaction` rig check (live 400).

### CSR client-supplied Approved status (#357, P0, review 27b84965dcae) — DONE 2026-11-20
The review ran at ee19cd5 (2026-10-14), before #350's fix (1097827: `on_create`
empties `status`). Main still had a way around it: a server-side apply create
whose body names another object than the URL skipped admission
(`admission::admit` matches the object by name) and was stored at the URL's
key with the client's status and spec.
- [x] `url_identity` (from `put_object`) checked on the apply-create path: 400 (dfcd941).
- [x] Unit (resource.rs status tests) + `csr-signers` rig: forged Approved on
  POST and on apply create dropped and unsigned; mismatched apply 400.
- [x] sc-build dfcd941 passed (all tests; apiserver clippy: no new lint, #356).
  Stage 97e53ab5e952. Closed. The rig additions run with the next `rigs` run
  (pve test machines down, stormcluster#128). #297 back in line.

### Resource fit counts every scalar resource (#297) — WAITING on install stormcentral>=78c9a8e (item 36c0c41666c1) for the rig (2026-11-26)
Found by #84's SchedulerPreemption conformance spec (`scheduling.k8s.io/foo`).
- [x] `NodeUsage.scalar` (6b9875e): ephemeral-storage, extended resources,
  hugepages, charged/uncharged from one rule (`filter::pod_scalar_requests`:
  containers with init/sidecar rules, pod-level, overhead, status max).
- [x] Resource fit: `Insufficient <name>`; an extended resource the node does
  not advertise is 0 (upstream); ephemeral-storage checked only when the node
  reports it, as cpu/memory are. Preemption inherits it (same filters).
- [x] Units (5 filter + 1 preemption on `scheduling.k8s.io/foo`). Rig
  `extended-resources` in the day suite (24be4ee).
- [x] sc-build 165db40 passed: build, all tests (scheduler 72), clippy -D
  warnings on apimachinery + scheduler. Along the way: MSRV 1.75 → 1.82
  (#354 closed), apimachinery lints (#355 closed), scheduler lints; the
  apiserver/controller-manager lints are #356 (open, commented).
- [x] Stage requested: build 66b75601b500.
- [ ] Rig `extended-resources`: runs 2882f2953e (pvetest2) and 0b04739c2f
  (pvetest1) ended `infrastructure` (stormcluster/stormlb APIs down on every
  pve test machine, stormcluster#128). #128's fix is stormcluster f1de3f6, not
  in 12.97: proposed `--after-install stormcluster>=f1de3f6` (19:19Z). Then
  `stormcentral test run rustkube rigs --tag <that machine> --commit <main>`,
  which also runs #357/#358/#359's rig checks; close.
- [ ] 2026-11-25 23:53 CST, main 0607f8b: run 2f7267c2d4 (pvetest1) ended
  `infrastructure` — pvetest1's sbregistry (:5100) refused the golden GET.
  pvetest2 refused (last install 13.00-flowsdn failed, no known release);
  pvetest3 refused (14.06 install still running). No pve machine can take
  the run; rerun `rigs` at main on the first one that runs a known release.
- [ ] 2026-11-26 00:2x CST, main 25deb6d: pvetest3 (14.06 cilium) run
  4e253bf40e ended `infrastructure` — stormrdp `gateway` API probe HTTP 503
  since 06:01Z (filed stormrdp#70). pvetest1 (14.06-flowsdn) refused: held
  for its failing sbregistry :5100 (stormcentral#1798). Rerun when either clears.
- [ ] 2026-11-26 08:02Z: stormcentral#1798 closed (fix 78c9a8e) but pvetest1
  and pvetest2 still refuse runs for :5100. Proposed item 36c0c41666c1
  `--after-install stormcentral>=78c9a8e@any:pve`; rerun `rigs` then.

### Review findings, 5 critical (#350) — DONE 2026-11-20
One commit per finding, the finding id in each; a unit test each.
- [x] 198fa7c65883 (3cef5d0) — a Pod update (PUT/PATCH/apply, any requester) may
  change only what upstream's `ValidatePodUpdate` allows (images,
  activeDeadlineSeconds down, tolerations added, gates removed, the bind
  setting `nodeName` once); resources only through `/resize`; the
  `kubernetes.io/config.mirror` annotation never changes. Re-checked after
  the mutating webhooks. Internal writes (manifests, no request) exempt.
- [x] f5ae1c953cf4 (dd4b24f) — the mirror annotation exempts a pod from SCC only when
  its node creates it: requester in `system:nodes`, `system:node:<n>` with
  `spec.nodeName == <n>`.
- [x] 27b84965dcae (1097827) — CSR create drops the client's `status`.
- [x] 77f265e40744 (a65ee0f) — kubelet-client auto-approval only for a node renewing
  its own identity (PKCS#10 O=[system:nodes], CN=system:node:<n> ==
  spec.username, requester in system:nodes, no SANs, client-auth usages).
- [x] sc-build 4f6159e passed (cargo build + test, 375 apiserver tests,
  clippy clean in the changed code); 692e762 (a test's clippy lint) passed
  too. 3cef5d0 failed one new test (`pod_update` inserted a null
  `initContainers` through `IndexMut`), fixed in 4f6159e; #352 closed by it.
- [x] Stage requested: build f9d196778b35. Issue closed.
- [x] e59512281f8c (7e729d2) — a continue key outside the listed prefix is a 400
  (apiserver) / `Invalid` (store adapter).

### Own image build: deploy/build-image.sh (#403, P1, stormcentral#1836) — DONE 2026-11-26 (flip is the master's, after stormcos#1665)
- Move the stage recipe for rustkube-apiserver/-controller-manager/-scheduler
  out of stormcos (`deploy/build-goldens.sh` 6875–8715 at stormcos f7f7ccb)
  into this repo, per the component build contract (`deploy/build-golden.sh
  <name> OUT`, kind `tree`; stormblock-registry#273 is the worked example).
- Found: the apiserver golden is mostly stormcos's: 67 `deploy/manifests`
  files baked into manifests.d/network, ~1430 lines of stormd config (≈80
  mints for stormcos services, signers, NODE_IP/VIP), static-pod generators,
  plus stormcert's three binaries and stormd.
- [x] Decision (the master, 2026-11-26): option A — the goldens carry only the
  binary, stormd and a default config; manifests/cert policy/static pods go
  to the stormcos content golden, stormcert binaries from stormcert's golden.
  A golden-content split inside the plan is the master's call, never a stop.
- [x] `deploy/build-image.sh` + `deploy/build-golden.sh` (b56c96a):
  usr/bin/kube-<c>; stormdbase dirs; `/stormd -> .stormd/bin/stormd`
  (stormcentral#1628 phase 3: no stormd input in a tree build); stormd's
  applets (STANDALONE_COMMANDS at stormd d2cf426); default config = the
  component's own [[process]] + API probe with the stage's flags (no
  APISERVER_EXTRA, no cadvisor/VIP/API-TLS edits); empty manifests.d.
  x86_64, aarch64 via zigbuild.
- [x] `deploy/check-image.sh [stormcos]`: the three trees' shape, nothing of
  stormcos/stormcert, reproducible bytes, unknown name/arch refused, and
  stormcos's gen-static-pod.py on each giving the stage's command + args.
- [x] sc-build 20261126-214943-b56c96ab passed (318 s, fresh VM): three trees
  2 files each, reproducible, refusals; against stormcos f7f7ccb
  gen-static-pod.py gives the stage's command + args (36/14/12).
- [x] stormcos#1665 filed for its half (manifests mount, cert plane,
  stormcert binaries path, static pods, the stage deletion after the flip);
  handoff on stormcentral#1836 (one tree row per golden, or per-golden runs).
- Flip (the master's, not this repo's): `rustkube` row special → tree once
  stormcos#1665 lands and a release proves it on pve.

### Protobuf /openapi/v2 (#343) — WAITING on rigs 838045bbdd (pvetest1, main f002db9) (2026-11-19)
- `GET /openapi/v2` under client-go's `Accept:
  application/com.github.proto-openapi.spec.v2@v1.0+protobuf` answered JSON;
  client-go failed `proto: cannot parse invalid wire-format data`
  (`oc explain vm -o plaintext-openapiv2`, pvetest1, 12.95).
- [x] Vendor gnostic-models v0.7.0 `openapiv2/OpenAPIv2.proto` into the
  descriptor set; `apimachinery::openapi_v2` negotiates the Accept and
  encodes the Swagger JSON as `openapi.v2.Document` (extensions/defaults/
  enums as `Any.yaml`, as gnostic); 406 when nothing acceptable.
- [x] Unit tests (negotiation, decode of the encoded message, the served
  document with CRDs). Rig `crd-openapi`: protobuf answer, 406, `kubectl
  explain --output=plaintext-openapiv2`.
- [x] sc-build ec978d7 passed; clippy clean in the changed files; stage 7fa5a92f9f2b requested.
- [x] `rigs` c04777d587 (pvetest1, ec978d7): 73/74; `crd-openapi` 14/16 —
  both `explain --output=plaintext-openapiv2` checks failed `mime: unexpected
  content after media subtype`: we answered `Content-Type: …spec.v2@v1.0+protobuf`;
  Go's `mime.ParseMediaType` refuses `@` (tspecial). Upstream answers the
  dotted `…spec.v2.v1.0+protobuf` whatever spelling was asked.
- [x] Serve the dotted spelling (`openapi_v2::CONTENT_TYPE`; `ACCEPT_TYPE` is the `@` one), rig expects it — 1c0b6bb; sc-build passed; stage build 4d697378aee0.
- [ ] `rigs` at 1c0b6bb: no machine could take it (pvetest1/2 mid-install of 12.97). Held `--after-install rustkube>=1c0b6bb@any:pve`; then run rigs, close on crd-openapi 16/16.
  2026-11-26: pvetest1 runs 14.07-flowsdn (passed); `rigs` 52d5a95ee4 queued there at 8a356d3 (carries 1c0b6bb). pvetest3's stormblock is wedged (c1989dd262: apiserver no /readyz, ublk writes unanswered ~10000s). 52d5a95ee4 sat queued: pvetest1 is held by the image gate (failed golden-stormdrive revert, stormcentral#1601); cancelled it. pvetest2 refuses runs (sbregistry :5100 failing). Re-queued `rigs` be3c77b765 on pvetest1 (waits in its line); item held `--after-run be3c77b765`.
- 2026-11-26 (later): `rigs` be3c77b765 errored — infrastructure (pvetest1's
  sbregistry got 502 from forge pulling the test image). Then no pve machine
  takes a run: pvetest1 and pvetest3's 14.08 installs failed (no known
  release); pvetest2 refuses (sbregistry :5100 failing). Held
  `--after-install 'stormcos>=f7f7ccb@any:pve'` (a fresh passed pve install),
  then rigs at main.
- 2026-11-26 (night): `rigs` cb87060b01 (pvetest2, 455564b) was interrupted —
  stormcentral restarted during it (at 'wait for the apiserver'); no test ran.
  Re-queued `rigs` 0e2114d89b on pvetest2 at main 9a61694 (carries 1c0b6bb);
  held `--after-run 0e2114d89b`. Close on crd-openapi 16/16.
- [x] 2026-11-27: `rigs` 0e2114d89b (pvetest2) errored before any test:
  pvetest2's apiserver did not answer /readyz in 10 min, as every component's
  runs there now (flowsdn, stormvm, rustkube-node). Re-queued `rigs`
  838045bbdd on pvetest1 at main f002db9 (waits behind the night runs);
  held `--after-run 838045bbdd`.
- Built-in kinds still have no v2 definitions (upstream-feature-inventory:
  empty schemas), so `explain pod -o plaintext-openapiv2` finds no schema —
  that is the built-in-schemas gap, #342, not this issue.
- sc-build at ec978d7 passed (sc-build `cargo build && cargo test`). The
  first runs: c092306 missed a trait import (#345), its test named two CRDs
  alike (#346), `is_none_or` is past MSRV (#349; the other MSRV and clippy
  hits there are #263's). `rigs` run c04777d587 on pvetest1 at ec978d7.

### Pods keep their template's annotations (#256, P2; nextnfs-operator#2, stormcos-options#26) — COMPLETE 2026-11-19 (ships in a release)
- `build_pod_from_template` (ReplicaSet/RC) copies only `labels` + `spec`;
  DaemonSet, Job and StatefulSet pod builders do the same. Upstream
  (`GetPodFromTemplate`) copies labels, annotations and finalizers. Effect:
  stormoptions' `options.storm.io/golden-digest` never reaches the operator
  pod, so no option becomes Ready; `kubectl rollout restart`'s
  `restartedAt` is lost too.
- [x] `owned::copy_template_metadata` copies `annotations` and `finalizers`;
  used by all four builders; unit tests per builder. f19424c.
- [x] README/CHANGELOG 09ef47e. Rig `replication-controller`: RC pods carry
  the template annotation; a Deployment's pod (via its RS) carries
  `options.storm.io/golden-digest`, and `restartedAt` after a restart
  patch. 06a126f.
- [x] sc-build sc-build-88d86b1b13 at f19424c: controller-manager 125
  passed, no clippy hits in the changed files (cronjob.rs's is #263).
- [x] `rigs` at 06a126f: 082dfba180 (pvetest1) replication-controller 19/19;
  5e29dd0334 (pvetest2) 18/19 with the old orphan-delete check flaking
  (filed #339), the five #256 checks passing. Other failures not #256's:
  status-put-latency (stormpump#153), hpa-metrics / vm-runstrategy (#336).
- [x] First ship-proof failed, not the fix: it asked for nfsop pods on
  pvetest2 (12.96-flowsdn), which carries the fix but has no option
  installed (`nextnfs-system` empty). Read 2026-11-19: pvetest2's
  controller-manager is stage 401979970229 (the fix), and no workload there
  has template annotations. pvetest1 runs 12.95, without the fix, and its
  `cilium` pod lacks the template's `kubectl.kubernetes.io/default-container`.
  Re-shipped `--commit f19424c` with a proof every cilium install has a
  subject for: `on pvetest1` (cilium flavor) kube-system pods carry
  `"kubectl.kubernetes.io/default-container":"cilium-agent"`.

### Registry roles: registry.storm.io (#333, P0, stormblock-registry#187) — COMPLETE 2026-11-18 (ships in a release)
- The owner chose a namespaced CRD `registry.storm.io/v1 Image` for the
  registry's object (stormblock-registry#187). Add the group to #325's roles:
  - [x] `system:registry`: `images` get/list/watch/create/update/patch,
    `images/status` update/patch; `registry-viewer`: read; `registry-editor`:
    get/list/watch/create/update/patch/delete; `view`/`edit`/`admin` as for
    image streams (edit holds both registry roles, so admin may bind them). 02a9b41.
  - [x] unit tests; README, CHANGELOG; sc-build bfd9284638 at 02a9b41: apiserver
    366 passed, 0 build warnings, no clippy hits in project.rs (workspace
    clippy debt is #263; #334/#335 closed as its duplicates).
  - [x] rig `registry-roles` extended: the Image CRD with printer columns,
    `kube-system/registry` bound to `system:registry`, its SARs (images/status),
    `oc get images -n shop`. `rigs` run b992b078e1 (pvetest2) at 02a9b41:
    registry-roles 49/49. Other failures not #333's: status-put-latency
    (stormpump#153), hpa-metrics and vm-runstrategy (passed on pvetest1 at
    860029e; filed #336).
  - Binding `kube-system/registry` to `system:registry` is stormcos#1043.

### Registry roles (#325, P0, stormblock-registry#188) — COMPLETE 2026-11-18 (ships in a release)
- TokenReview, SubjectAccessReview, SelfSubjectAccessReview and
  LocalSubjectAccessReview already exist (#85, #59, #69); a `sc user create`
  user is a ServiceAccount in `storm-users`, which TokenReview already answers.
- [x] ClusterRoles as OpenShift's bootstrap policy: `registry-viewer`,
  `registry-editor`, `system:image-puller`, `system:image-pusher`,
  `system:registry` over `image.openshift.io` (+ legacy `""`) imagestreams,
  `imagestreams/layers` (get = pull, update = push), images, tags.
- [x] `view` reads image streams, `edit` holds `registry-editor`, so a project
  admin may bind every registry role in their project (escalation, #98).
- [x] unit tests: the roles answer the registry's SARs; admin holds them.
- Not here: the per-namespace `system:image-pullers` binding OpenShift's
  controller makes (needs RBAC create in the controller-manager, against #176):
  filed as #326. sc's `add-role-to-user <user>` must name the storm-users
  ServiceAccount: sc#95.
- [x] sc-build 112d7e1 passed (new unit tests ran). Image requested.
- [x] rig `registry-roles` in `rigs` run 8d9b1b7fa4 (pvetest1): 31/34. All
  SAR/TokenReview checks passed; the 3 failures were the rig's: the describe
  grep wanted Kind and Name on one line, and `can-i …/layers` named a resource
  NAME, not the subresource, against a group with no discovery (oc then sends
  `imagestreams.image.openshift.io` in the core group → `no`). Fixed: grep per
  line, `--subresource=layers`, a CRD standing in for the registry's
  image.openshift.io (stormblock-registry#187). status-put-latency's failure
  there is stormpump#153 (no /dev/fd in containers), not #325.
- [x] `rigs` run 97a837819d at 860029e (pvetest1): registry-roles 34/34.
  The run's other failures are not #325's: status-put-latency is stormpump#153,
  and namespace-order (8/8 at 112d7e1, with only the rig script changed since)
  is a teardown timing flake, filed as #329 (relists, #274/#273).
- [x] Close-out comment; `shipped` with a ClusterRole proof. The registry's
  own test runs in stormblock-registry#188.

### fetch.sh: partial stage (#323) — COMPLETE 2026-11-18
- [x] A binary with no golden at `<sha>` comes from the newest golden of its image at an ancestor (only when the stage ran at `<sha>`, i.e. some golden is there); MANIFEST names it; the refusal names the missing binary
- [x] Verified: `fetch.sh 89bfa0e` delivered apiserver@89bfa0e + controller-manager/scheduler from bf51ae7's goldens; vm.sh's MANIFEST check passes on conform.g8.lo; `fetch.sh e4225a1` (no golden) still refused

### No stubs (#322, P-1) — COMPLETE 2026-11-18
- [x] Aggregated-API connection upgrades spliced (was 501 NotImplemented); rig check in aggregation.sh
- [x] Test fakes: panicking stubs → `Error::Store`; docs reworded with the gap's issue (#246, #3, stormcos_qa#26) or what replaced it
- [x] Build VM sc-build-9adc90db2e at 89bfa0e: 0 warnings, all unit tests pass (apiserver 365)
- [x] golden-rustkube-apiserver at 89bfa0e on conform.g8.lo: aggregation 0 failed ×3 (rig at e4225a1); the bf51ae7 apiserver fails the 3 upgrade checks (501); scc-restricted 0 failed. fetch.sh refused the partial stage (#323); delivered by hand
- [x] Marker grep empty; gaps: #246, #3, stormcos_qa#26

### restricted-v2 on stormcos (#321, P0, stormstorefront#1 option A) — COMPLETE 2026-11-18 (ships in a release)
OpenShift manifests (no runAsUser/fsGroup, no root) must run unchanged.
- [x] cf375a6 `scc.rs`: per-namespace `openshift.io/sa.scc.uid-range` /
      `sa.scc.supplemental-groups` (1000000000/10000 blocks) from a monotonic
      allocator (`/registry/rangeallocations/scc-uid`, CAS; never reused); on
      namespace create, lazily at pod admission, boot backfill; 75068f6 a
      Namespace update (PUT, reconciled manifest) keeps them
- [x] Pod admission picks the SCC as OpenShift does (anyuid → restricted-v2 →
      privileged, RBAC `use` for requester or pod SA); restricted-v2 sets
      runAsUser/runAsNonRoot/fsGroup/APE false/drop ALL/seccomp, refuses in
      OpenShift's words; `openshift.io/scc`; platform + `enforce: privileged`
      exempt; mirror pods untouched. ClusterRoles `system:openshift:scc:*`
- [x] Units (scc::tests, 10); rig `scc-restricted.sh` (day list);
      README/oc-compatibility/CHANGELOG
- [x] Build VM sc-build-72b1e582dd at 75068f6: 0 build warnings, all unit
      tests pass (apiserver 364); clippy --no-deps on apiserver: nothing in
      the new code (workspace clippy backlog is #263)
- [x] golden-rustkube-*-73e5e502d340 (bf51ae7) on conform.g8.lo:
      scc-restricted 15/15 ×3; oc-adm, projects, resource-quota 0 failed;
      control (apiserver 34a8ee084049) fails 11. `shipped` with an api proof
      (the default namespace's uid-range on a test install)
- Open risk: conformance specs whose controller-made pods pin UID 1000 in
  restricted e2e namespaces are refused now (as on OpenShift); watch #291's
  next run
- Kubelet side (fsGroup on claims/emptyDir) is rustkube-node#171

### ServiceAccount key files followed (#298, P2, stormcos#176) — COMPLETE 2026-11-06
A promote onto a running master (stormcluster#67) or an SNO master join
(stormcluster#21) installs the cluster's SA pair under a running apiserver,
which kept signing/verifying with the node's own key.
- [x] `auth.rs`: keys behind a shared `RwLock` (every clone), `reload`
      (previous public keys kept until the next change; refused when the
      signing key's tokens verify against none of the new keys), saPin
      (sha256 of SPKI) logged; server.rs follows both files via
      `tls_reload::watch_files`. Unit; README/certificates.md/CHANGELOG
- [x] Build VM sc-build-a41b414221 at 9090e15: 0 warnings, apiserver 353,
      clippy clean in auth.rs/server.rs
- [x] golden-rustkube-apiserver-34a8ee084049 (4f59790) on conform.g8.lo:
      sa-key-reload 14/14 ×3; control ebd156a fails 8 (signs with the node's
      key, cluster-key tokens 401, no saPin). #298 closed

### Manifest-dir prune (#236, P1, stormcos#668) + mode from the label (#299) — COMPLETE 2026-11-04
A flavour switch over kept data (cilium ↔ flowsdn on server1) leaves the old
flavour's DaemonSets in ImagePullBackOff: the applier only creates/updates.
- [x] 3089917 #299 `Mode::of`: label first (upstream), then the annotation, then
      `storm.io/manifest-mode`; units; help text, module doc
- [x] 847063d #236 inventory ConfigMap `kube-system/rustkube-manifests` (the dir +
      the Reconcile documents it named); at startup, after the pass, delete
      recorded objects no readable file names any more (dangling link = not
      named) if the stored object is still Reconcile; finalizers/Namespaces
      terminate; no prune when a file is unreadable/unparseable or the dir
      is empty; each deletion logged. Units; rig; docs
- [x] 847063d's build hung an hour (#307, P0 auto-filed): serde_yaml's
      document iterator repeats a syntax error forever, and my unit fed it
      one; ebd156a stops at the first. Build VM sc-build-435774720f: 0
      warnings, apiserver 352 passed, clippy clean in manifests.rs, 80 s
- [x] golden-rustkube-apiserver-4b47213c228d on conform.g8.lo:
      manifest-prune 12/12 ×3; control dd354b3a fails 5 (no inventory,
      nothing pruned). Objects applied before this are not in the inventory
      (server1's leftovers: one manual delete). #236, #299, #307 closed

### server1 reboot: apiserver listens 385–775 s after root ready (#290, P0) — SHIPPED 2026-10-28
Read from server1's boot logs (stormcentral bootlog server1-1792702658,
-1793204384, -1793208481, -1793213309; console + pods.log). Not the
apiserver's own work: fastetcd after a warm reset (unclean stop, SMR disk)
spends 195–376 s in redb's full repair, and on 12.55 its first start never
answered /livez, so the kubelet's 600 × 1 s startup probe killed fastetcd
(and the apiserver, still waiting for it, #293) and both started over —
fastetcd#176, fixed in fastetcd v1.37.0 (early /livez, quick-repair commits).
The apiserver's share after the store answers is ~60–80 s on that disk
(~1–2 s per fsync, fastetcd#135): bootstrap+backfill+CRD load 25–47 s
(not split in the log), manifests 12–40 s — every `Reconcile` manifest
rewritten on every boot (~20: cilium RBAC/ConfigMap, coredns, CSIDriver).
- [x] 297e7e2 per-phase boot timing (`boot phase <name>: N s`; summary
      `boot: serving …` at listen); unchanged Reconcile manifests not
      written; Namespace `spec.finalizers` kept on update (upstream; the
      reconciled cilium-secrets lost `kubernetes` every boot). Units
- [x] README/CHANGELOG (96aee5f); build VM sc-build-4377ac4bf9: workspace
      exit 0, apiserver 348, 0 rustc warnings (clippy hits only in #263's
      older code); golden stage build 440e8d9a9510
- [x] Findings on #290; `shipped`. Next server1 reboot on a release with
      fastetcd v1.37.0 + this golden: read `boot:` from pods.log

### ReplicationController: ReplicaFailure condition (#125, P2) — COMPLETE 2026-10-29
The issue's two specs pass at d5f48f5 (GC "delete pods created by rc when not
orphaning", ResourceQuota "capture the life of a replication controller";
both failed at efbea2d); replication-controller rig 14/14 on four test
machines. sig-apps "ReplicationController should surface a failure
condition … exceeded quota" fails without needing a node: no
`ReplicaFailure` condition (RS and RC share the reconcile). The other
sig-apps RC specs wait for running Pods (no kubelet on conform).
- [x] 0a41ade `replicaset.rs`: upstream's ReplicaFailure (FailedCreate /
      FailedDelete, set once, removed after a clean pass or at desired);
      unit; ea1a855 file clippy-clean. Build VM sc-build-be4b9fc611: c-m 121
- [x] golden 3e10ed08dcac on conform.g8.lo, 3 focused runs: RC "surface a
      failure condition", RC scale, GC "delete pods created by rc", quota
      "life of a replication controller" all passed ×3. #125 closed.
      Adoption/release of Pods (RS + RC) filed as #295

### EndpointSlice mirroring churn (#133 + #272, P1) — COMPLETE 2026-10-29
endpointslice-mirroring rig fails every run; probe on conform.g8.lo
(01a3affcd023/dfb448b7c686 goldens): mirrored slices created and deleted in
a cycle — 23 creates / 22 deletes / 0 updates in one rig run; idle, the
bootstrap default/kubernetes-<hash> ~1 pair per 7 s (#272's 15k idle
requests). The deleter is service.rs `mirror()` (GC logs none; nothing else
deletes slices); which of its two conditions fires is not logged.
- [x] 4eb27f8 `mirror()` logs each delete with its reason (not wanted /
      owner changed, both uids); apiserver's bootstrap default/kubernetes
      Endpoints labelled `endpointslice.kubernetes.io/skip-mirror: "true"`
      (upstream). Build VM sc-build-8c66c06bf1: workspace exit 0, apiserver
      348, 0 rustc warnings, changed files clippy-clean
- [x] golden b3e0f453f79d: default/kubernetes loop gone (0 deletes/70 s),
      user slices still churned and mirror() logged no delete — the deleter
      was `deleted()`: owner_keys routed the slice's Endpoints owner (same
      name, own UID) as a missing Service. 2460f71: `service_owned` (only
      Service owners route) + mirrored slices deleted only when the name is
      free; unit; build VM sc-build-d1c93da429 (c-m 120)
- [x] golden-rustkube-controller-manager-9ce3ba85d932 on conform.g8.lo:
      endpointslice-mirroring 13/13 ×3; deadlines clean (idle 0 requests in
      60 s); EndpointSliceMirroring conformance spec 3/3. #133, #272 closed.
      DELETE events without UID stay #274

### Boot while the datastore is down: lost manifests, 403s (#293, P1) — COMPLETE 2026-10-22
server1 12.49 reboot: fastetcd repairing its redb file (13:15:08); the
apiserver's `wait_for_datastore` (60 s) and `load_existing_crds` (60 s) give
up and their results are unused, so the one manifest pass ran against a store
that did not answer (0 created, 114 failed: `get` errors read as "absent"),
then RBAC's store fallback failed closed → 403 for everyone until fastetcd
answered. server1's console log was unreachable (502), so the timeline is
inferred from the code, not read from the apiserver's log.
- [x] 995a3b6 `wait_for_datastore`: no deadline (ERROR each minute); the
      startup probe allows 600 s and nothing can be served without the store
- [x] manifests: a `get` error is `Unavailable`, not "absent"; Unavailable
      and NotServed retried in the background in filename order (1→30 s;
      unserved given up after 10 min); summary WARN/ERROR with causes
- [x] RBAC: a deny reached with an unreadable store → 503 + Retry-After: 1
- [x] Units (manifests 2, RBAC 2; MemStore lists, fails writes when
      unavailable); README/CHANGELOG; rig `datastore-late.sh` (rigs, 537e195)
- [x] Build VMs sc-build-ec35189573 (apiserver 346) and sc-build-c56955129f
      at 537e195: workspace build + tests exit 0, 0 rustc warnings, changed
      files clippy-clean. Golden stage build 01a3affcd023 requested
- [x] datastore-late rig on conform.g8.lo (test machines blocked on
      stormcos#271), store 150 s late (aaedeab): golden-rustkube-apiserver-
      01a3affcd023 13/13; control dfb448b7c686 9 FAIL — `0 created … 4
      failed` at INFO, objects 404, 403 with the store down (server1's
      signature). 70 s did not reproduce: the old CRD load's own 60 s
      covered it. #293 closed

### Quantities stored as JSON numbers (#292, stormupdate#23) — COMPLETE 2026-10-21
cilium's `install-cni-binaries` `limits.cpu: 1` (YAML number) was stored and
returned as `1`; upstream's Quantity always serializes as a string, so
kube-rs/k8s-openapi clients (stormupdate's controller) failed to list pods.
- [x] 071e284 `apimachinery::quantity::{canonical_number,
      numbers_to_strings}`: a number at a quantity path of a built-in that
      carries quantities → upstream's canonical DecimalSI string (`1`,
      `500m`, `1k`); CRDs and CRs untouched. Called in create admission,
      `builtin_update`, and `ResourceStorage::{create,update}` (controllers,
      manifests, webhooks); boot backfill of stored objects (as #101)
- [x] Units (apimachinery 128, apiserver 342); rig `quantity-strings.sh`
      (rigs); README/test README/CHANGELOG. Build VM sc-build-35c0e0fd84:
      workspace exit 0, 0 rustc warnings, no clippy warning in changed files
- [x] Golden stage build dfb448b7c686 requested
- [x] quantity-strings rig on conform.g8.lo (test machines blocked on
      stormcos#271): control b52da76 goldens 8 of 9 FAIL (`{'cpu': 1}`);
      golden-rustkube-*-dfb448b7c686 (recorded at 5074b54 = 071e284 + docs)
      10/10 PASS. #292 closed; ships with the release carrying dfb448b7c686

### 12.39 apiserver boot +13 s (#286, P0) — COMPLETE 2026-10-22
Not rustkube code. 12.42 kubelet rings (pvetest1/2): on the late boot the
kubelet's first create of a control-plane static pod is refused at its root —
stormpump `VolumeRegister` ENXIO/EINVAL/ENOENT/EBADMSG, or stormblock 404 for
the root it just cloned — and the kubelet's `start_backoff` floor (10 s,
rustkube-node deadlines.rs:76) delays the retry, which works:
apiserver `started: wait=11419ms … total=11493ms` vs 253–570 ms otherwise.
stormblock#485 (closed) measured no stormblock change in the path.
- [x] e697313 `--client-ca-file` waited for like the CRL (hardening, not
      the cause); build VM sc-build-2c676e5883; golden 26a55cc30646
- [x] Filed stormpump#182 (refusals), stormblock#489 (404 on the new root),
      rustkube-node#287 (retry a refused create in ~1 s); evidence on #286;
      proposed after rustkube-node#287. server1's 385 s is #290
- [x] Master's review: the refused root is the kubelet's own start-up
      sweep deleting a static pod's fresh root (same `ctr-ct-N` as last
      boot) → rustkube-node#288. Both kubelet fixes on rustkube-node main:
      008304b (#287, refused create retried 0.5→8 s) and 08cb1a0 (#288,
      per-start root names; sweep skips this start's). Proposed
      `--after-release rustkube-node>=08cb1a0`
- [x] 12.43 composed with rustkube-node 81dc1bd (contains 08cb1a0); the
      item came back on the compose. Not measured: pvetest1's 12.43 update
      failed at preflight (02:23Z, host unreachable), pvetest2 has not taken
      it, both VMs unreachable 2026-10-21 evening. Parked `wait-owner
      --until` a 12.43+ install on both pve VMs
- [x] pvetest2 12.43-flowsdn (pvetest2-1792636341): reboot → apiserver
      38.5 → 20.7 s, boot 24.4 s (firmware/initramfs, not the apiserver);
      phases 6.1 / 4.0 s; three apiserver starts total 0.29–0.55 s, no
      refused root in the kubelet ring. Posted on #286
- [x] pvetest1 12.49 (pvetest1-1792669683): boot → apiserver 19.6 s
      (phase 3.9 s), reboot 19.0 s (phase 4.0 s); reboot's apiserver start
      attempts=1, total 308 ms, no CreateContainerError. pvetest2 12.48:
      4.6 / 3.0 s. Closed as a duplicate of rustkube-node#288

### Kubelet events 403 in a test namespace (#282, P1) — COMPLETE 2026-11-06
Not RBAC: the stormcos kubelet's cert is `O=system:nodes` → `cluster-admin`.
The namespace (stormcos_qa turbomode 543727dbc0) was deleted by the run's
cleanup before the Dell rebooted; the 403 is NamespaceLifecycle (Terminating,
or missing before 776a392). Upstream also 403s, but with the
`NamespaceTerminating` cause client-go's recorder drops quietly.
- [x] `namespace_lifecycle`: the cause on the 403; unit; rig
      `node-events.sh` (rigs); README/test README/CHANGELOG
- [x] Build VM sc-build-09c08f291e at d5f48f5: workspace build + tests exit 0
- [x] Kubelet half filed: rustkube-node#260 (log the cause at debug)
- [x] Golden requested 2026-10-21 (stage build 143a30c11fc3)
- [x] node-events rig: test machines still blocked (stormcos#271), so run
      on conform.g8.lo from golden-rustkube-apiserver-4b47213c228d (ebd156a):
      6/6 ×3; control efbea2d fails 2 (no NamespaceTerminating cause,
      missing namespace 403 not 404). #282 closed

### VM template changes reach the running VMI (#269, P1, stormvm#23/#24) — COMPLETE 2026-10-13
Kubelet half on rustkube-node main (#246 cpu/memory/NICs, #247 disks): it
diffs the VMI spec once per generation, reports hot-added disks in
`status.volumeStatus[].hotplugVolume`, refusals as `HotplugSynchronized`
False. This side: `virtualmachine.rs` PUTs the VMI's spec on a live change.
- [x] a69b5f7 `live_update` (+ `paired` for interfaces/networks and
      disks/volumes, `first_difference` for the rest). Live: cpu.sockets ≤ the VMI's maxSockets, memory.guest ≤ maxGuest,
      bridge-bound non-pod interface (+ network) added/removed, disk (+
      volume; virtio/scsi, no cloud-init seed) added, a hot-added disk
      removed. Everything else, no maxima, past the maxima, a boot disk
      removed, or the kubelet's refusal → VM condition `RestartRequired`
- [x] 8 units; b169719 vm-runstrategy.sh live cases; README/CHANGELOG
- [x] Build VM at b169719: workspace 641 passed / 4 ignored, 0 rustc
      warnings; clippy-clean virtualmachine.rs (impl moved, c-m 16 VM units)
- [x] rigs 7cac9d5177 (pvetest1, b169719): vm-runstrategy 26/26 (9 new);
      other 12 failures as before. 93bb6fc112 (pvetest2) image build error
      (stormcentral's dnf, libtinfo). C2NR0Q2/server3 refused (no release)
- [x] golden-rustkube-controller-manager-8227afb87508 (stormcos#422);
      #269 closed. Live node check after the release

### Dead node never marked (#267, P0, server8 12.14) — SHIPPED 2026-10-13
- [x] fb27723 fake-apiserver unit: the *existing* controller does mark and
      drain a node dead 9 days before it started (Ready False + not-ready),
      so server8's stall is not "never judged" — cause there unknown (no log
      access); a failed node write was retried but its error never logged
- [x] a57833b/4f912cb upstream semantics: conditions Unknown
      (lastHeartbeatTime kept), `unreachable` taints, not-ready only for
      Ready=False; no Lease → newer of creation and Ready heartbeat; node
      write errors propagated (logged); README/CHANGELOG. Build VMs: 633
      passed / 4 ignored, 0 warnings, node.rs clippy-clean
- [x] golden-rustkube-be6ddd9ee998 (stormcos#424)
- [ ] server8's CM log — the master's step (2026-10-13: not an owner
      decision). server8 :6443 refused, no syslog from it: parked with
      `--probe tcp://192.168.11.17:6443`. Master (2026-10-13): read on the
      first hardware install of a passing release. Marked `shipped`
      2026-10-13; a new cause the log names → new issue

### Rollout stalls on an unschedulable surge pod (#266, P2, stormcos#482) — COMPLETE 2026-10-28
- [x] `rollout::plan_rolling` old-RS scale-down as upstream's
      reconcileOldReplicaSets (maxScaledDown from spec totals; unhealthy
      first; healthy down to available - minAvailable); units incl. the
      issue's case; rig `rollout-unschedulable.sh`; docs (0f8114f, ebcc42b)
- [x] Build VMs: 631 passed / 4 ignored, 0 warnings;
      golden-rustkube-31a9a9242129 (stormcos#424); stormcos#482 told
- [x] rig on conform.g8.lo (test machines blocked, stormcentral#512 /
      stormcos#271), goldens 01a3affcd023/dfb448b7c686: 6/6 runs pass
      (21–22 s); control efbea2d fails 4 (old RS kept, two pods, stalled).
      1c81036901's failure was the rig: whether the surge pod is seen
      Unschedulable before the old one goes is timing → NOTE line (0f473fb,
      ddbf36f). No live rollout yet (pvetest1's updates fail in stormupdate's
      stage); a regression there → new issue. #266 closed

### Bootstrap tokens + forge-node CSR rules (#264, P0, stormcert#78) — COMPLETE 2026-10-28
- [x] `bootstrap_token.rs` (upstream's: Secret kube-system/bootstrap-token-<id>,
      type, usage, expiration, extra groups) in `SigningKeys::authenticate`
      (requests + TokenReview)
- [x] `csr_admission.rs`: spec.username/groups stamped on create, spec kept on
      update; /approval needs `approve`, /status certificate `sign` on the
      signer; bootstrappers: forge-node CSRs only, read own only (GET, watch by
      name). CM role signs its four signers. `system:storm:forge-node-signer`
      ClusterRole (reconciled, unbound: stormcos binds stormcert)
- [x] units + `test/e2e/bootstrap-token.sh`; README/certificates/test
      README/CHANGELOG. Build VMs at ba24d77/098943f: 629 passed / 4
      ignored, 0 warnings; golden-rustkube-4c916e869ff5 (stormcos#424);
      stormcert#78 told to bind its identity
- [x] bootstrap-token rig on conform.g8.lo (test machines still blocked on
      stormcentral#512 / stormcos#271), rig at 906e6f2: golden-rustkube-
      apiserver-01a3affcd023 32/32; control efbea2d goldens 19 FAIL (401 for
      every bootstrap token, forged spec.username stored, no signer role).
      #264 closed

### --client-crl-file (#260, P2, stormcert#61) — COMPLETE 2026-10-29
- [x] `--client-crl-file` (repeatable, PEM or DER) → WebPkiClientVerifier
      `with_crls`, end-entity only, unknown status allowed; re-read with the
      client CA (30 s), last good kept on a bad file; units; rig
      `client-crl.sh`; README/certificates.md/CHANGELOG (6a4e0bd)
- [x] Build VM at 6a4e0bd: 620 passed / 4 ignored (apiserver 318); 0 rustc
      warnings, changed files clippy-clean
- [x] bf1bb67 DER-completeness unit; golden-rustkube-5ac87c053591
      (stormcos#424); wiring notes on stormcos#481
- [x] client-crl 13/13 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a
      (C2NR0Q2), 51ef481a0b (pvetest2); conform.g8.lo from the 01a3affcd023
      goldens 13/13; efbea2d's apiserver refuses the flag (no revocation
      before). #260 closed

### Docs refresh from the code (since 2026-10-02) — COMPLETE 2026-10-09
- [x] README (date, How it ships at stormcos 73091eac, rig suites, build
      volume, stale claims), every docs/ page but the research and legacy
      Terragrunt ones, test/README, CLAUDE.md, audit addendum, CHANGELOG
- [x] Filed #257 (stormblock PV reclaim from the claim), #258 (HPA not via
      `/scale`); rig day budget noted on #173. presentation.md not re-rendered

### Table-only Accept on reviews → 406 (#126, P3) — COMPLETE 2026-10-29
- [x] `table::{only_table, refuse_table}` on the five review routes; unit;
      `test/e2e/table-review.sh` (rigs); README/conformance/CHANGELOG
- [x] Build VM at fb08f87 exit 0 (apiserver 288);
      golden-rustkube-773e0d8ddf86 (stormcos#366)
- [x] "Servers with support for Table transformation should return a 406 …"
      conformance spec passed in the d5f48f5 run from goldens (failed at
      efbea2d); table-review 7/7 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b. #126 closed

### LimitRanger admission (#131, P2) — COMPLETE 2026-10-29
- [x] `limitranger.rs`: Container defaults + annotation, Container/Pod
      min/max/ratio, PVC storage bounds, 403 upstream wording; requests from
      limits before QoS; units; `test/e2e/limitrange.sh` (rigs); docs
- [x] Workspace at cc19063 on a build VM exit 0 (apiserver 287, 3 new);
      golden-rustkube-79c26ab4b5e5 (stormcos#366)
- [x] [sig-scheduling] "LimitRange … pod has those defaults applied"
      conformance spec passed in the d5f48f5 run from goldens (failed at
      efbea2d); limitrange 9/9 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b. #131 closed

### stormblock PV volumeMode from the claim (#201, P2) — COMPLETE 2026-10-29
- [x] `stormblock::desired_pv` (PV construction out of `provision_claim`)
      sets `volumeMode` from the claim (default Filesystem); unit;
      stormblock-class.sh Block case; storage.md, CHANGELOG
- [x] Build VM sc-build-470bf3e632 at 7c859df: 612 passed / 4 ignored;
      golden-rustkube-1589ac3cd829 (stormcos#366)
- [x] stormblock-class 5/5 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b and on conform.g8.lo (controller-manager 3e10ed08dcac);
      control efbea2d fails the Block cases. #201 closed (with #92)

### Slow pod status PUT breakdown (#191, P2) — COMPLETE 2026-10-29
- [x] `WritePhases` in `guaranteed_update`: read/mutate/webhooks/write/
      retry_wait histogram; `slow write` warn > 100 ms with the key;
      `slow request` warn in metrics_middleware (long-running left out);
      unit; `test/e2e/status-put-latency.sh` (rigs); docs/metrics.md, CHANGELOG
- [x] Build VM at fc74517: 611 passed / 4 ignored (#239, my unit's path,
      closed); golden-rustkube-268f9826b9cd (stormcos#366)
- [x] The breakdown names the phase in production: pvetest2 12.19 (163
      `slow write`, read-heavy), server1 12.55 (write_ms 8538 of 8552) — the
      datastore (fastetcd#135). Rig fixed (ee524d1: colour codes broke its
      webhooks_ms parse; p99 bound was the disk → NOTE + apiserver's own
      share < 50 ms): 5/5 ×3 on conform.g8.lo, own share ≤ 0.5 ms. #191 closed

### Secret stringData folded into data (#101, P2) — COMPLETE 2026-10-14
- [x] `builtin_admission::fold_string_data` on create, PUT, PATCH (before
      the immutability check, again after webhooks), startup manifests; boot
      backfill for stored Secrets; unit; README/CHANGELOG
- [x] b677499 on a build VM (`SC_BUILD_VM=1 sc-build`, job 414c3ccedc):
      workspace 534 passed / 4 ignored; `test/e2e/secret-stringdata.sh`
- [x] secret-stringdata 5/5 in rigs 756f2f6c20 (pvetest1), 1a879717b9,
      51ef481a0b (pvetest2); in every golden since 10-07; #101 closed

### autoscaling/v1 served (#123, P3) — COMPLETE 2026-10-29
- [x] `handlers/hpa_v1.rs` (to_v1/to_v2 with upstream's annotations; v1
      routes delegating to the v2 handlers; watch converted per line; PATCH
      on the v1 view); discovery (v2 preferred + v1), apply table; autoscaling/v1
      proto vendored (Scale's schema too); units; `test/e2e/hpa-v1.sh`; docs
- [x] Build VM cabae5a433 at 7ef6fac: 577 passed / 4 ignored (#235: my
      unit caught a null-annotations insert, fixed 7ef6fac);
      golden-rustkube-10934b6134a7 (stormcos#366)
- [x] Discovery "should locate the groupVersion and a resource within each
      APIGroup" conformance spec passed in the d5f48f5 run (failed at
      efbea2d). hpa-v1's test-machine failures (TypeError) were #277's raw
      chunked WATCH reads; 92f399a fixes all five rigs; on conform.g8.lo from
      the 01a3affcd023 goldens hpa-v1 10/10, watch-timeout 6, runtime-class
      18, flowcontrol 29, gc-orphan-load 5. #123, #277 closed

### Admission policies served (#119, P3) — COMPLETE 2026-10-29
Split: (1) serve the four resources here; (2) CEL evaluation is #234 (the
`cel` crate 0.15 handles struct literals via registered StructTypes; needs
Cargo.lock via a build job; shares the engine with #217).
- [x] routes (+ cluster /status), discovery, apply table, kinds; Warning on
      create that policies are not enforced; `test/e2e/admission-policy-api.sh`
      (rigs); README/inventory/conformance/CHANGELOG; #234 filed
- [x] Build VM 2bd668d442 at 08d5974: 575 passed / 4 ignored;
      golden-rustkube-883fe8afc72d (stormcos#366)
- [x] The four "…AdmissionPolicy(Binding) API operations" conformance specs
      passed in the d5f48f5 run from goldens (failed at efbea2d); the four
      evaluation specs still fail (and hang ~40 min each): #234.
      admission-policy-api 30/30 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b. #119 closed

### YAML bodies + built-in fieldValidation (#122, P2) — COMPLETE 2026-10-29
- [x] protos re-vendored to release-1.36 (all additive, checked per field);
      `protobuf::unknown_fields`; `field_validation.rs` middleware (POST/PUT,
      Strict/Warn/Ignore, upstream wording); protobuf_mw: YAML→JSON, missing
      Content-Type = JSON, Strict YAML duplicates; units;
      `test/e2e/field-validation.sh` (rigs); README/conformance/CHANGELOG
- [x] Build VM 84a18836ef at a3bff57: 575 passed / 4 ignored;
      golden-rustkube-96f8267cda52 (stormcos#366)
- [x] All 7 [sig-api-machinery] FieldValidation conformance specs passed in
      the d5f48f5 run from goldens (5 failed at efbea2d, incl. the issue's
      two typed-object specs); field-validation rig 9/9 in rigs fe841f11a1,
      3c6585e0ea, 7cb36fe72a, 51ef481a0b. #122 closed. Not done: dropping
      unknown built-in fields under Warn/Ignore; PATCH bodies

### ServiceCIDR / IPAddress served (#134, P3) — COMPLETE 2026-10-29
- [x] networking/v1 proto re-vendored (release-1.36, additions only);
      routes (+ cluster /status), discovery, resources_for (networking was
      missing), kinds; bootstrap `kubernetes` ServiceCIDR Ready; protobuf unit;
      `test/e2e/service-cidr.sh` (rigs); docs/CHANGELOG
- [x] Build VM 9c265a5785 at aafcbdf: 572 passed / 4 ignored;
      golden-rustkube-44fc40d7ae0d (stormcos#366)
- [x] Both [sig-network] ServiceCIDR/IPAddress conformance specs passed in
      the d5f48f5 run from goldens (conform.g8.lo, 10-21; failed at efbea2d);
      service-cidr rig 12/12 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b. #134 closed. Not done (optional in the issue): IPAddress
      per ClusterIP, allocation from extra ServiceCIDRs

### VMI migrate addedNodeSelector (#208, P2) — COMPLETE 2026-10-29
- [x] cf83dc4 apiserver: MigrateOptions.addedNodeSelector → migration spec
      (non-string 422); scheduler: reads the migration's selector, adds it to
      the VMI's (VMI's keys win); units; vmi-migration.sh cases; docs
- [x] Build VM 104f75011a at 1ffcc34: 571 passed / 4 ignored;
      golden-rustkube-c4cbcef1dd7d (stormcos#366)
- [x] vmi-migration 36/36 (4 addedNodeSelector checks) in rigs fe841f11a1,
      3c6585e0ea, 7cb36fe72a (C2NR0Q2), 51ef481a0b (pvetest2); conform.g8.lo
      from the dfb448b7c686 goldens 36/36. No pre-cf83dc4 binaries to control
      with (goldens of then carry none, stormcos#62). #208 closed

### TokenRequest default back to 1 h (#206, P3) — SHIPPED 2026-10-07
- [x] rustkube-node#122 (d25f05c, 0cdc142, e69287f) is in a318daf, the
      rustkube-node of published stormcos 11.88 (and every later compose)
- [x] 49ee0f0 `DEFAULT_TTL_SECS` 3600; unit; README/inventory/CHANGELOG;
      build VM 9d8d14de2d 571 passed / 4 ignored; golden-rustkube-5e640df87001
      (stormcos#366, release note posted there)

### KubeVirt controllers gated on their CRDs (#172, P2) — COMPLETE 2026-10-29
- [x] `runner.rs::kubevirt_gate`: VM / VMI launcher / VMI migration
      controllers start when their CRDs are Established (shared CRD feed),
      stop on delete, abort with the term. deadlines.sh: strict 0 idle +
      no KubeVirt requests. README/design/CHANGELOG
- [x] Built + units on a build VM at 6235778 (job 9ffe19b531): 571 passed /
      4 ignored; golden-rustkube-4f7661e44994 (stormcos#366)
- [x] KubeVirt rigs (vm-runstrategy, vmi-migration, vmi-launcher, vmi-verbs)
      pass in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a (C2NR0Q2), 51ef481a0b
      (pvetest2): the gated controllers start with their CRDs. deadlines on
      conform.g8.lo, dfb448b7c686 goldens: 0 KubeVirt requests without the
      CRDs; efbea2d's controller-manager: 2,575 (404 VM LISTs). Its idle-0
      check fails on both: #272's EndpointSlice loop. #172 closed

### ServiceAccount key rotation (#223, P2) — SHIPPED (golden-rustkube-4f7661e44994) 2026-10-07
- [x] `SigningKeys::from_pem`: signing key RSA or P-256; verify against
      every public key of every `--service-account-key-file` (repeatable,
      comma-separated, multi-PEM); unit (old key verifies after rotation);
      README/certificates.md (rotation steps)/CHANGELOG
- [x] Built + units on a build VM at 90208b0 (job 50292519d5): workspace
      570 passed / 4 ignored (apiserver 280, incl. the rotation unit), exit 0
- [x] golden-rustkube-4f7661e44994 (stormcos#366); marked shipped

### Scale subresource (#86, P2) — COMPLETE 2026-10-29
- [x] `handlers/scale.rs`: autoscaling/v1 Scale GET/PUT/PATCH for
      deployments/replicasets/statefulsets and CRDs with
      `subresources.scale` (`CrdDefinition.scale`); discovery entries (apps +
      CR `/scale`, `/status`); protobuf path → Scale. Units;
      `test/e2e/scale.sh` (rigs, real kubectl); docs/CHANGELOG
- [x] Built + units at 90208b0 (job 50292519d5, 570 passed / 4 ignored)
- [x] scale (real kubectl) 14/14 in rigs fe841f11a1, 3c6585e0ea,
      7cb36fe72a (C2NR0Q2), 51ef481a0b (pvetest2); conform.g8.lo from the
      01a3affcd023 goldens 14/14; control efbea2d fails 12 (`kubectl scale`
      NotFound, the issue's symptom). #86 closed

### Scheduling gates (#87, P2) — COMPLETE 2026-10-29
- [x] ddcc821 scheduler: gated Pod → PodScheduled=False/SchedulingGated (no
      Event), placed when the last gate goes; apiserver PUT/PATCH adding a
      gate → 422 (upstream's message). Units; `test/e2e/scheduling-gates.sh`
      (rigs); README/inventory/research/presentation/test README/CHANGELOG
- [x] Built + units at 90208b0 (job 50292519d5, 570 passed / 4 ignored)
- [x] scheduling-gates 7/7 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a
      (C2NR0Q2), 51ef481a0b (pvetest2); conform.g8.lo from the dfb448b7c686
      goldens 7/7; control efbea2d fails 4 (gated Pod bound, gate add 200).
      In every golden since 10-07; #87 closed

### HPA (#89, P2) — COMPLETE 2026-10-29
Owner (2026-10-07, twice): the real HPA, CPU/memory from **cadvisor**;
serve `metrics.k8s.io` (pods/nodes, `kubectl top`); upstream's ratio,
tolerance, scale-down stabilisation. A (inert) shipped meanwhile.
- [x] A: `hpa.rs` inert (5415161), compiled + units at 566403b
- [x] apiserver `resource_metrics.rs`: `metrics.k8s.io/v1beta1` NodeMetrics
      / PodMetrics from each node's cadvisor (`/api/v1.3/subcontainers/`,
      InternalIP, `--cadvisor-scheme/-port/-ca-file/-token-file`), root cgroup
      for nodes, `io.kubernetes.*` labels for pods, 10 s cache; discovery
      (built-in group); view reads pod metrics
- [x] HPA: upstream calculator, behavior/stabilisation/rate limits, min/max,
      conditions, currentMetrics, 15 s resync, Pod feed; no metrics → no change
- [x] Units; `test/e2e/hpa-metrics.sh` (rigs, stub cadvisor); README,
      inventory, oc-compat, presentation, design, test README, CHANGELOG
- [x] cadvisor#3 told the labels rustkube reads
- [x] Built + units at 90208b0 (job 50292519d5): controller-manager 94
      (HPA calculator/behavior units), apiserver 280 (cadvisor summary)
- [x] hpa-metrics rig: the 10-13 C2NR0Q2 failures (NodeMetrics 404, no
      PodMetrics) were the rig's stub cadvisor — negative CPU counters in its
      first 10 s (fixed 80b7184). On conform.g8.lo from the 01a3affcd023 /
      dfb448b7c686 goldens: 3/3 runs 10/10. Control, efbea2d's placeholder
      controller-manager with the same apiserver: the 7 HPA checks fail.
      #89 closed. On stormcos pod metrics wait for cadvisor#3 (open)

### Rejected bearer token → 401, not anonymous (#115, P2) — COMPLETE 2026-10-14
- [x] `auth_middleware`: a presented Bearer token nothing accepts → 401
      Status (reason Unauthorized) whatever `--anonymous-auth`; anonymous only
      without credentials (no header, empty `Bearer `, other scheme); scheme
      any case (upstream's bearertoken.go). Units; `test/e2e/bad-token.sh`
      (rigs, `RK_ANONYMOUS_AUTH=true`); README/test README/CHANGELOG
- [x] 7bebbaf on a build VM (sc-build-e80aa11f7e): workspace 548 passed /
      4 ignored (apiserver 271), exit 0. #229 (my test closure) closed
- [x] bad-token 9/9 in rigs 756f2f6c20 (pvetest1), 1a879717b9, 51ef481a0b
      (pvetest2); in every golden since 10-07; #115 closed

### Metrics (#90, P2) — COMPLETE 2026-10-29
- [x] Reconcile metrics recorded per object pass (`owned::run`)
- [x] Buckets (upstream's for apiserver/etcd request durations and scheduler
      e2e; DefBuckets else); `render()` types process_cpu_seconds_total counter
- [x] apiserver `/metrics` inside authn + RBAC (non-resource get);
      `system:monitoring` role + binding bootstrapped
- [x] CM/scheduler: `--tls-cert-file`/`--tls-private-key-file` (HTTPS,
      reloaded), delegated TokenReview + SAR on `/metrics` (10 s cache),
      `--authorization-always-allow-paths`; roles gain the two reviews
- [x] Standby leader gauge 0 from the start; `apiserver_storage_objects`
      from resource-wide prefixes only
- [x] Units; `test/e2e/metrics-auth.sh` (rigs); rigs scrape with the admin
      token; docs/metrics.md, README, test README, CHANGELOG (BREAKING)
- [x] stormcos#384 filed (ironprom token/https for 10257/10259, certs,
      probe scheme) — must land with the golden carrying this
- [x] Compiled + units on a build VM at f7bdbe7 (job 19f5cef027): workspace
      557 passed / 4 ignored (apimachinery 117, apiserver 272), exit 0
- [x] metrics-auth 23/23 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a
      (C2NR0Q2), 51ef481a0b (pvetest2); on conform.g8.lo from the
      01a3affcd023/dfb448b7c686 goldens 23/23; control efbea2d 3/23 (20
      FAIL: /metrics open, summaries, no buckets, :10257/:10259 plain/no
      auth, no reconcile metric). #90 closed; stormcos#384 is stormcos's

### Service create cost (#113, P2) — COMPLETE 2026-10-14
Cause (from the code): `service_ip::allocate` walked the range from offset
1, one store create-if-absent per address, so N Services = N+1 writes per
create, and concurrent creates raced for the same lowest address.
- [x] Candidate = random free address from the watch cache's view of the
      claim keys, upper band first (upstream's static band min(max(16,
      size/16), 256) last); lost race → next. Claim keys unchanged. Unit;
      `test/e2e/service-create.sh` (rigs); README/test README/CHANGELOG
- [x] fd72fbc broke main (E0373, #230; #231 auto-filed); 566403b fixed it.
      Workspace on a build VM at 566403b (job 3cc07c33b9): 553 passed / 4
      ignored (apiserver 271, controller-manager 89), exit 0
- [x] service-create 8/8 in rigs 756f2f6c20 (pvetest1, 4.3 s whole rig),
      1a879717b9 and 51ef481a0b (pvetest2, 3.4–3.5 s); timings now NOTE
      lines; in every golden since 10-07; #113 closed

### Gateway controller: own classes only (#91, P2) — SHIPPED (golden-rustkube-4f7661e44994) 2026-10-07
- [x] a4670a6 `gateway.rs`: acts only on GatewayClasses with controllerName
      `rustkube.io/gateway-controller`, their Gateways, and its own HTTPRoute
      `status.parents` entries (others kept; a route of foreign Gateways not
      written); no `status.addresses`; Gateway + listeners `Programmed=False`
      (Pending, no data plane). 3 units; README/inventory/presentation/CHANGELOG
- [x] Compiled + units at 566403b (3cc07c33b9): controller-manager 89,
      workspace 553 passed / 4 ignored
- [x] golden-rustkube-4f7661e44994 (stormcos#366); marked shipped. Whether
      the controller should exist stays #70

### API aggregation wired (#83, P2) — COMPLETE 2026-11-06
Done when a registered, Available APIService's group is in discovery and its
requests are proxied (metrics-server or a stub). Consumers: metrics.k8s.io
(#89's real HPA, `kubectl top`), stormblock's storage.storm.io (comment).
- [x] 4c294b3 `aggregation.rs` rewrite: APIServices followed via the watch
      cache (1 s); proxy middleware inside authn + RBAC (ClusterIP, TLS for
      `<svc>.<ns>.svc` / caBundle, `--proxy-client-cert/key-file` reloaded,
      X-Remote-User/Group, client creds dropped, responses streamed,
      unavailable 503, upgrades 501, built-in groups never); protobuf mw
      bypassed for claimed paths; availability every 5 s (Local /
      ServiceNotFound / Passed / FailedDiscoveryCheck) written on change;
      `/apis`, `/apis/{g}` list aggregated groups
- [x] `extension-apiserver-authentication` ConfigMap (client CA,
      `--requestheader-client-ca-file`, allowed names, header names; 30 s
      re-read) + `system:auth-delegator`, reader Role
- [x] Units (aggregation 5, contract 1); `test/e2e/aggregation.sh` (rigs);
      README/inventory/presentation/oc-compat/test README/CHANGELOG
- [x] Compiled + units at 566403b (3cc07c33b9; after four cancelled
      jobs, stormcentral#535): apiserver 271, workspace 553 / 4 ignored
- [x] aggregation rig 17/21 every run: discovery/LIST 503 right after
      Available (the table followed status 1 s behind; the identity check
      then read the availability probe), watch read chunk sizes (rig). Fix:
      `set_available` from the check before the status write; rig r.readline
- [x] stormcos#405 (front-proxy pair + requestheader CA) filed 10-08
- [x] Build VM at fc180f3: tests pass (apiserver 334), aggregation.rs clippy-clean
- [x] rigs: 3dd6f3f469, b29fc80675 errored (pvetest2 down; pvetest1 no
      release) → stormcentral#846 (closed)
- [x] rig: effaf28113 (pvetest2, c90df0c, 2026-10-21) errored (stormcos#271);
      test machines still can't start test pods (stormcentral#512), so on
      conform.g8.lo from the ebd156a goldens (apiserver 4b47213c228d, has
      fc180f3): aggregation 21/21 ×3; control efbea2d refuses
      `--proxy-client-cert-file` (no aggregation). #83 closed

### Store compaction / continue-token expiry (#139, P2) — COMPLETE 2026-10-14
- [x] fastetcd: auto-compaction off by default (`--auto-compaction-retention
      0`, revisions); space reclaim above 80% keeps 1000 revisions anyway;
      stormcos passes none of these flags — no routine compaction today, and
      an apiserver loop does not conflict (treat already-compacted as
      success). Posted on #139
- [x] `compactor.rs`: upstream's round (`compact_rev_key` version CAS via
      `KvStore::compact_claim`, compact to the last win's revision),
      `--etcd-compaction-interval` (5m0s, 0 off), gauge
      `apiserver_storage_compacted_revision`; `list_page`: compacted token →
      410 `Expired` + `metadata.continue` (bare key = current snapshot);
      `ApiError::gone` reason `Expired`. Units; `test/e2e/compaction.sh`
      (rigs, with #127's watch check); README/test README/CHANGELOG
- [x] 73589ce on a build VM (sc-build-dc10dbc8b3): workspace 547 passed /
      4 ignored (apiserver 270), exit 0
- [x] compaction rig failed every run (500 for a compacted token, WATCH
      hang): rigs' fastetcd v1.12.0 words its compacted error `required
      revision N has been compacted`, and returns a compacted watch's cancel
      in the create response etcd-client drops. Adapter: `is_compacted`
      (both wordings), a read at a watch's start revision after create
- [x] compaction 10/10 in rigs 51ef481a0b (pvetest2, 15a4de7; 11 failures
      left, as before); #139 closed. Golden with the next component build
      (after stormcentral#827)

### nodes/{name}/proxy (#108, P3) — COMPLETE 2026-10-29
- [x] `handlers/node_proxy.rs` (every method → kubelet :10250, apiserver
      token, status/content type/body streamed), RBAC path arm
      (`nodes/proxy`), discovery; units; `test/e2e/node-proxy.sh` (stub
      kubelet); docs. Kubelet `/logs/` filed rustkube-node#198
- [x] Build VM at 76f125d: 607 passed / 4 ignored (apiserver 309);
      golden-rustkube-ac7748374265 (stormcos#366)
- [x] node-proxy 8/8 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b and on conform.g8.lo (apiserver 4fe0997b273e, rig 71e6a10);
      control efbea2d fails 7 ("admin cannot be authorized … unrecognized").
      #108 closed; node-logs' content needs rustkube-node#198 (open)

### CSR controller and external signers (#199, P2) — COMPLETE 2026-10-29
- [x] `--csr-external-signer-names` → `CsrController::with_external_signers`;
      sign only upstream's kubernetes.io/* signers; unit;
      `test/e2e/csr-signers.sh`; README/certificates.md
- [x] Build VM at fb741a6: 610 passed / 4 ignored (controller-manager 104);
      golden-rustkube-504ec94ee5c9 (stormcos#366)
- [x] csr-signers 6/6 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b and on conform.g8.lo (controller-manager 3e10ed08dcac);
      control: efbea2d's controller-manager signs the kubelet-serving CSR an
      external signer approved. #199 closed. stormcos passes the flag when
      stormcert's csr-signer is on (stormcert's session files it)

### Remove the GitHub workflow (#114) — COMPLETE 2026-10-08
- [x] `.github/workflows/images.yml` removed (458f598); Actions confirmed
      disabled (`actions/permissions` → enabled false); releasing.md, audit
      table, plan updated; workspace on a build VM 606 passed / 4 ignored

### SelfSubjectReview (#116, P3) — COMPLETE 2026-10-29
- [x] handler + route + discovery; system:basic-user gains the rule and is
      reconciled at boot; unit; `test/e2e/whoami.sh` (kubectl auth whoami)
- [x] Build VM at 5c0e041: 606 passed / 4 ignored (apiserver 308);
      golden-rustkube-d85a5ba35d81 (stormcos#366)
- [x] whoami 4/4 (kubectl auth whoami) in rigs fe841f11a1, 3c6585e0ea,
      7cb36fe72a, 51ef481a0b and on conform.g8.lo (apiserver 4fe0997b273e);
      control efbea2d fails 4 ("selfsubjectreviews API is not enabled").
      #116 closed

### Ephemeral-volume controller (#94, P2) — COMPLETE 2026-10-29
- [x] `ephemeral.rs` (pods primary, owned PVC children; create from the
      template; foreign claim → Warning Event, never adopted); CM role
      creates PVCs; unit; `test/e2e/ephemeral-volume.sh`; README/storage.md
- [x] Build VM at 40157d0: 605 passed / 4 ignored (controller-manager 102);
      golden-rustkube-41c5a5a6ec37 (stormcos#366)
- [x] ephemeral-volume 6/6 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b and on conform.g8.lo (controller-manager 3e10ed08dcac);
      control: efbea2d's controller-manager makes no claim. #94 closed

### Aggregated discovery (#107, P3) — COMPLETE 2026-10-29
- [x] `wants_aggregated`, `to_v2_resources`, `aggregated` on `/api` + `/apis`
      from `builtin_lists` (now also events/metrics) + CRD registry; aggregated
      APIs Stale; units; oc-adm.sh `inspect` → works + curl checks; docs
- [x] Build VM at 5215b7f: 604 passed / 4 ignored (apiserver 307);
      golden-rustkube-b15d23ed5f76 (stormcos#366)
- [x] The four AggregatedDiscovery conformance specs passed in the d5f48f5
      run from goldens (failed at efbea2d); oc-adm inspect + the two
      aggregated-discovery checks pass on conform.g8.lo at 12a63ba (57/57).
      #107 closed

### authorization.openshift.io reviews (#106, P3) — COMPLETE 2026-10-29
- [x] `RbacEngine::who_can`; `handlers/openshift_authorization.rs` (SAR,
      LocalSAR, RAR, LocalRAR; legacy-group mapping); routes, discovery,
      advertised table; unit; oc-adm.sh verbs → works; docs
- [x] Build VM at 046d2c3: 602 passed / 4 ignored (apiserver 305);
      golden-rustkube-f4592f74ec16 (stormcos#366)
- [x] oc-adm: #106's checks (who-can, new-project --admin) passed in every
      test-machine run; its 2 failures were the rig's expectations (top pod
      works since #89; inspect of `work` hit a node-less Pod). 12a63ba; 57/57
      ×2 on conform.g8.lo (apiserver 4fe0997b273e). #106 closed

### Scheduler preemption (#84, P2) — COMPLETE 2026-10-29
- [x] `preemption.rs` rewritten (on_node/select/waiting_for_victims over
      ClusterState + filters, PDB-aware reprieve); `Scheduler::preempt`
      (nominate, Eviction API, Preempted Event); nominated room held in
      snapshot; bind clears nominatedNodeName; scheduler role pods/eviction;
      units; `test/e2e/scheduler-preemption.sh`; docs
- [x] Build VM at 72fc783: 601 passed / 4 ignored (scheduler 64: the old
      module's 10 tests replaced by 3 on the wired path);
      golden-rustkube-7df7dc5a5562 (stormcos#366)
- [x] scheduler-preemption 8/8 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b and on conform.g8.lo (apiserver 4fe0997b273e, scheduler
      dfb448b7c686); control efbea2d fails 5. #84 closed. The conformance
      preemption specs use an extended resource the fit filter ignores: #297;
      three of them also need running Pods

### Unserved resources → 404 (#110, P3) — COMPLETE 2026-10-29
- [x] `served.rs` middleware from `discovery::advertised` (the discovery
      handlers' own lists); units; `test/e2e/unserved-resource.sh`; docs
- [x] Build VM at 999f2c0: 608 passed / 4 ignored (apiserver 304);
      golden-rustkube-dd10785be2ea (stormcos#366)
- [x] unserved-resource 22/22 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b and on conform.g8.lo (apiserver golden 4fe0997b273e);
      control efbea2d fails 16 (200 `<plural>List`, writes stored). #110 closed

### Kubernetes 1.36 API (#285, P2) — BUILT + RELEASED (12.58), pve suites PASS 2026-11-19, the Dell BLOCKED on stormuefi#76
The issue's crate pair does not apply: nothing imports k8s-openapi (declared
0.24 `v1_32` in apimachinery/apiserver/cloud, never `use`d — the only "1.32"
here) and kube-rs is not used; the wire API is the release-1.36 protos +
JSON. Served groups already match 1.36: none of the "stop serving" list is
served; MAP v1 (#119, CEL #234), DRA v1 (#137), ServiceCIDR (#134),
`/resize` (#136) are. Behaviour gaps found, in order:
- [x] Drop the unused k8s-openapi dependency (and serde-value, ordered-float
      2.10.1, which only it used)
- [x] `metadata.generation` for built-ins (none before: controllers reported
      observedGeneration 1 against no generation): `generation.rs`, 1 on
      create, +1 on a main-resource spec change, upstream's per-kind rules,
      Pods (1.35) without nodeName (the scheduler binds by PUT), deletion
      start; wired before and after webhooks on every write path; units;
      rig builtin-generation.sh. Kubelet's Pod observedGeneration: rustkube-node
- [x] Sidecars (init, restartPolicy Always) as upstream's PodRequests
      (1.33 GA): `apimachinery::pod_resources` for scheduler fit, quota
      usage and LimitRanger's Pod aggregate (which ignored init containers);
      units
- [x] Job `spec.managedBy` (1.35 GA): controller skips other managers';
      validated on create, immutable; units; rig job-managed-by.sh
- [x] Ordered namespace deletion (1.34 GA): Pods first, the rest after;
      rig namespace-order.sh (no unit: the controller needs live feeds)
- [x] resourceVersions decimal everywhere (KEP-5504, 1.35): already — store
      revisions as decimal strings (storage.rs, watch events/bookmarks, lists);
      a non-numeric one sent in is a 400. No change
- [x] Filed: rustkube-node#286 (Pod status.observedGeneration), #287
      (--authentication-config), #288 (external SA token signing), #289
      (externalIPs warning, if upstream sends one). MAP enforcement is #234.
      AuthorizeWithSelectors: RBAC ignores selectors upstream too — not filed
- [x] Build VMs per step (f0bb67a745, abd853be45, eaf6080208, 51306b0ea3,
      564fc55498): tests pass, 0 rustc warnings, changed files clippy-clean;
      golden requested 2026-10-21
- [x] First rig runs on conform.g8.lo (main 1958910 goldens, 2026-10-21):
      job-managed-by 10/10, namespace-order 8/8; builtin-generation 4 failed —
      the Deployment controller's revision annotation PUT bumped generation
      (now on its /status write; /status keeps generation), rig had no Node and
      a wrong RS expectation. b52da76 golden: builtin-generation 14/14
- [x] Published 12.58/12.58-flowsdn carry it: apiserver 01a3affcd023
      (537e195), CM/scheduler dfb448b7c686 (5074b54). Rigs on the ebd156a
      goldens (2026-11-06): builtin-generation 14/14, job-managed-by 10/10,
      namespace-order 8/8
- [x] pve (2026-11-19, stormcentral#512 closed): short 5/5 on pvetest2
      (af0d4ad73e, d1113e3) and pvetest1 (e13daa5548, 407da31); rigs 74/74 on
      pvetest2 (10e311c1a3, 407da31) incl. the three 1.36 rigs, after 407da31
      took /dev/fd out of the rigs (lib.sh FIFO tee; status-put-latency had
      failed on both VMs for it alone)
- [ ] The Dell: C2NR0Q2 refuses runs, "its last install (12.81) ended
      cancelled: it runs no known release" — stormcentral#1407 (its local
      disk never boots a pallet). Proposed after it; then short + rigs there,
      and close. 2026-11-19 later: #1407 closed with a check only (80fa627:
      an install that reboots into the network clone now fails); C2NR0Q2's
      pallet itself fails "no kernel inside the kernel member" — stormuefi#76,
      open. Same refusal today. Re-proposed after stormuefi#76

### Cold container start 1.49 s on 12.32-flowsdn (#284, P2) — CLOSED (not reproduced) 2026-10-21
The issue: confirm on the next release before tracing. `testhost installs`
on pvetest2: container_start 1.5 s on 12.32 only; 12.33–12.40 (8 installs)
0.3–0.5 s with the kubelet's total 0.2–0.5 s (~0.1 s outside it, as on
12.30/12.31); pvetest1 0.2–0.6 s throughout. One sample, not traced; closed

### Conformance binaries from goldens (#140, P2) — COMPLETE 2026-10-21
stormcentral#557 is live: `stormcentral component fetch rustkube --commit
<sha>` (the three rustkube-* stage goldens, sha256-checked) and `… fastetcd`
(newest golden, v1.34.0+2). Checked here at d5f48f5; a commit with no golden
exits 1. conform.g8.lo has no stormcentral CLI or token, so the fetch runs
where the session's token is and is pushed to the VM:
- [x] c8c7b83 `test/conformance/fetch.sh <sha>`: fetch + MANIFEST (goldens,
      sha256s) → rsync to conform@conform.g8.lo:/srv/conformance/<sha>; vm.sh
      reads that (no dev rsync); stage.sh deleted; 7b711d1 vm.sh runs the
      commit's scripts from a worktree (~/wt/<sha>); docs/CHANGELOG. Build VM
      sc-build-0049c402d6 exit 0; 3b126d9 two duplicated #[test] (warnings)
- [x] Six chunks on conform.g8.lo at d5f48f5 from goldens: 123 passed /
      306 failed of 429 (efbea2d: 75); #159's orphan-RS spec passed
      concurrently. docs/conformance.md table. #140 closed. Triage of the
      new run against the failure table not done
Owner: "Goldens". Goldens are sealed stormblock volumes on forge; nothing
reads a golden's files from another machine (`registry/<c>:<tag>` is a
display name). Filed stormcentral#557 (`component fetch <c> --commit --out`,
sha256-checked); fastetcd has no golden and no release assets (noted there).

### GC orphan failure under concurrent chunks (#159, P3) — COMPLETE 2026-10-21
- [x] efbea2d's GC (sweep, non-snapshot `live`, unconditional delete, 30 s
      interval) has both a latency and an unsafe-deletion path for the spec;
      d69e618 (#146) removed both. Remaining gap fixed: a failed owner strip
      now fails the pass (retry with backoff). `test/e2e/gc-orphan-load.sh`
      (20 concurrent runs under churn, per-run uid/RV/owners/timings);
      docs/conformance.md
- [x] Build VM at 9bff762: 606 passed / 4 ignored; golden-rustkube-0036569fb42a
- [x] Concurrent conformance rerun at d5f48f5 from goldens (#140,
      2026-10-21): the orphan-RS spec passed with six chunks at once
- [x] gc-orphan-load on conform.g8.lo (not a build slot): d5f48f5 13 runs /
      260 orphans, main 1958910 8 runs / 160: 0 unsafe, slowest 4.4 s;
      control efbea2d slowest 34.4 s (the 30 s sweep). #159 closed 2026-10-21.
      fetch partial-answer bug → stormcentral#1076, fetch.sh checks (639e0c7)

### ResourceQuota controller + admission (#124, P2) — COMPLETE 2026-10-29
- [x] `apimachinery::quota` (usage, scopes, formatting); apiserver
      `quota_admission.rs` (check all, CAS charge, rollback, charged-at);
      controller-manager `resourcequota.rs` (feeds + LIST for other count/,
      5 s settle, 5 min resync); units; `test/e2e/resource-quota.sh`; docs
- [x] Build VM at 46cd67b: 606 passed / 4 ignored (apimachinery 123,
      apiserver 302, controller-manager 101); golden-rustkube-4ecdc374808d
- [x] All 12 ResourceQuota conformance specs passed in the d5f48f5 run
      from goldens (10 failed at efbea2d); resource-quota 11/11 in rigs
      fe841f11a1, 3c6585e0ea, 7cb36fe72a, 51ef481a0b. #124 closed

### RuntimeClass (#135, P3) — COMPLETE 2026-10-29
- [x] node.k8s.io/v1 served; `builtin_admission::runtime_class`; scheduler
      `pod_requests` + overhead; units; `test/e2e/runtime-class.sh`; docs
- [x] Build VM at 1db5ca5 (covers a9f3fff #158, 8ea98d6 #120, 1db5ca5 #135):
      600 passed / 4 ignored (apiserver 301, scheduler 71);
      golden-rustkube-5a7defd5f41c (stormcos#366)
- [x] All five [sig-node] RuntimeClass conformance specs passed in the
      d5f48f5 run from goldens (failed at efbea2d); runtime-class rig 18/18
      on conform.g8.lo from the 01a3affcd023 golden after #277. #135 closed

### CRD schemas in /openapi (#120, P2) — COMPLETE 2026-10-29
- [x] `openapi_crd.rs` (v2 definitions, v3 index + documents), handlers take
      state; `CrdRegistry::all_versions`; register drops stale versions;
      units; `test/e2e/crd-openapi.sh` (rigs, kubectl explain); docs
- [x] Build VM at 1db5ca5 (covers a9f3fff #158, 8ea98d6 #120, 1db5ca5 #135):
      600 passed / 4 ignored (apiserver 301, scheduler 71);
      golden-rustkube-5a7defd5f41c (stormcos#366)
- [x] Only 2 of the 9 CustomResourcePublishOpenAPI specs passed (d5f48f5).
      993c3a3 false x-kubernetes-* flags dropped; 91a8de5 metadata =
      ObjectMeta's fields from the vendored proto; dd354b3 v2 drops
      preserve-unknown-fields. Golden apiserver 4fe0997b273e on conform.g8.lo:
      9/9 in 3 of 3 focused runs; crd-openapi 12/12 (with kubectl). #120 closed

### Startup manifests through admission (#158, P3) — COMPLETE 2026-10-29
- [x] `apply_one`: create → admit_create / CRD on_create; Reconcile →
      `resource::builtin_update` (now shared with PUT and guaranteed_update)
      / CRD on_update; Service claims released on a failed create; node
      ports carried by port name (upstream; fixes #132's matching); Service
      defaults on updates; `test/e2e/manifest-admission.sh`; docs
- [x] Build VM at 1db5ca5 (covers a9f3fff #158, 8ea98d6 #120, 1db5ca5 #135):
      600 passed / 4 ignored (apiserver 301, scheduler 71);
      golden-rustkube-5a7defd5f41c (stormcos#366)
- [x] Rig failures were real: the manifest pass refused LimitRange (apply
      table in discovery.rs drifted: 9 creatable stored built-ins missing).
      630e469 adds them + a unit holding the table to discovery; f453582 rig
      FAILs instead of KeyError. Golden apiserver ed8490a03a01 on conform.g8.lo:
      manifest-admission 12/12 ×3. #158 closed

### VMI verbs pause/unpause/softreboot/freeze/unfreeze (#141, P2) — COMPLETE 2026-10-29
- [x] `kubevirt::vmi_verb` → kubelet `/vmVerb` (rustkube-node#94), routes,
      discovery, edit role; unit; `test/e2e/vmi-verbs.sh` (rigs, stub
      kubelet); README/CHANGELOG
- [x] Build VM at 0cf5acd: 596 passed / 4 ignored (apiserver 298);
      golden-rustkube-2ac2b650df54 (stormcos#366); rustkube-node#94 closed
- [x] vmi-verbs 12/12 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b and on conform.g8.lo from golden 3e10ed08dcac; control
      efbea2d fails 9 (every verb 404). #141 closed. On a node: stormvm#42's
      medium suite can use them once both goldens are released

### ReplicationController controller (#125, P2) — BUILT 2026-10-07
- [x] `replicaset::Kind` (RS / RC), RC instance in runner; fullyLabeledReplicas;
      apiserver RC defaults; `replicationcontrollers/scale` + discovery;
      units; `test/e2e/replication-controller.sh` (rigs); docs
- [x] Build VM at 847dd1e: 595 passed / 4 ignored (controller-manager 99);
      golden-rustkube-04a64e29a29f (stormcos#366)
- [ ] replication-controller rig (stormcentral#512); close

### build-release.sh on the private volume (#156) — COMPLETE 2026-10-07
- [x] OUT under $TMPDIR, cargo's target dir, no root/persistent-mount
      header, sha256s; docs/releasing.md
- [x] `sc-build deploy/build-release.sh` at f14da25 as stormbuild on a
      build VM: 3 static binaries, 3 tarballs, 3 podman images under
      /work/job/tmp, sha256s printed, exit 0

### NodePort allocation + type changes (#132, P2) — COMPLETE 2026-10-29
- [x] `node_port.rs` (claim keys, `--service-node-port-range`, Plan
      commit/abort), create (admission) + update (put_object,
      guaranteed_update) + delete + startup/manifests; create failure
      releases claims; units; `test/e2e/service-nodeport.sh` (rigs); docs
- [x] Build VM at 706a807: 592 passed / 4 ignored (apiserver 295);
      golden-rustkube-e7ee3d193c98 (stormcos#366)
- [x] service-nodeport 16/16 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b. The four Services conformance specs no longer fail on
      allocation (efbea2d: NodePort 0 / no ClusterIP); at d5f48f5 they wait
      for their Deployment's Pods Ready (no kubelet on conform). #132 closed

### Presentation rendered and inspected (#160) — COMPLETE 2026-10-07
- [x] marp-cli 4.5.1 + Chrome 155 headless on a build VM (no browser on the
      VMs; Puppeteer fetched one into TMPDIR), PNGs decoded and read here.
      d521501: 12/12 fit, no clipping; fixed slide 7 breaks, slide 8 empty
      header, stale claims; 48cb177 re-rendered 12/12 fit. Command in the
      deck's header comment

### WATCH honours timeoutSeconds (#165) — COMPLETE 2026-10-29
- [x] `WatchParams::timeout` (400 on a non-integer), `WatchResponseOpts
      .timeout` on every watch path, deadline in the initial and live loops;
      units; `test/e2e/watch-timeout.sh` (rigs); README/design/CHANGELOG
- [x] Build VM at ad439d8: 589 passed / 4 ignored (apiserver 292);
      golden-rustkube-b15e040af5b3 (stormcos#366)
- [x] watch-timeout 6/6 on conform.g8.lo from the 01a3affcd023 golden
      after #277's reader fix (the test-machine failures were that reader);
      control efbea2d: every timeoutSeconds watch still open. #165 closed

### EndpointSliceMirroring (#133, P3) — BUILT 2026-10-07
- [x] `endpointslicemirroring.rs` (desired slices) + Service controller:
      Endpoints-by-name and mirrored-slice routes, mirror/remove, delete on
      Service deletion; own Endpoints labelled skip-mirror; units;
      `test/e2e/endpointslice-mirroring.sh` (rigs); docs
- [x] Build VM at 44b5ced: 587 passed / 4 ignored (controller-manager 98);
      golden-rustkube-fb045ca519eb (stormcos#366)
- [ ] endpointslice-mirroring rig (stormcentral#512); close

### In-place pod resize, apiserver half (#136, P2) — COMPLETE 2026-10-29
- [x] `handlers/pod_resize.rs` (merge resources/resizePolicy; upstream's
      ValidatePodResize incl. the version-skew guard), route, discovery;
      `qos_of`; units; `test/e2e/pod-resize.sh` (rigs); docs. Kubelet half
      filed rustkube-node#192
- [x] Build VM at 8ccfc7f: 584 passed / 4 ignored (apiserver 290);
      golden-rustkube-e4fb2ff2f004 (stormcos#366)
- [x] pod-resize 13/13 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
      51ef481a0b. The three InPlace Resize conformance specs fail on
      conform.g8.lo before and after ("3 pods not found running", 300 s: no
      kubelet there); they need a node + rustkube-node#192 (open). #136
      closed

### Legacy Terragrunt/RPM path (#157) — COMPLETE 2026-10-09
Owner (2026-10-09): A, retire. Delete deploy/terragrunt, deploy/packaging,
deploy/systemd (only the RPM used it), new-tcluster.sh, replace-master.sh,
ha-soak-test.sh, verify-cluster.sh; docs/terragrunt-deploy.md → short note.
Keep gen-pki.sh / gen-node-token.sh / renew-certs.sh (certificates.md, the
serving-cert rig); gen-pki.sh's default output leaves the terragrunt tree.
- [x] 683f812: deleted; docs (terragrunt-deploy note, releasing, audit,
      CHANGELOG), .gitignore; renew-certs.sh's stale restart advice (#105)
- [x] Build VM sc-build-03a0ebb50a at 683f812 exit 0; gen-pki.sh +
      renew-certs.sh (FORCE) run clean; #157 closed. No golden (no code)

### flowcontrol.apiserver.k8s.io/v1 (#118, P3) — COMPLETE 2026-10-29
- [x] Routes (+ cluster /status), discovery (/apis + APIResourceList),
      resources_for, kinds; flowcontrol/v1 proto vendored (release-1.36);
      bootstrap exempt/catch-all; protobuf unit; `test/e2e/flowcontrol.sh`
      (rigs); README/inventory/conformance/CHANGELOG. Not enforced
- [x] Build VM at 451d783: 582 passed / 4 ignored (apimachinery 120);
      golden-rustkube-f52d8fa2f47f (stormcos#366)
- [x] Both "API priority and fairness should support FlowSchema /
      PriorityLevelConfiguration API operations" conformance specs passed in
      the d5f48f5 run from goldens (failed at efbea2d); flowcontrol 29/29 on
      conform.g8.lo from the 01a3affcd023 golden after #277's reader fix
      (92f399a). Not enforced (as the issue scoped). #118 closed

### CR schema defaulting/pruning/fieldValidation (#121, P2) — COMPLETE 2026-10-14
- [x] 3ae6a5c `schema.rs` (defaults, prune, unknown paths; preserve-unknown,
      additionalProperties, embedded-resource, ObjectMeta), CrdDefinition
      .schema, Strategy::Custom{schema, validation}; create/PUT/PATCH/apply
      prune+default, Strict 400 (both upstream spellings) / Warn headers;
      GET/LIST default on read; Strict JSON + YAML duplicate keys.
      534c63f (migrate caller), b9a59d0 (own YAML duplicate scan)
- [x] Workspace on a build VM at b9a59d0: 541 passed / 4 ignored (apiserver
      264). #226, #227 (my intermediate failures) closed
- [x] `test/e2e/cr-schema.sh` (rigs, the conformance bodies); docs
- [x] cr-schema 9/9 in rigs 756f2f6c20 (pvetest1), a031a5d3fd (pvetest2) and
      every pvetest/C2NR0Q2 run since 10-13
- [x] Audit's C3/C4/C6 (flowsdn): validation is #240's — 88a0641
      `schema::validate` (type, required, enum, pattern, lengths, bounds,
      counts, list-type set/map; 422 field errors; ratcheting; nulls pruned
      where not nullable), 6 units (apiserver 333); cr-schema validation
      cases, Namespaced + Cluster. CEL, /status, watch defaults, format stay
      #240; SSA keyed-list ownership filed separately
- [x] cr-schema 34/34 in 1a879717b9 (pvetest2, e0778bf); #121 closed;
      progress on #240; #275 filed (SSA keyed lists). Golden for 88a0641
      with the next component build (after stormcentral#827)

### Service ports in strategic merge patch (#150, P2) — COMPLETE 2026-10-14
- [x] `ports` keyed by content: `containerPort` when entries carry it, else
      port + protocol (default TCP); `merge_id` used by merge, `$patch:
      delete` and `$setElementOrder`; unit test; README/CHANGELOG
- [x] Compiled + unit tests on a build VM at b677499 (534 passed); in every
      golden since 10-07
- [x] `test/e2e/service-ports-patch.sh` (suite `rigs`) 10/10 in a031a5d3fd
      (pvetest2, 399c4c0; the other 12 failures as before); #150 closed

### Multi-master safety and failover (#149, P2) — COMPLETE 2026-10-14
Real three-master hardware: none (owner on #162). The matrix that fits one
host is a rig: `test/e2e/multi-master.sh` (rigs-night) with lib.sh
`RK_ETCD_MEMBERS=3` / `RK_APISERVERS=3`. Existing coverage: scheduler
leader pause/kill (scheduler-failover), second apiserver (crd-restart),
paged snapshots (list-snapshot-race).
- [x] lib.sh multi-member/multi-apiserver; multi-master.sh; docs
- [x] cb30463 suite `multi-master` (the rig alone, 900 s budget, day):
      rigs-night only takes 22:00–07:00 and C2NR0Q2/server3/pvetest1 are
      refused (no release). Build VM: rustkube-test build/test/clippy clean
- [x] 83c1270bd5 (pvetest2, cb30463): 15/16, the fresh controller-manager
      on C made no Pods for e (no log in the pod tail) → bc181d8 diag NOTE
      lines. 74bc069ff0 (bc181d8): 14 passed, then LIST via C right after
      quorum returned errored (rig crashed); the no-quorum write was **403**
- [x] 776a392 namespace admission: only NotFound → 404 `namespaces "x" not
      found`, other read errors pass through (503); unit + MemStore
      unavailable switch; rig wants 5xx, retries C's LIST (timed). Build VM:
      644 passed / 4 ignored; 8619eaf clippy alias, changed files clean
- [x] 9fda2733bf, 2adaca0d28, 961df54f69 (776a392/ff88c93): 16/16 each. The
      e-Pods failure: deploy() ignored its create's answer while C was
      still catching up after quorum (f2b3e0f checks it; 76966b548e 18/18)
- [x] f782522 master partition (C → fastetcd via a userspace proxy the rig
      closes) + one member paused; bc5e494 waits for the proxy's cut (first
      run's 201 rode a live connection); b749b49 timings as NOTE lines.
      25/25 in 7d4962ed39, f39953a31e, a833054561 (pvetest2): takeovers
      17.7 s (paused CM), 15.8 s (killed scheduler), 15.9 s (partitioned CM)
- [x] #149 closed. Golden for 776a392 rides #147's request (after
      stormcentral#827). Separate machines' clocks/disks: hardware (#162)

### Least-privilege bootstrap RBAC (#176, P2) — COMPLETE 2026-10-13
Owner (2026-10-07): A now — one controller-manager role written from real
calls; C — the Node authorizer — as its own issue. Scheduler: upstream's
role + KubeVirt status writes.
- [x] c5978ee `control_plane_rbac.rs`: `system:kube-controller-manager` (read/patch/
      update/delete everything for GC + namespace deletion; create only what
      controllers create; no tokens, RBAC creates, impersonate, bind,
      escalate) and `system:kube-scheduler` (explicit reads; pod PUT,
      pods/status, PVC patch, VMI/migration status, lease, events); reconciled
      at boot; bindings repointed from cluster-admin (autoupdate opt-out)
- [x] exec/attach/portforward authorized as `create` (upstream ≥1.31), else
      `get *` would be a shell in any pod
- [x] lib.sh runs CM/scheduler as their own identities — every rig checks the
      roles; units; docs; C filed as #228
- [x] 4088060 on a build VM (job sc-build-c644eb2f68): workspace 543 passed
      / 4 ignored (apiserver 266), exit 0
- [x] rigs under the roles (lib.sh tokens for system:kube-controller-manager
      / system:kube-scheduler): 51/63 pass in 7cac9d5177, 5bdcbdac41,
      756f2f6c20 (pvetest1) and the C2NR0Q2 runs; no 403 for either identity
      (only aggregation's intended one); the 12 failures are each rig's own
      issue. rigs-night not run (stormcos#554). In every golden since 10-07;
      #176 closed, nodes stay #228

### Scheduler Events (#138, P2) — COMPLETE 2026-10-13
PodScheduled=False/Unschedulable landed with #194; the Events were missing.
- [x] `scheduler::events`: `Scheduled` on an acknowledged bind (spawned off
      the loop), `FailedScheduling` inside `report_pod_unschedulable` after
      its message-change check; unit for the shape; pod-limit.sh checks
- [x] Build VM at b677499 (534 passed / 4 ignored), at 343b5b5 (643 / 4,
      0 warnings) incl. `events_are_shaped_as_upstreams`
- [x] pod-limit 12/12 (Event checks included) in rigs 7cac9d5177,
      5bdcbdac41, 756f2f6c20 (pvetest1); 10d07cb in every golden since
      10-07 (latest be6ddd9ee998); #138 closed

### resource.k8s.io/v1 (DRA) served (#137, P2) — COMPLETE 2026-10-13
Owner (#137): "Please make sure there added, … after our turbomode merge"
(merged, #163). Conformance's 4 DRA specs are CRUD only.
- [x] 6fc3cbf: routes, discovery (+ manifest table), kinds/apiVersions,
      protobuf (`resource/v1` from release-1.34, imports all in 1.32);
      protobuf round-trip unit; discovery test
- [x] 9939bcb: `test/e2e/dra-crud.sh` (suite `rigs`); README/conformance/
      test README/CHANGELOG. Allocation filed as #225
- [x] Build VM at b677499 (534 passed / 4 ignored); dra-crud 41/41 in
      rigs fe841f11a1, 1c81036901, 3c6585e0ea (C2NR0Q2), 7cac9d5177
      (pvetest1); in every golden since 10-07 (latest 8227afb87508); closed

### #147: WFFC claims bind one at a time (PVC row) — SHIPPED 2026-10-14
#147's scale/multi-node acceptance waits for hardware (owner on #162: "when
we have hardware we will have it"). What the test machine measures today:
turbomode 7277704177 (pvetest1), 25 Pods each with a fresh WFFC claim:
request→scheduled p50 1.42 / p95 2.54 / max 2.58 s — over the < 1 s target,
node time excluded (plain Pods: 0.17 s p50). Cause: the PV binder's claim
controller runs `workers() = 1` (so two claims cannot choose one PV); each
bind is several writes (~100 ms), 25 in a line → 0–2.6 s.
- [x] 3d3b099 binder: 8 workers; `needs_choice` → `choosing` lock with the
      PV list read inside it; pre-bound (provisioned) PVs bind in parallel.
      Unit; workspace at 3d3b099 534 passed / 4 ignored
- [x] `test/e2e/wffc-latency.sh` (suite `rigs`); control branch
      `control/147-binder-1-worker` (ceaae45: 1 worker, rigs = wffc only)
- [x] Docs (storage.md, test/README, CHANGELOG) at 98f3624
- [x] 2026-10-13 rigs on C2NR0Q2: wffc-latency p99 2.195 / 2.269 / 1.871 s
      with 8 workers — still over 1 s. 9d7791a: the rig prints per-step
      percentiles (create→selected, selected→pv, pv→claim-bound,
      claim-bound→pod-bound) as `NOTE  ` lines, copied to the pod log
- [x] e3aa273d2c errored (C2NR0Q2 apiserver down). 84a3539: watchers read
      decoded lines (r.fp gave chunk sizes). 874df289d5 (pvetest1, 84a3539):
      p99 1.952 s; create→selected 0–1.47 s down the queue (scheduler's inline
      selected-node patch ~60 ms each), pv→claim-bound ~0.5 s (PV PUT + a
      second binder pass), Pods bound in a batch at ~1.6 s
- [x] 343b5b5 + earlier: scheduler `Write::Claims` — reserve, patches in the bind set;
      binder: pre-bound PV not rewritten, Bound status in the same pass.
      Units (scheduler 65, c-m 118); workspace 643 passed / 4 ignored, 0 warnings
- [x] 5bdcbdac41 (pvetest1, 343b5b5): p99 0.928 s, create→selected max
      0.16, pv→claim-bound p50 0.455, pod bound at claim-bound
- [x] Repeat 756f2f6c20 (pvetest1, 5f25dfa): p99 0.591 s (p50 0.458),
      create→selected max 0.09, pv→claim-bound p50 0.339 (largest left);
      94da6b412d (pvetest2) still queued. Distributions posted on #147
- [x] pvetest2 repeat 94da6b412d (5f25dfa): p99 0.279 s — three passes
      on two machines (0.928 / 0.591 / 0.279 s)
- [x] Golden: stormcentral#841 closed 2026-10-14; golden-rustkube-*-9184c2d39ee2
      (stage at stormcos f77591d, built 13:47Z after the last code commit;
      `component build rustkube` → unchanged), stormcos#421–423
- [x] Scale/multi-node acceptance split to #281 (hardware, #162); #147
      marked `shipped` 2026-10-14 (closes with the release)

### Metadata-only CRD watch decode error (#180, P2) — COMPLETE 2026-10-13
server1 (0.15.1) cilium agent: "unable to decode an event from the watch
stream" on its `as=PartialObjectMetadata` CRD watch; the rest of the message
(cut stream vs malformed frame) was never captured.
- [x] Code read: every metadata-only frame type is PartialObjectMetadata
      (ERROR a Status); client-go raises this error only for a real decode
      failure or a non-EOF/non-timeout stream error. stormcos manifests carry
      no non-string label/annotation values (the manifest loader stores YAML
      unchecked)
- [x] 0644141 units: every frame (ADDED, MODIFIED, DELETED with/without last
      state, BOOKMARK heartbeat/initial-end, initial ADDED, ERROR) decodes under
      Go's ObjectMeta types — pass (apiserver 255 at 0644141).
      `test/e2e/metadata-watch.sh` (suite `rigs`) written; needs a test machine
- [x] metadata-watch rig 14/14 in all three `rigs` runs on C2NR0Q2
      (fe841f11a1, 1c81036901, 3c6585e0ea)
- [x] b30eb37 `/test node-logs` (host /var/log/pods read-only): run
      a1e677f99d on C2NR0Q2 (12.14, cilium) — 244 pod logs from 10-10, 3
      cilium-agent logs, no watch-stream decode error; positive control
      flags cilium's format. Absent on the current release → #180 closed

### LIST/GET from the watch cache (#171, P2) — SHIPPED 2026-10-13
Every LIST and GET is a linearizable fastetcd Range, `resourceVersion=0`
included; #5's cached LIST was switched off and never back on. Upstream's
cacher rules: RV "" → store; RV 0 → cache; RV N (NotOlderThan) → cache once
at N; Exact → store at N; continuation → store pinned to the token's rev.
- [x] 241ce6a (+91c82b1): `storage::Read`; `ResourceStorage::{list_page_read,
      get_read}` from the resource-wide prefix cache (`read_page`/`read_one`,
      namespace by key range), 50 ms wait else store, `{rev}:{key}` tokens,
      Exact → store at N, RV unparseable → 400; built-in + CR LIST/GET;
      `apiserver_watch_cache_reads_total`. Units: apiserver 253; workspace at
      91c82b1 527 passed / 4 ignored. #222 (my type error) closed
- [x] `test/e2e/cache-reads.sh` (suite `rigs`)
- [x] `cache-reads` 14/14 in all three `rigs` runs on C2NR0Q2 2026-10-13
      (fe841f11a1, 1c81036901, 3c6585e0ea); README/metrics.md/CHANGELOG
- [x] In every golden since 2026-10-07 (latest be6ddd9ee998); marked
      `shipped` — server1 GET p99 < 20 ms (#187) after a release has it

### e2e rigs as pods on the test machines (#173, P2) — COMPLETE 2026-11-19
Owner (#162): "Persistent tests should be pods, and should live on forge."
- [x] 68e3260/f3cea50: test/build.sh stages binaries + upstream tools;
      `/test rigs` / `/test rigs-night` exec `test/rigs.sh`; requires.toml
- [x] First completed day run fe841f11a1 (C2NR0Q2, 1414947): 63 rigs in
      1048 s of 1800 s, 51 pass / 12 fail. No rebalancing needed
- [x] d9777b2: `/test` waits for run.sh (results reader `pgrep -x test`
      archived /results at the start: 1 of 63 logs kept); no-FAIL-line
      failures name the error line. Build VM: rustkube-test 9 pass, clippy 0
- [x] 1c81036901 still archived 1 file: the reader's `pgrep -x test` never
      sees the test (every component) — stormcentral#804; no /dev/fd in
      containers — stormpump#153. 183e8b9: failed rigs' tails in the pod log
- [x] 3c6585e0ea at 183e8b9: 51 pass / 12 fail, tails in the pod log; each
      failing rig's result posted on its issue (#83 #89 #107 #118 #123 #133
      #135 #139 #147 #158 #165 #191; #266 flaky 1/3)
- [x] multi-master (night list's first rig) passes as its own pod suite:
      7d4962ed39, f39953a31e, a833054561 (pvetest2)
- [ ] rigs-night as a pod: pve VMs install again (12.58), but every test
      image fetch fails, nodes' sbregistry has no forge peer (stormcos#271,
      2026-10-29). Proposed after it
- [x] Its other nine rigs on conform.g8.lo from the 01a3affcd023 goldens
      (2026-10-29): 8 pass (list-snapshot-race, crd-restart,
      scheduler-failover 68 s, schedule-latency, get-latency, watch-deadline
      423 s, serving-cert, client-cert-reload). deadlines: its stale-Lease
      check waited for `not-ready`, #267 made it `unreachable` (rig fixed
      19fd0b1, passes); its idle check fails, 15,022–15,564 requests in 60 s
      = #272's EndpointSlice loop (evidence posted there)
- [ ] First rigs-night pod run: stormcos#271 closed 2026-11-06, #272 closed
      (idle check should pass). Queued da598f8d8c (pvetest2, 91d0c9c; waits
      for the 22:00–07:00 Chicago window). Day check 5e9f2f7a9c (pvetest1):
      forge fetch works now, but the Job started while the node's registry
      was still `building` the image → ImagePullBackOff (stormcentral#512,
      evidence posted). Proposed after #512; close after a night run passes
- [x] First rigs-night pod run: ef4f9c31e3 (pvetest1, 1538895, 2026-11-07)
      ran all 10 night rigs in the pod, ~1025 s of the 7200 s budget: 8 pass;
      multi-master (KeyError 'items') is #310, serving-cert (renewal exit 1)
      is #328, both rig results, not pod faults. da598f8d8c ended `no VM
      3102` (pvetest2 not installed). stormcentral#512 closed 2026-11-19.
      #173 closed

### CR metadata.generation (#198, P2) — COMPLETE 2026-10-06
Nothing set or bumped `generation` on custom resources, so no controller could
report `status.observedGeneration` (stormcluster#12).
- [x] d715e20 (+ 5b2ca51, a function the refactor dropped) #128's
      `StatusField` → `Strategy::{BuiltIn, Custom{status_subresource}}`: CR
      create → `generation: 1`; main update → stored (1 if none), +1 when
      `spec` changed (with the status subresource) or anything outside
      `metadata` (without); `/status`, metadata-only writes and client-sent
      values leave it. Built-ins unchanged
- [x] Unit (apiserver 251); workspace at 5b2ca51 525 passed / 4 ignored.
      `test/e2e/cr-status.sh` 50/50 at 5b2ca51 (18 generation checks);
      control (generation writes removed) fails 18. #220 (the dropped
      function) closed
- [x] README/test README/CHANGELOG
- [x] golden-rustkube-06b2476d3811 (stormcos#333); #198 closed

### Stamp the requester on storage.storm.io CRs (#210, P1) — COMPLETE 2026-10-06
stormdrive#45's controller SARs the creator of a `DriveOperation` before
touching a drive; nothing unforgeable on the object said who that was.
- [x] 212c4d6 `requester.rs` + `admission::after_mutating` (with #98's
      check): create of a `storage.storm.io` object → annotations
      `storage.storm.io/requester` (username) and `…/requester-groups`
      (groups, `,`) over the client's; every update (PUT, PATCH, apply,
      `/status`) keeps the stored values; unstamped objects stay unstamped
- [x] Units (apiserver 250); workspace at c01668c 524 passed / 4 ignored.
      `test/e2e/requester.sh` 20/20 at c01668c (fastetcd v1.12.0); control
      (stamp disabled) fails 15, every stamp check
- [x] README/test README/CHANGELOG
- [x] golden-rustkube-4b49fa1fdefc (stormcos#333); #210 closed

### VM reads Starting while the kubelet retries a failed start (#209, P1) — COMPLETE 2026-10-06
rustkube-node#76: under Always/RerunOnFailure/running the kubelet retries a
failed start itself; the VMI stays `Pending`, `reason: FailedStart`. #104's
VM status reacted only to phase `Failed`, so the VM read Starting forever.
- [x] f9de8ac `virtualmachine.rs` `failing_start`: FailedStart (not finished,
      not Running) → `CrashLoopBackOff` + `Failure` condition with the VMI's
      reason/message; `startFailure`/recreate untouched (kubelet's retry)
- [x] Unit (controller-manager 83); workspace at f9de8ac 522 passed / 4
      ignored. `test/e2e/vm-runstrategy.sh` 17/17 at f9de8ac (fastetcd
      v1.12.0, 6 new); control (`failing_start` false) fails 3 — the
      CrashLoopBackOff/condition checks, #209's symptom
- [x] README/CHANGELOG
- [x] golden-rustkube-2b0e51a10e5a (stormcos#333); #209 closed

### CR main writes overwrite status (#128, P1) — COMPLETE 2026-10-06
A CRD version with `subresources.status`: upstream's main POST drops the
body's status, main PUT/PATCH/apply keep the stored status; rustkube stored
the submitted status. CRDs without the subresource keep whole-object
semantics; `/status` writes already copied only status (+ #78's RV check).
- [x] f88a92f `CrdDefinition.status_subresource` (per version; v1beta1
      spec-level too); `StatusField::{Writable, Kept}` through `put_object`
      and `patch_stored_object` (+ apply upsert); CR create drops status
      after mutating admission. Built-ins unchanged (`Writable`)
- [x] Units: apiserver 248 (3 new); workspace at 539786c 521 passed / 4
      ignored. #219 (the new unit's missing namespace) closed
- [x] `test/e2e/cr-status.sh` 32/32 at 539786c (fastetcd v1.12.0), the
      audit's C12 for namespaced + cluster CRDs with/without the
      subresource; control (StatusField a no-op) fails 12, every isolation
      check
- [x] README/test README/CHANGELOG
- [x] golden-rustkube-2fad9faa182a (stormcos#333); #128 closed

### Client cert + client CA reload (#105, P1) — COMPLETE 2026-10-06
controller-manager/scheduler built reqwest once with `Identity::from_pem`;
the apiserver read `--client-ca-file` once. Owner (#161): certs roll
routinely, so every stormcert renewal hits this.
- [x] 0458394 `apimachinery::tls_reload`: `certified_key` (#93 key match,
      moved from apiserver), `ReloadingKey` (server + client resolver),
      `watch_files` (30 s, content); `api_client_builder` hands reqwest a
      rustls config (webpki + CA roots, insecure, reloading identity).
      CM/scheduler follow `--client-certificate/--client-key`; apiserver
      follows `--client-ca-file` (each accept takes the current config)
- [x] Units: apimachinery 113 (5 new), apiserver 245 (CA-rollover
      handshakes); workspace at 0458394 518 passed / 4 ignored
- [x] `test/e2e/client-cert-reload.sh` 17/17 at c6ddc0e (fastetcd v1.12.0);
      control (reloads disabled) fails 10, every reload-dependent check.
      serving-cert.sh 15/15 at c6ddc0e. #218 (rig's own Lease setup) closed
- [x] README/certificates.md/test README/CHANGELOG
- [x] golden-rustkube-2ff21b9b3958 (stormcos#333); #105 closed

### RBAC escalation prevention (#98, P1) — COMPLETE 2026-10-06
Any caller who may write RoleBindings could bind any role (cluster-admin in
one's own project). Upstream's rule: a binding needs `bind` on the role or
every rule of it held in the binding's scope; a role needs `escalate` or
every rule held (`aggregationRule` needs `escalate`). `system:masters`
bypasses; ownerReferences/finalizers-only updates skip.
- [x] 233fc70 `escalation.rs`: upstream `Covers` (verbs/groups/resources,
      `*/sub` and `pods/*`, resourceNames, nonResourceURLs) + `confirm`,
      called from `admission::admit` after mutating webhooks (POST, PUT,
      PATCH, apply upsert); 157c0fd Debug for RbacEngine
- [x] apiserver 246 tests (10 new); `test/e2e/projects.sh` 46/46 at 157c0fd
      (fastetcd v1.12.0); control (hook disabled) fails 8 — every escalation
      allowed. Workspace at 157c0fd 514 passed / 4 ignored, exit 0
- [x] README/inventory/presentation/CHANGELOG. Namespace writes stay
      cluster-scoped (safe to relax now; not changed)
- [x] golden-rustkube-5b99942904e4 (stormcos#306); #98 closed

### Admission webhooks wired into writes (#82, P1) — COMPLETE 2026-10-06
`admission.rs` had a webhook client nothing called; rewritten and wired.
- [x] cfb2858: request attributes in a task-local set after RBAC (user,
      verb, GVR, subresource, namespace, name, dryRun), Warning headers back;
      configs from watch-cache views; rules/scope/subresources, namespace +
      object selectors, failurePolicy, timeout, sideEffects vs dry-run
      delete, IfNeeded reinvocation, JSONPatch; url or Service (ClusterIP,
      SNI `svc.ns.svc`, caBundle). Wired: create (built-in, CR, apply upsert,
      pods/eviction), PUT, `guaranteed_update` (PATCH, /status), DELETE and
      deletecollection items. admissionregistration + events.k8s.io exempt;
      CEL matchConditions not evaluated (webhook called)
- [x] apiserver 236 tests (7 new); `test/e2e/admission-webhook.sh` 20/20 at
      54d14ba (fastetcd v1.12.0); workspace 504 passed / 4 ignored at 54d14ba
- [x] Control: rig with pkg/apiserver/src at 483a93b fails 13 of 20 (no
      webhook called); #214–#216 (control attempts) closed. CEL
      matchConditions follow-up filed #217
- [x] golden-rustkube-af640123da63 (stormcos#306); #82 closed

### 100-Pod burst: ~12 Pods wait ~60 s (#205, P1) — COMPLETE 2026-10-28
Not a scheduler defect. Run 7277704177: BestEffort `sleep 60` Pods on one
node that allows 110 Pods (rustkube-node's fixed `allocatable.pods`), with
~23 already in use. `peak_running_observed` was 87, and the rest bind as the first sleepers go
Succeeded at ~t+60 s (#194's limit, covered by `test/e2e/pod-limit.sh`).
The scheduler has no 60 s interval (backoff caps at 25.6 s).
- [x] Evidence + question posted on #205; `wait-owner`: A = configurable
      kubelet max-pods (rustkube-node + stormcos, recommended), B = QA burst
      sized/reported against free slots (stormcos_qa). No rustkube change
- [x] Owner chose A (2026-10-06): kubelet `--max-pods`, stormcos sets 250;
      stormcos_qa reports slot wait apart from scheduling. Filed
      rustkube-node#165, stormcos#329 (also: Cilium /24 ≈ 252 IPs per node);
      commented stormcos_qa#49. #205 proposed after stormcos#329
- [x] stormcos#329 (kubelet `--max-pods 250`) on pvetest1: turbomode
      a5c0e611fe (12.58 era): allocatable 250, 212 free, peak running 100,
      no Pod waited for a slot; request→scheduled p50 0.47/0.56 s, max
      0.84/0.94 s (was p95 60.86 s). Run failed on 2–6 Pods never finishing
      — rustkube-node#250 / stormcos_qa#74, not scheduling. #205 closed

### Serving-cert reload applies a mismatched pair (#93, P1) — COMPLETE 2026-10-06
`tls.rs` reloaded any pair that parsed; `renew-certs.sh` moved the key before
the cert and kept going after a failed `openssl` (called under `||`, so no
`set -e`). Owner (#161): certs roll routinely, so every renewal hits this.
- [x] 125c277 `tls.rs`: one `certified_key` for startup and reload, refusing
      a pair whose public keys differ (`CertifiedKey::keys_match`; rustls
      0.23.37's ring keys all implement `public_key`); refusal logged once per
      change. `renew-certs.sh`: every step checked, pair compared before
      either move, `.new` files removed on failure
- [x] 3 units (P-256/P-384/Ed25519; mismatch; reload sequence); workspace at
      df35db7 500 passed / 4 ignored. `test/e2e/serving-cert.sh` 15/15 at
      70fad5e (fastetcd v1.12.0); control (be616b7 tls.rs) fails 4 — #213,
      with #211/#212 (shallow-checkout attempts), closed as control runs
- [x] golden-rustkube-ce546522ad2b (stormcos#306); #93 closed

### Reflector's own WATCH deadline is not an outage (#207, P2) — COMPLETE 2026-10-06
The apiserver ignores `timeoutSeconds` (#165), so the reflector's 330 s
reqwest timeout ended every watch: warn "reflector WATCH reconnecting",
`Unavailable` (feed unsynced, GC fails closed), reconnect counted + backoff,
recovery wake on resume — every informer, every ~5.5 min.
- [x] 4bfa056 `reflector.rs`: own deadline (tokio, not reqwest's) on an open
      stream → `Ended::Deadline`, resume from the revision at once; not
      opening by the deadline stays an outage. 2 units (200 ms deadline);
      apimachinery 108 passed; mutation (arm → Unavailable) fails the unit
- [x] `test/e2e/watch-deadline.sh` 5/5 at f0fb56a (fastetcd v1.12.0): 59
      watches resumed in 400 s idle, 0 reconnect warnings, 0 LISTs, counter
      0. Control (4bfa056 reverse-applied): cm 51 + scheduler 8 warnings,
      73 LISTs. Rig counts no `/metrics` scrapes (apiserver labels them list)
- [x] Workspace sc-build at 3a6567e: 488 passed / 4 ignored, exit 0.
      golden-rustkube-b8b7c5ee394b (stormcos#109); #207 closed

### VMI launcher Pods (#203, P1) — COMPLETE 2026-10-05
Owner's choice B on rustkube-node#88: the controller makes KubeVirt's
`virt-launcher-<vmi>-<5>` Pod for a pod-network VMI; the kubelet adopts it
(CNI identity for Cilium, status, deletion — rustkube-node 3d9d26e).
- [x] 61d980e `vmilauncher.rs`: one per VMI on `status.nodeName` (pod
      network as stormvm-spec reads it, `storm.io/bridge` wins); a migration
      target gets its own (`kubevirt.io/migrationJobUID`); the loser is deleted
- [x] 6 units; `test/e2e/vmi-launcher.sh` 24/24 at 944b89f (fastetcd v1.12.0);
      workspace 495 passed / 4 ignored; vm-runstrategy, vmi-migration,
      pod-limit 0 failed. #204 (rig's own bug) closed
- [x] Filed rustkube-node#152: kubelet's `launcher_for` must pick the Pod on
      its own node (two launchers during a migration)
- [ ] After the release + rustkube-node#88 on a node: a pod-network VM is a
      Cilium endpoint with the launcher's identity, reachable via a Service

### VirtualMachineInstanceMigration (#184, P3) — COMPLETE 2026-10-05
Control-plane half of live migration; node half rustkube-node#40. Contract is
upstream KubeVirt's VMI `status.migrationState`: controller writes
`migrationUid`/`sourceNode`/`mode`; **scheduler** writes `targetNode` (same
filters/scores as VMI placement, source excluded, charged on both nodes until
failed or moved); target kubelet writes `targetNodeAddress`; source kubelet
writes `startTimestamp`, then `completed`/`failed`/`endTimestamp`; controller
moves VMI `status.nodeName` on success.
- [x] CRD filed as stormcos#288 (YAML in the issue); rigs apply it
- [x] 8d998b4 controller `vmimigration.rs` (finalizer abort, one per VMI,
      5 min / 15 min timeouts); 18f5465 + 74d9eeb scheduler target placement
      (74d9eeb: target stays charged between `completed` and the move — the
      first rig run caught the overcommit); e33455a `migrate` subresources
- [x] Units: apimachinery 106, apiserver 229, controller-manager 76,
      scheduler 69 (21 new). `test/e2e/vmi-migration.sh` 31/31 at 74d9eeb
      (fastetcd v1.12.0); workspace 489 passed / 4 ignored at 549180c;
      vm-runstrategy 10/10, pod-limit pass, scheduler-failover 19/19.
      golden-rustkube-0fe2570f64e1 (stormcos#164). #202 (first rig run) closed
- [ ] After stormcos#288 + rustkube-node#40: a real stormvm migration
      between two nodes (the abort-while-sending path is unit-tested only)

### Pod-bound ServiceAccount tokens (#182, P2) — COMPLETE 2026-10-05
Upstream/OpenShift shape, for stormcos#54's metadata service (host-network
caller → its pod by token). Kubelet half (send boundObjectRef + 3607 s,
rotate) is rustkube-node#122.
- [x] 725dfcd TokenRequest: `spec.audiences` (default `--api-audiences`,
      default `--service-account-issuer` = https://kubernetes.default.svc),
      `expirationSeconds` 600 s–2^32 (absent stays 24 h until the kubelet
      rotates), `boundObjectRef` Pod/Secret/Node (uid 409, other SA 400);
      claims `iss`/`aud`/`nbf`/`jti` + `kubernetes.io`; pod-bound 3607 s → 1 y
      + `warnafter` (`--service-account-extend-token-expiration`)
- [x] Authentication + TokenReview: `aud`/`iss` checked when present; bound
      SA/object must exist with that uid (deleted > 60 s ago → refused), watch
      cache first, store on a miss; review reports uid + extra pod/node/jti
- [x] apiserver 227 tests (11 new); `test/e2e/bound-token.sh` 24/24 at
      214cb8f (fastetcd v1.12.0); workspace at c8a4404 468 passed / 4 ignored;
      token-auth.sh 13/13. golden-rustkube-152acbd2a1f0 (stormcos#164)
- [ ] After the release + rustkube-node#122: a pod's projected token is
      pod-bound (TokenReview names the pod), stormcos#54 can use it

### Static admin token from install-config (#188, P1) — COMPLETE 2026-10-03
storminstall writes `apiToken`; stormpump#78 writes it on first boot to
`/state/config/token-auth.csv` (0600) as `<token>,system:admin,system:admin,"system:masters"`;
stormcos#256 passes the path. Path and format accepted as stormpump wrote them.
- [x] ff60e56 `--token-auth-file` (upstream format), checked before the JWT
      path, constant-time; re-read every 5 s (late write, rotate, remove →
      revoked; malformed keeps last good); TokenReview answers it
- [x] 16b5c0b README/certificates.md/CHANGELOG; 3aed8c3 `test/e2e/token-auth.sh`
- [x] sc-build: apiserver 216 (9 new); token-auth.sh 13/13 at 3aed8c3;
      workspace at 3aed8c3 exit 0 (448 passed, 4 ignored)
- [ ] After the release + stormcos#256: `sc` logs in to a node with apiToken

### Pods beyond allocatable.pods (#194, P1) — COMPLETE 2026-10-03
pvetest1 (stormcos_qa turbomode run 4bbb76be8f): 1,000 Pods bound to a node
with `allocatable.pods = 110`. The scheduler charged CPU/memory per node but
never counted Pods, and a pod with no requests skipped resource fit entirely.
- [x] 6957abc: NodeUsage counts non-terminal Pods per node (watch, binds,
      assumptions; VMIs take no slot — rustkube-node runs no virt-launcher);
      "Too many pods" at `allocatable.pods`, checked after the placement
      filters (f1c902b)
- [x] Unplaceable Pod gets `PodScheduled=False/Unschedulable`, "0/N nodes are
      available: …", written only when it changes (Events stay #138)
- [x] At f1c902b: scheduler units 65 (3 new); `test/e2e/pod-limit.sh` 10/10;
      workspace 448 passed / 4 ignored; indexed-safety 11/11,
      scheduler-failover 19/19, schedule-latency pass (fastetcd v1.12.0).
      golden-rustkube-71422ae4df20 (stormcos#164); #194 closed. Auto-filed
      #195 was the rig's own expectation, closed
- [x] Verified on a release (#197, closed 2026-10-07): turbomode 7277704177 on
      pvetest1 — 100 Pods, peak 87 running beside the node's own, the rest
      bound as the first finished (~t+60 s); 4bbb76be8f had 1,000 bound

### Scheduler create→bind latency (#190, P1) — SHIPPED 2026-10-03
pvetest1: kubelet's `scheduled` 156→393 ms, growing pod by pod. That number is
kubelet-seen minus PodScheduled.lastTransitionTime, which the scheduler writes
truncated to whole seconds: 0–999 ms of truncation, drifting as pods are
created ~4.0x s apart. Measured, the scheduler was never the cost.
- [x] `test/e2e/schedule-latency.sh` (release, fastetcd v1.12.0). Baseline at
      3c02bd3: create→seen p50 20.7 / p99 187 ms, no growth; scheduler share
      0.2–0.8 ms; burst backlog from serialized binds; 40–45 ms watch stalls
- [x] c5b86b9 `storm.io/scheduled-at` (µs) + e2e metric emitted; 163b633 binds
      in the background (≤16, Work held, aborted with the term); 97d56d9
      apiserver TCP_NODELAY (the 40 ms stalls were Nagle + delayed ACK)
- [x] After: ADDED→bound p50 4.0–7.3 ms; scheduler share p99 ≤ 1.1 ms over 10
      runs; e2e p99 9.7–30 ms in 7 of 10 runs, the rest in the bind write's
      datastore time (etcd update ≈ 96% of the PUT) on shared dev.
      scheduler-failover 19/19 at 163b633
- [x] Workspace build+test 444 passed (4 ignored); indexed-safety 11/11 at
      5d74187; golden-rustkube-c058e790b5a4 (stormcos#164); kubelet half
      filed rustkube-node#135; #190 marked `shipped`; #192 (baseline control
      run) closed
- [ ] After the release + rustkube-node#135: pvetest1 burst of 20 by
      `storm.io/start-timing`, p50 < 20 / p99 < 50 ms

### DaemonSet pod not back after a reboot (#189, P0) — BLOCKED on stormcos#231, 2026-10-02
server3 11.65 after the power-cut reboot: cilium DS desired 1 / current 0, no pod.
- [x] Live read of server3's kube-system (23:22 UTC): the DS controller is NOT
      stuck — 32 cilium pods created since the reboot, each `Failed` within
      seconds (kubelet: the pod's own `kubernetes.io~empty-dir` /
      `~projected` dirs "do not exist" on the host), deleted, and the per-node
      failedPodsBackoff (1 s → 15 min, as upstream) leaves no pod between tries
- [x] Filed rustkube-node#129 (kubelet writes per-pod dirs in its own view,
      checks/mounts them on the host; pod should stay Pending, not Failed);
      #189 proposed after it. No rustkube code change: DS behaviour matches
      upstream's DaemonSet controller
- [x] rustkube-node#129 closed: the real cause is a full 60 MiB host root
      holding /var/lib/kubelet + /var/log/pods (ENOSPC). Kubelet now keeps such
      pods Pending with the errno (golden-rustkube-node-cb302b196e29). Host fix
      is stormcos#231 (832b627, own kubelet-data/pod-logs volumes), open until
      released; #189 proposed after it. Still no rustkube change
- [ ] After stormcos#231 ships: stormcentral's install reboot step shows
      cilium back within 60 s of node Ready; then close #189

### Stored CRDs not served after a restart (#185, P0) — SHIPPED 2026-10-02
server3 11.62 reboot: CRDs listed, but cilium.io/kubevirt.io gone from /apis and
every CR 404s. Boot registers CRDs from ONE datastore LIST whose error is
swallowed (`if let Ok`), nothing re-reads them, and /readyz is always "ok".
- [x] cc1282b: boot load pages (50) and retries a minute, logs the count;
      `follow_stored_crds` follows the CRD prefix via the watch cache (1 s
      version check; unregisters only CRDs it saw stored then gone); manifest
      Reconcile re-registers a CRD; /readyz 503 until registered
- [x] `test/e2e/crd-restart.sh` 43/43 at cc1282b (restart, reboot with store
      up 15 s after apiserver, second apiserver create/delete); apiserver 207
      unit tests incl. 2 new. Control (fix reverse-applied): fails the
      cross-replica create (404 after 20 s); restart/reboot pass on the old
      code too — the rig does not reproduce server3's failed boot read, whose
      exact cause (error vs empty answer) is unproven. Workspace sc-build
      at fc4565f exit 0
- [x] golden-rustkube-3dd6c1c7903f (stormcos#164); #185 marked `shipped`;
      install reboot check filed stormcentral#289
- [ ] After the release, server3 reboot test: cilium.io in
      /apis, cilium ready (stormcentral install reboot step)

### Docs refresh from the code (since 2026-09-25) — COMPLETE 2026-10-02
- [x] `git log --since=2026-09-25`: no CLI/port/API change since the 09-29
      audit; RBAC-from-cache (#177), informer/backoff/lease/scheduler fixes
      (#144/#145) described in README, event-driven-design, test/README
- [x] #174/#175: fastetcd#50 fixed (v1.6.1), fastetcd's other clients,
      `stormblock-csi` class, stormcos#170, stormblock#111, stormcos_qa#25,
      #140 answered "Goldens"
- [x] How it ships re-checked at stormcos bb347bf4: stormd goldens compiled
      from a rustkube checkout by stage mode; component golden unmounted
      (stormcos#62). Known gaps named in place: #171, #172, #173, #176, #182
- [x] Audit addendum in docs/changes-since-2026-09-18.md

### Single-object GET latency (#177, P0) — IN PROGRESS 2026-10-01
server1 11.57: GET of one Lease/CRD 1.3–3.7 s while lists take ms; cilium-operator
loses its lease. GET/LIST handlers and fastetcd Range are the same path, so the
suspect is per-request RBAC (LIST every ClusterRoleBinding + GET each role, all
linearizable datastore reads) for every non-system:masters client.
- [x] `test/e2e/get-latency.sh` (d1ef838): baseline — SA GET = 4 datastore
      calls (LIST CRBs + 2 role GETs + object), idle p50 11.7 ms vs admin 1.6;
      under 40-client renew load SA GET p99 1182 ms. Metadata-only CRD watch
      decodes fine (checked; not a defect on the rig)
- [x] 04c3e32: RBAC from watch-cache views (allow from memory, deny re-checks
      the store). 6762513: 1 datastore call per GET; idle SA p50 1.4 ms debug /
      0.2 ms release; grant immediate, revoke < 10 ms; apiserver 205 tests pass
- [x] Remaining load latency is the datastore, filed fastetcd#71 (P0): direct
      v3-gateway probe under the same load, linearizable Range p50 139–157 ms,
      p99 0.4–6.9 s; serializable p50 0.2–0.5 ms; fastetcd ~9% CPU
- [x] 7b36d52: rig 0 failed on release + fastetcd v1.8.0 and debug + v1.6.1;
      28da43f: workspace tests pass (4 datastore tests ignored)
- [x] golden-rustkube-0162589e3b0a (stormcos#164); #177 proposed after fastetcd#71
- [x] fastetcd#71 fixed in fastetcd v1.9.0 (bb3e828). Rig on release +
      v1.9.0: 0 failed; under load SA GET lease p99 998 → 68 ms (p50 1.1 ms),
      store linearizable p99 80 ms; idle p99 2 ms
- [x] 2026-10-01 server1 still on rustkube 0.15.1, GETs 0.4–0.9 s: neither
      golden installed yet. #177 marked `shipped` (closes with the release)
- [ ] After the release: server1 GET p99 < 50 ms, cilium 1/1, coredns Ready

### Controller deadlines (#144) — COMPLETE 2026-09-29
Poll loops were already gone from controller-manager source (remaining sleeps:
error retry, startup wait, Lease retry). The new rig found three live defects.
- [x] `test/e2e/deadlines.sh`: cron +0.0–0.1 s, Job deadline +0.1–2.7 s,
      Event TTL +0.1–1.7 s, Lease grace +0.1–1.4 s after their deadlines; RS
      backoff 10 s; GC deletes proven-absent owner's dependent, keeps an
      unserved-kind owner's; idle control plane 0 API requests in 60 s
- [x] Fixed: RS re-counted its retained Failed Pod every pass, never replaced
      it (f58498c); cleared backoff requeued itself at zero delay (25eadee)
- [x] Fixed: 45 s heartbeat BOOKMARKs reached subscribers as empty calls —
      GC/namespace re-discovered and restarted every worker, scheduler
      re-queued pending Pods; routine reconnects were resets (abff0aa)
- [x] 4024040: deadlines 9/9, indexed-safety 11, indexed-selectors 14,
      daemonset-nodes 10, scheduler-failover 19, vm-runstrategy 10, exit 0.
      5c450f5: workspace 441 passed, 4 ignored. Auto-filed #167–#170 closed.
      Live latency stays #147; no golden (#163)

### Shared watches and work queues (#143) — COMPLETE 2026-09-29
WorkQueue, reflector, reactor::WatchHub and informers::Hub were on main with
unit/fake-HTTP tests; this added the integration coverage the handoff named.
- [x] `apimachinery/src/turbomode_tests.rs` (82d75f6): 8 tests, scripted API
      server over HTTP — sharing/seed, outage (unsynced, resume from RV), 410
      and malformed-frame relists, LIST/EOF backoff, cancellation, one extra
      pass per busy pass, idle watch wakes nothing. 20/20 repeats on dev;
      five mutations (no relist, dropped dirty, no abort, outage stays synced,
      EOF hot loop) each fail a test
- [x] de33401: whole workspace 439 passed, 4 datastore tests ignored, exit 0.
      Controller poll loops are #144; live latency #147. No golden (#163)

### Watch-cache revision waiters (#148) — COMPLETE 2026-09-29
Notify-based wait, register-before-check, bounded store fallback and
termination wake were already on main (turbomode merge); this added the tests.
- [x] Wait budget as a parameter; `last_progress` on tokio time (ac7ec79)
- [x] Scripted-store tests: pump wake, re-seed wake (unchanged/changed),
      deadline fallback, pump-end wake, cancellation, 2000-round multi-thread
      stress. Mutations fail them: registering after the check hangs the
      stress; dropping pump/re-seed notifies fails three tests
- [x] c516a59: apiserver 205 passed, sc-build exit 0. No API LIST handler
      calls `WatchCache::list` (LIST reads the datastore); no latency
      attribution claimed — that is #147. No golden (#163)

### Event-driven scheduler (#145) — COMPLETE 2026-09-29
Wakeups, serialized accounting, separate renewal and term cancellation were
already on main from #146; this closed the renewal and fault-test gaps.
- [x] Lease renewal retries transient failures until 10 s after the start of
      the last successful attempt (`apimachinery::lease::hold`, scheduler +
      controller-manager). 594e19d: units 96/61/59 passed, exit 0
- [x] Found + fixed: a Pod with a missing claim kept a reservation pinning it
      to its first-choice node (d455412); unit test + rig control proves it
- [x] test/e2e/scheduler-failover.sh 19/19 on four runs (d455412, c4aec6c),
      takeover 16–18 s, no overcommit, stale leader bound nothing; the
      pre-fix control fails; indexed-safety 11/11. Live 3-master gate is #149

### #146 dev acceptance — COMPLETE 2026-09-29
- [x] Read #146, instructions and #163; continued on main. fastetcd#50 fixed
      in v1.6.1; pin API rigs to that release and report its exact commit.
- [x] Baseline 9f622fa: five-crate tests 409 passed, four storage tests ignored,
      docs passed; sc-build exit 0, 34 s. Full workspace at 226d84e: 418 passed,
      four ignored, docs passed; twenty fresh short rigs 5/5 each, exit 0, 510 s.
- [x] 52188fb: 174 complete/paginated LIST snapshots match 400 creates;
      exact WATCH suffixes at distinct early/middle/late revisions and selector
      14/14 pass; sc-build exit 0, 139 s.
- [x] Matrix: safety 11/11 and CSI expansion 14/14 at 52bbb89; DaemonSet
      10/10, VM 10/10 and short 5/5 at ec59f74 (exit 0, 206 s).
- [x] Corrected DaemonSet test status-convergence barrier (#164, closed).
      Hardened listener selection after compilation, outside ephemeral ports;
      original peer transport error #166: diagnosed 2026-10-07 as the peer-port bind failing (EADDRINUSE; tonic 0.12.3 serve exits only there), closed; fastetcd#137 to print the OS error.
- [x] Investigated #153 startup membership and #154 POST timeout. Fresh
      checks pass; original causes remain unproven. Preserve both open issues.
      #165 tracks ignored WATCH timeoutSeconds; probe uses client deadlines.
- [x] Updated README, design, handoff, test docs and changelog; #146 closure
      report records dev acceptance and retained incidents. Live validation
      stays #147/#149. No production code/version change or golden requested.

### Merge turbomode into main (#163) — 2026-09-29
Owner instruction #163 supersedes the earlier branch-only restriction below.
No golden or release is requested; unfinished acceptance stays on its issues.
- [x] Read #163 and open issues; fetched main is already in turbomode history.
- [x] Merge origin/main into turbomode: already up to date; no conflicts.
- [x] turbomode ae000d5: sc-build cargo build --workspace --locked &&
      cargo test --workspace --locked passed (51 seconds, remote exit 0).
- [x] Refreshed branch-status docs; merged with --no-ff and pushed main
      at 908d078. Same whole-workspace build/tests passed (48 seconds,
      remote exit 0). No conflicts; main was already a turbomode ancestor.
- [x] Both runs: 418 passed (apimachinery 91, apiserver 198,
      controller-manager 61, scheduler 59, test container 9); four storage
      tests ignored because they require a datastore. Doc tests passed.
      Local build-history append is read-only; remote results above passed.
- [x] Merge scope complete; closure report prepared for #163. Continue
      #142–#149 on main; #146/fastetcd#50 snapshot safety, #147 latency,
      #149 multi-master, and #153–#155 investigations remain unverified.
      Existing owner decisions #140/#161/#162 are unchanged by this merge.
Version remains v0.18.0 plus unreleased changes: this is the requested branch
integration, not completion or release of the pending turbomode feature work.

### Documentation audit — 2026-09-29
- [x] Compare history since 2026-09-18 and current source with README and every docs page.
- [x] Verify CLI defaults, listeners, API limitations, built-in stormblock PVC ownership and delivery tooling; distinguish turbomode from shipped behavior.
- [x] Track unsupported promises: existing #90/#114/#140, newly filed #156/#157; storage corrections address #112/#117.
- [x] Pushed refresh 635855d and review b906e58. All 41 CLI flags covered;
      26 relative documentation link targets resolve. sc-build at 635855d:
      five libraries 409 passed, 4 datastore-dependent tests ignored, doc tests
      passed; remote exit 0 in 33 seconds (local log append read-only).
      #112/#117 closed with documentation evidence; #114 workflow removal (done
      2026-10-08) and
      #156/#157 tooling gaps remain open. No code/version change or golden.

### History (condensed)

Phases 0–3 (v0.1.0–v0.3.0, March 2026) scaffolded a 10-crate orchestrator:
store, apiserver, scheduler, controllers, kubelet, proxy, DNS, CNI, cloud. The
kubelet/proxy/CNI moved to rustkube-node, DNS went external, the embedded
stormforce store was replaced by fastetcd, and the crates were renamed to the
upstream-shaped `cmd/` + `pkg/` layout. The Phase 0–3 checklists described
that repo and were removed on 2026-09-24 (#80): several of their items were
never wired (webhooks, aggregation, preemption) or never written (the cloud
provider). What exists now is in the README; the release history below says
when each piece landed.

### Findings from the docs pass (#80)
- [x] Admission webhooks are never called (#82) — wired 2026-10-06
- [x] Aggregation proxies nothing (#83) — wired 2026-10-07 (rig waits)
- [x] Scheduler never preempts (#84) — preemption wired 2026-10-07
- [x] `schedulingGates` honoured (#87, 2026-10-07): SchedulingGated, add
      refused; build blocked on stormcentral#544
- [x] PriorityClass / TokenReview missing from `/apis` (#85) — fixed in
      6fd2722; discovery was exercised by the efbea2d conformance run.
- [x] `/scale` subresource (#86, 2026-10-07): apps + CRDs; build blocked on
      stormcentral#544
- [x] `--data-dir`, `--cluster-domain` (#88) — COMPLETE 2026-10-29.
      Owner: B. `--tls` cert kept in `<data-dir>/apiserver.crt|key` (reused
      while valid, matching, > 30 d left and carrying
      `kubernetes.default.svc.<cluster-domain>`; else regenerated; unwritable
      dir → warn, in memory), files followed like `--tls-cert-file`. Units;
      rig `tls-data-dir.sh`; README/certificates.md/CHANGELOG
  - [x] 0e83609; build VM sc-build-639371ce9d: 609 passed / 4 ignored
        (apiserver 316, apimachinery 124; 6 new units)
  - [x] 8f26005 (clippy type_complexity); 0 rustc warnings, my files
        clippy-clean; workspace clippy backlog filed #263.
        golden-rustkube-d633117067d8 (stormcos#424)
  - [x] tls-data-dir 14/14 in rigs fe841f11a1, 3c6585e0ea, 7cb36fe72a,
        51ef481a0b and on conform.g8.lo (apiserver 4fe0997b273e); control
        efbea2d fails 11 (nothing written, a new cert each start). #88 closed
        2026-10-29
- [x] HPA placeholder (#89) — real HPA on cadvisor metrics 2026-10-07 (rig waits)
- [x] Metrics: reconcile metrics, buckets, authenticated `/metrics` (#90)
      — 2026-10-07 (rig waits)
- [x] Gateway controller: hardcoded address, overwrites foreign classes (#91)
      — own classes only, no address, Programmed=False (2026-10-07)
- [x] Serving-cert reload applies a mismatched key/cert pair (#93) — fixed 2026-10-06
- [x] `/status` PUT is conditional on the body's `resourceVersion` (#78),
      all four handlers; `test/e2e/status-rv.sh`

### Open, found since the docs pass
- [x] GC deleted a live Deployment's ReplicaSet (#99): protobuf creates were
      stored with `uid: ""`; fixed in v0.15.3
- [x] NotFound names the store key (#109) — upstream wording + details, 2026-10-08;
      built 77d1b47 (609 passed, #238 my one-line miss), golden-rustkube-e069ee5958f3;
      notfound-message 5/5 (four test machines, conform.g8.lo; efbea2d 0/5);
      ten subresource/verb 404s probed, none names a key. #109 closed 2026-10-29
- [x] Unserved resources answer an empty list (#110) — 404 since 2026-10-07
- [x] LIST items carry resourceVersion and continuation pages request the
      first page's revision (64963b2, #111). The still-open issue label does
      not mean this code is absent; fastetcd snapshot correctness is #50 there.
- [x] RBAC escalation prevention (#98) — 2026-10-06; Namespace writes stay
      cluster-scoped (#97)
- [x] Secrets: `stringData` folded into `data` (#101) — 2026-10-07 (rig waits)
- [x] PVC `status.phase` not defaulted to `Pending` on create (#102) — Pods,
      PVCs and PVs get `Pending` on create (#67)
- [x] No generic ephemeral-volume controller (#94) — `ephemeral.rs`, 2026-10-08
- [x] #35 closed as superseded — **owner, 2026-09-28: "tests per component,
      and only system burn or stress tests of the system in the qa."**
      rustkube's tests live in its own `test/` container, never stormcos_qa
- [ ] Test containers per the stormcos test standard (#96) — 2026-11-19: stormcentral#512 closed. Live short 034b3dd344 / e038b12606 (pvetest2) 4/5: SCC `restricted-v2` (#321) refused the suites' pods (pinned runAsUser 65532 outside the namespace range); d1113e3 leaves the UID to SCC. At d1113e3: short af0d4ad73e 5/5 (pvetest2), medium 5f6401cc18 18+1 skip (pvetest1), long f17b3ce04a errored at the node API-health gate before the suite ran (2026-11-20: stormcluster#56 h2 ALPN on :9102, stormlb#30 metrics probe on 9111 vs listen 9104; not rustkube). Long requeued as 3a8c6238bc (pvetest1, 3fb4272) errored at the same gate (2026-11-20; stormcluster#123 = #56, 12.97 predates f1de3f6); item parked --after stormlb#30 --after-install stormcluster>=f1de3f6@any:pve, then rerun long. 2026-11-26: both cleared (stormlb#30 closed; passed pve installs since), but `test run rustkube long` is refused on all three: pvetest1/pvetest3 14.08 installs failed (no known release), pvetest2 sbregistry :5100 failing. Held --after-install stormcos>=f7f7ccb@any:pve, then long on that machine. 2026-11-26 17:35 CST: pvetest2 takes runs again; long e383b1ecdd queued there at main 9b1d9c0, item held --after-run e383b1ecdd; close if it passes. Controller events dropping the refusal reason filed #340. Before: BLOCKED on stormcentral#512 (2026-11-06: stormcos#271 fixed; short run eb5b18ef34 on pvetest2 fetched the image from forge, then ImagePullBackOff — the node's registry still `building` it when the Job started; proposed after #512). Before that BLOCKED on stormcos#271 (2026-10-29: `test run rustkube short` refused on pvetest1/pvetest2, sbregistry has no forge peer; C2NR0Q2 is a blade, evening window, and hardware waits for a pve pass, #791). The same image's `rigs` suite ran as a pod on C2NR0Q2/pvetest2 (10-13…10-21), so delivery and /test work; short/medium/long have no live result yet. Earlier: BLOCKED on stormcentral#512 (2026-10-07); IN PROGRESS
      2026-09-29: sc-build at a4dba9c compiled rustkube-test and passed all
      9 unit tests (remote exit 0; local build-log append read-only). Real
      short suite at main@d7bc1f8 was refused with HTTP 400 before any Job:
      C2NR0Q2's apiserver unavailable after three errored runs. It is the only
      registered test host; last install 11.52 failed. Blocked on
      stormcentral#63; retry the real suite after API recovery. No live pass
      and no run ID. Prior work, 2026-09-28: `test/` crate `rustkube-test` (workspace member, excluded
      from default-members), `/test short|medium|long`, JSON lines, exit
      0/1/2. Runs as stormcentral's `storm-test` SA: `*` in its namespace
      only, no cluster reads. [x] short, medium, long written (test/README.md)
      [x] test/build.sh stages a static-pie musl binary on dev
      [x] test/e2e/test-container.sh: short 5/5 on a real control plane
      [x] found + fixed: `?dryRun=All` delete deleted; workload controllers
      re-created children of an owner being deleted
      [x] rig run at 3fe5869: short 5/5, medium 14 + 1 skip (the 4
      kubelet-only checks fail on the rig, as expected), long 2 waves pass
      [ ] a real `stormcentral test run rustkube short` — 2026-10-07 run
      cf99131f71 (C2NR0Q2, 566403b): image built and pushed (the dev build
      step, stormcentral#526, is past), then "the registry did not list it"
      within 10 min — every test machine (stormcentral#512). Earlier: refused
      2026-09-28: C2NR0Q2's apiserver not answering, install 11.52 failed; the e2e scripts
      in `test/e2e/` (lib.sh + projects, status-rv, watch-deleted,
      vm-runstrategy) are the
      start of it

### Controller-manager drop-in parity (#2) — acceptance environment needed, 2026-09-29
- [x] sc-build at 5687d0f compiled kube-controller-manager and passed 58 unit
      tests plus doc tests (remote exit 0; local build-log append read-only).
      This baseline run does not verify upstream acceptance.
- [x] Read #2 and totrust#4; audit CLI, controller runner and HPA. TLS/token,
      Leases, metrics/health and workload controllers exist. Kubeconfig and
      signal handling are absent; controller presence does not establish parity.
- [x] Read totrust/PINS.yaml: the shared acceptance baseline is v1.31.4.
      This is distinct from rustkube's advertised 1.36 API posture; pin changes
      belong in totrust and must not be silently chosen in this repository.
- [ ] Owner identifies an isolated otherwise-upstream cluster with real
      kubelets and a deployment route for swapping only controller-manager.
- [ ] Complete kubeconfig, shutdown, ServiceAccount/token lifecycle,
      ResourceQuota (#124), real HPA (#89) and the controller parity review.
      Coordinate indexed workers and recovery safety with #146.
- [ ] Run all-upstream baseline and Rust-CM-only comparison: Deployment ->
      ReplicaSet -> Pods, scale up/down, owner-reference deletion and failover.
      Keep #2 open until implementation and upstream acceptance pass.

### Certificate lifecycle (#20) — scope decision needed, 2026-09-29
- [x] sc-build at 6f7e318: 3 cert-helper and 197 apiserver tests passed;
      the controller-manager csr:: filter matched no tests. Remote exit 0,
      local build-log append read-only. No renewal/rollover acceptance run.
- [x] Read #20 and inspect TLS reload, renewal tooling and current stormcert
      integration. Expiry metrics and serving reload exist; #93 still permits
      mismatched serving pairs. Client identities/trust reload remain #105.
- [x] Identify existing renewal owner: stormcert-agent has a renewal loop;
      stormcos#119 tracks wiring it into the golden. Do not duplicate it here.
- [ ] Owner reconciles #20's operator/CA-rotation roadmap with stormcert
      ownership and the ten-year CA decision recorded in stormcos#119.
      Canonical CA selection also remains open in stormcert#49.
- [ ] Once scope is settled, finish the rustkube reload obligations (#93/#105)
      and coordinate external delivery/renewal through their owning issues.
- [ ] Verify renewal without authentication loss, malformed/mismatched pair
      retention and the selected trust/key rollover contract before closure.
      cert-manager compatibility is optional and has not been implemented.

### Multi-arch image placement (#8) — acceptance target needed, 2026-09-29
- [x] Baseline sc-build at 89f36a8: apiserver 197 and scheduler 57 unit tests
      passed, doc tests passed, remote exit 0. Local build-log append was
      read-only. No image-admission or mixed-hardware acceptance was run.
- [x] Read #8; inspect built-in admission and scheduler affinity/gate handling.
      The scheduler has an arch affinity unit test. rustkube-node registration
      code sets arch/os labels; deployment on mixed hardware is not verified.
- [ ] Owner identifies an isolated mixed amd64/arm64 acceptance cluster and
      the supported deployment route for the changed control plane.
- [ ] Implement safe scheduling-gate handling (#87) before enabling gated
      placement; webhook-based admission additionally requires #82.
- [ ] Implement image resolution (indexes and single-image config), private
      pull-secret authentication, bounded credential-scoped cache, intersection
      across workload images, exclusions and affinity injection that preserves
      existing constraints. Failed inspection must not silently release a gate.
- [ ] Verify registry failure/recovery, conflicting image architectures, init
      containers, existing affinity, private secrets and excluded namespaces.
- [ ] Run real mixed-architecture placement acceptance; keep #8 open until
      admission, enforcement and runtime results are all verified.

### Scheduler drop-in parity (#3) — needs owner baseline, 2026-09-29
- [x] Baseline at 58ea6d2: sc-build compiled kube-scheduler and passed all
      57 scheduler unit tests plus doc tests (remote exit 0). Local build-log
      append was read-only. This does not verify upstream parity.
- [x] Read #3 and audit scheduler CLI, scheduling loop, score functions and
      unused plugin traits against the recorded scope.
- [x] Follow-up during #2: totrust/PINS.yaml supplies the shared acceptance
      baseline v1.31.4. README posture and research versions do not override it.
- [ ] Owner identifies an isolated otherwise-upstream acceptance cluster with
      real kubelets and a deployment route; shared pin changes belong in totrust.
- [ ] Implement kubeconfig/configuration profiles, framework/default scoring
      parity, priority/backoff/unschedulable queues and nomination, preemption
      (#84), scheduling gates (#87), and Pod scheduling events/status (#138).
      Coordinate indexed scheduling/reservations with #145/#146.
- [ ] Build/test on dev via sc-build, then compare placement against upstream
      on identical inputs, including infeasible and failure cases. Unit tests
      and the rustkube synthetic rig cannot close upstream acceptance.
- [ ] Keep #3 open until implementation and upstream acceptance are verified.

### Turbomode indexed workers (#146) — dev acceptance complete
Owner #163 authorizes integration into main; no goldens. Design: docs/event-driven-design.md;
handoff: docs/turbomode-handoff.md.

Resume checkpoint 2026-09-29: pushed fc67179 passed the handoff five-crate
`sc-build` command (exit 0; four storage integration tests ignored because
they require a running datastore). The branch already
contains the routed dependency runner and DaemonSet migration. Master update
on #146 authorizes remaining implementation now; live-target selection is
not a blocker for this work. Live validation belongs to #147/#149.
Implementation and dev acceptance are complete against fastetcd v1.6.1.
The follow-up verification above supersedes the former fastetcd#50 blocker.
Continue live acceptance in #147/#149; do not claim those gates passed.
- [x] origin/main merged into turbomode (de8c912); handoff step 2 on dev:
      `cargo test --locked` for the five crates passes (87/197/56/57, storage 4 ignored)
- [x] `owned::run` takes extra dependency feeds with routers (Node → DaemonSet,
      Pod labels → Service/PDB, PVC/PV → binder…), children optional
- [x] DaemonSet (Node eligibility + owned Pods), implemented at 826a8a4;
      API-rig node regression passed 10/10 at 72908ec
- [x] Service/EndpointSlice, PDB: selector-indexed Pod membership, UID-safe
      endpoint cleanup, CAS/no-op writes; real selector rig passed 14/14 at
      eff0752 after repairing UID-less bootstrap endpoints.
- [x] Baseline 79c1a9e: handoff five-crate command passed on dev (four
      datastore-dependent storage tests ignored).
- [x] PV binder: serialized indexed claim workers and per-PV lifecycle;
      only 404 proves claim absence; real CSI expansion passed 14/14 at 72908ec
- [x] Stormblock claim and reclaim workers; node lifecycle indexed by Lease
      name and assigned Pod, preserving expiry/toleration deadlines
- [x] Attach/detach: per-PV workers with indexed claim/Pod/driver/attachment
      dependencies and conditional detach; unit tests pass
- [x] VM (owned VMI), CSR, root CA (ConfigMap → Namespace); VM rig 10/10 at eff0752
- [x] Migration and HPA object workers with named Pod/Node/target routes;
      HPA remains the #89 placeholder, status timestamp echoes suppressed
- [x] Namespace: separate provision/teardown object pools, discovered shared
      feeds, conditional deletes and authoritative finalization confirmation
- [x] Gateway/HTTPRoute named-reference workers; stable condition timestamps
- [x] Events: per-event TTL deadlines and conditional deletion
- [x] GC: indexed resource workers fail closed on any unsynced feed; confirm
      owner absence and finalizer-dependent membership with authoritative reads
      and apply destructive UID/revision preconditions. All three propagation
      modes, Event expiry and namespace finalizers pass at 226a388
- [x] Scheduler: one serialized Pod/VMI queue, indexed storage reads, shared
      acknowledged-write accounting and retained bind/volume assumptions;
      optional VMI feed enabled by CRD observation. Shared accounting unit
      tests and burst/capacity-release API-rig checks pass at 226a388.
- [x] Five-crate final implementation tests at 9a24ce9: 409 passed, four
      datastore integration tests ignored. API safety/CSI/DaemonSet/VM checks
      pass at 1b6f951. Thirty fresh selector repetitions and ten short-suite
      repetitions pass; retain original intermittent failures #153/#154.
- [x] Audit create expectations, cache recovery, delayed acknowledgement
      history, destructive preconditions and shared resource reservations.
- [x] Isolate an upstream consistency violation: 981dcdb's
      `test/e2e/list-snapshot-race.sh` has no controllers/scheduler and detects
      165 inconsistent LIST snapshots out of 255, against 400 accepted creates.
- [x] fastetcd#50 fixed by its owning project in v1.6.1; rustkube's pinned
      snapshot/pagination/WATCH probe verifies that contract at 52188fb.
- [x] Rechecked units, startup membership and full API-rig case matrix;
      investigated #154 POST path and twenty fresh short runs. #153/#154
      remain historical incidents with no proven individual root cause.
- [x] #154 (one 30 s POST timeout in the short rig, 09-29) closed 2026-10-07
      as not reproducible: 31+ clean reruns; fastetcd#71 (the plausible
      cause) fixed in v1.9.0, rigs on v1.12.0; failures now dump diagnostics
- [x] Earlier rig failures #151 (bootstrap UID) and #152 (port/binary setup)
      fixed and closed with successful sc-build evidence. #155 records the
      original snapshot failure; post-fix acceptance evidence is recorded above.

### VM runStrategy on a failed VMI (#104) — COMPLETE 2026-09-28
- [x] Failed VMI recreated for `Always`/`RerunOnFailure`/`running: true`
      (Succeeded too for `Always`), with backoff in `status.startFailure`;
      `Once`/`Manual` leave it
- [x] printableStatus `CrashLoopBackOff` (backing off) / `Failed` (left
      failed); VMI `status.message` on a VM `Failure` condition
- [x] A refused VM status write is logged, not swallowed
- [x] `runStrategy: Once` starts the VM (it read as `spec.running`)
- [x] `test/e2e/vm-runstrategy.sh` on dev: 10/10 (v0.16.0)

### Presentation (#81) — COMPLETE 2026-09-26
- [x] `docs/presentation.md`, 12-slide Marp deck from the code as of v0.15.2;
      renders with `npx @marp-team/marp-cli docs/presentation.md` (checked on
      dev). Update it when a slide's claim changes, like any other doc

### Docs from the code (#80) — COMPLETE 2026-09-26
- [x] README, docs/, CLAUDE.md and module docs rewritten from the code
      (2026-09-24), re-checked against v0.15.2: every flag/env/default,
      ports, the 19 controllers, crate docs; stormcos/stormcert
      cross-references checked against their code
- [x] storage.md intro and CSI `CreateVolume` corrected (#103, #95)

### Watch DELETED events (#100) — COMPLETE 2026-09-26
- [x] Tombstone namespace from the key's real shape (CR keys have a group segment)
- [x] DELETED carries the object's last state from the watch cache's snapshot;
      selectors filter deletions; name-only tombstone only below the cache window
- [x] A watch from revision 0 ("from now") is served by the cache, not the store
- [x] `test/e2e/watch-deleted.sh` on dev

### Storage — current through v0.18.0
- [x] PV/PVC binding, protection finalizers, phases, reclaim, events (#56)
- [x] Attach/detach — `VolumeAttachment` for drivers that require it
- [x] Volume-aware scheduling — PV `nodeAffinity`, `selected-node`,
      `CSIStorageCapacity`
- [x] Volume expansion (#63) — COMPLETE 2026-09-28 (v0.18.0). PVC update
      validation + PersistentVolumeClaimResize, binder capacity only on
      becoming Bound, strategic-merge directives. `test/e2e/volume-expansion.sh`
      (hostpath CSI + external-provisioner v6.3.0 + external-resizer v2.2.0)
      14/14; the pre-#63 code fails 4. Node half: rustkube-node#42
- [x] Snapshots (#64) — COMPLETE 2026-09-28. Install is stormcos#170
      (moved from stormpump#28, open); CreateSnapshot maps onto CoW
      snapshots (stormblock#111, closed). `test/e2e/snapshot-controller.sh`:
      upstream v8.6.0 CRDs + the real snapshot-controller as its SA, 18/18.
      Fixed on the way: RoleBinding SA subject namespace, pre-bound claim
      binding its PV, protobuf inline embeds + webhook `Webhooks` field
- [x] `ReadWriteOncePod` enforcement — scheduler + admission (#65); the
      kubelet's mount refusal is rustkube-node#42
- [x] In-kubelet `stormblock` class: `stormblock.rs` writes the PV once the
      scheduler picks a node (#71, v0.13.0–v0.14.1)
- [x] `stormblock.rs` checks the class's provisioner (#92, 2026-10-08):
      `stormblock.storm.io` (or `/in-kubelet`); StorageClass feed wakes claims.
      Built fe60e97 (608 passed); golden-rustkube-d638c9a6eb55.
      stormblock-class 5/5 (four test machines, conform.g8.lo); efbea2d's
      controller-manager provisions a CSI-provisioner `stormblock` class.
      #92 closed 2026-10-29
- See [docs/storage.md](docs/storage.md) for the contract with stormblock,
  sbregistry and stormblock-csi. rustkube creates no volume bytes itself.

### Custom resource keyspace (#76) — COMPLETE 2026-09-22
- [x] #74 verified on dev (apply-created CRD serves its CRs without restart); closed
- [x] CR keys are `/registry/{group}/{plural}/...` (upstream layout); the CRD
      objects themselves and all built-ins keep their keys
- [x] Boot-time migration of old `/registry/{plural}/...` CR keys, split by the
      object's `apiVersion` group — idempotent, HA-safe, uid-checked
- [x] `handlers/kubevirt.rs` (VM/VMI) and `manifests.rs` key CRs the same way
- [x] A CRD group without a dot is refused (422) and not served
- [x] e2e on dev: seeded with the pre-#76 binary, booted the new one on the
      same fastetcd store — colliding plurals separated, uids kept, second
      boot moved nothing, same name in two groups coexists, VM `start` works

### Node ssh login (#79, part of stormcos#60)
- [x] Bootstrap `kube-system/node-admin` ServiceAccount + `node-admin`
      ClusterRoleBinding → `cluster-admin`
- [x] RS256 token signed offline with the SA signing key authenticates as
      `system:serviceaccount:kube-system:node-admin`; SA groups derived from
      `sub`, not the token. Claim contract in docs/certificates.md
- The token itself is stormcert's (stormcert#5)

### Scale measurement (#66) — awaiting execution decision, 2026-09-29
- [x] Five-crate `sc-build` at f5d3876: 400 unit tests passed, four storage
      integration tests ignored, doc tests passed; no scale workload run.
      Remote exit 0; local runs.jsonl append reported a read-only filesystem.
- [x] Read issue history: the old 250-node curve used truncated controller
      LISTs; pagination was fixed in v0.12.0, but the full-read curve is absent.
- [x] Document the 10/100/1000-node protocol in docs/scale.md, keeping fake
      Nodes alive with Leases and checking the full Pod population.
- [ ] Owner selects an isolated test environment (#162). Delivery is
      decided: goldens (#140), persistent tests as pods on forge (#173);
      neither implemented. Do not launch a stress workload before that.
- [ ] Run and publish CPU/request/latency/datastore curves and Deployment
      scale-to-Pod-creation latency; keep #66 open until measurements exist.

### Phase 4: Scale & Conformance

The dated entries below preserve the historical investigation; temporary logs
and staged artifacts are not promised to exist today. Current procedure and
results are in docs/conformance.md.

- [ ] 1000+ node testing (#66) — indexed workers now exist on turbomode,
      but their full-population resource/latency and failover curves are unverified
- [x] K8s conformance test suite (#67) — closed 2026-09-28; first run on dev,
      `test/conformance/run.sh`: e2e.test v1.36.x `[Conformance]` (443) against
      apiserver + controller-manager + scheduler with two heartbeat-kept
      stand-in Nodes and no kubelet; pod-dependent tests are expected to
      fail as "needs a node". Triage every failure into: missing /
      wrong / needs a real node / not applicable → issues. A full-cluster run
      needs stormcos test machines (rustkube-node#27, #32).
  - **State 2026-09-27 (session restarted mid-run):** sig-api-machinery ran
    on dev (e2e v1.36.5): 14 passed, 76 failed. Triage so far, not yet filed:
    flowcontrol (APF) not served (2); Validating/MutatingAdmissionPolicy not
    served, 3 specs hang to the suite timeout (6); aggregated discovery (#107,
    4); CRD schemas not published to /openapi (8); CR defaulting, CR
    fieldValidation unknown/duplicate, YAML request bodies refused ("Expected
    request with Content-Type: application/json") (5); CRD deletecollection
    405; CRD /status spec mismatch; autoscaling/v1 not served (Discovery
    spec); resourcequotas/podtemplates/replicationcontrollers missing from
    `resource_to_list_kind` + not in discovery (#110; ResourceQuota 10 specs,
    chunking 2); no ResourceQuota or ReplicationController controller;
    SelfSubjectAccessReview ignores Accept: Table (should 406); watch with a
    selector sends no DELETED when an object stops matching; webhook/
    conversion/aggregator specs need a pod (+ #82, #83). **Open question:**
    webhook specs fail with `resource "deployments" not found` — **found**:
    protobuf creates stored `metadata.namespace: ""`, so client-go's next GET
    had no namespace in its path and hit the CRD catch-all; fixed 2026-09-27.
  - **2026-09-27:** fixed from the api-machinery triage: empty namespace on
    protobuf create, selector watch DELETED/ADDED, deletecollection. Filed:
    APF #118, admission policies #119, CRD OpenAPI #120, CR schema #121,
    YAML/fieldValidation #122, autoscaling/v1 #123, ResourceQuota #124,
    ReplicationController #125, Table 406 #126; list kinds on #110. Not yet
    triaged: CRD /status spec mismatch (probe with RK_WHY_LINES=40),
    OrderedNamespaceDeletion, GC dependency circle — rerun first.
    Unit tests pass at 2e5dcb9 (after #129/#130, test-only). All six chunks
    + a CRD /status probe ran at 2e5dcb9 (`tmp/chunks.sh`, logs
    `tmp/conf-*.log`; the pre-fix api-machinery log is
    `tmp/conf-apimachinery-5ebe056.log`). Summaries at 2e5dcb9:
    api-machinery 15/90, misc 14/34, network 6/47, node 11/103, storage
    6/91 passed; apps still running.
  - **Fixed from that run (after 2e5dcb9, unverified by a rerun yet):** CRD
    schema x-kubernetes-* over protobuf; LIST item RVs + pinned paging +
    remainingItemCount (#111); DELETE-to-terminating retries; generateName;
    events.k8s.io protobuf + deletecollection; SA token volume injection;
    impersonation; watch without RV sends current state; EndpointSlice
    managed-by + prompt endpoints cleanup; ConfigMap/Secret key + sysctl
    validation, qosClass; immutable ConfigMap/Secret; VolumeAttributesClass.
  - **Filed from it:** LimitRanger #131, NodePort/type-change #132,
    EndpointSliceMirroring #133, ServiceCIDR/IPAddress #134, RuntimeClass
    #135, pod resize #136, DRA #137.
  - **2026-09-27, owner's rule: conformance no longer runs in build slots.**
    The last in-slot run (six chunks at 430b268, logs `tmp/conf-*.log`;
    the 2e5dcb9 run is in `tmp/run-2e5dcb9/`) is allowed to finish; no new
    ones. From ab130be: `sc-build test/conformance/stage.sh` once per commit
    → /build/assets/conformance/<sha>, then the chunks run on conform.g8.lo
    with RK_BIN/RK_FASTETCD (docs/conformance.md). conform.g8.lo did not
    resolve yet on 2026-09-27.
  - **2026-09-28:** first VM run at efbea2d: 75 passed. The drop was the
    VM's missing kubectl (fixed d1881ac; chunk rerun 19 passed, best yet);
    GC orphan was load (4/4 focused); PriorityClass `value` immutability
    fixed (c241688). docs/conformance.md has the table. #67 closed: the run
    exists and every failure is filed. **Blocked for new runs:** stage.sh
    can't write /build/assets under the no-kept-state build rule (#140).
    Owner answered **Goldens**: binaries reach conform.g8.lo as the
    component's golden; run.sh/vm.sh not yet changed. efbea2d is still
    staged there.
- [ ] ARM64 verification (#68) — PARKED 2026-10-09 until the owner's ARM64
      host is set up. aarch64 musl builds through sc-build (8e82cca:
      16M/11M/8.7M, static, start under qemu, arm64 images). Owner
      2026-10-09: measure A first (the three binaries as built; B/C from the
      numbers); "Microtik is not compatible with what we are building … no
      mikrotik, and this issue parked till hardware available." Next: on the
      host, boot apiserver against fastetcd, record RSS and startup.
      2026-10-14: no ARM64 machine registered; `wait-owner --until` the
      host is set up (recommend A, approved). Came back the same day when
      needs-owner was removed (wait-owner labels it); re-parked, the loop
      filed as stormcentral#847. Came back again 10-14 (no ARM64 test host):
      proposed `--after stormcentral#847` instead of another wait-owner.
      2026-10-21: #847 closed; still no ARM64 test machine. aarch64 release
      build still passes at b52da76 (sc-build-d2fc674379). Parked with
      `wait-owner --until` as the master asked

### `oc` compatibility — the surface that drives completeness

**[docs/oc-compatibility.md](docs/oc-compatibility.md)** is the reference: what
`oc` can ask a cluster to do, and therefore what this apiserver has to answer
for. It is the specification for "complete", not a wish list — a verb that is
not answered is a gap whether or not anyone has hit it yet.

Work it as a checklist against a live cluster rather than by reading: `oc
<verb> --help` on the target is authoritative, and the runbook's Part II is a
verification pass that should collapse into one script with an exit code.

Known state on 2026-09-24:

- [x] `oc logs` — apiserver `pods/log` proxying to the kubelet's
      `/containerLogs`, with TokenReview so anything can authenticate to a
      kubelet at all (#54). `container`, `tailLines`, `previous` work;
      `timestamps`/`sinceSeconds`/`sinceTime` are inert because stormpump logs
      are raw by design. `follow` streams end to end (rustkube-node#34).
      `limitBytes` is honored on the node. A named container may be an init or
      ephemeral one (#55).
- [ ] `oc exec`, `attach`, `port-forward` — and therefore `rsh`, `cp`, `rsync`
      and `debug`. **The apiserver half is done** (#42): proxied to the kubelet
      as a transparent connection upgrade, so SPDY and WebSocket both pass
      through; the query's `stdin/stdout/stderr` become the kubelet's
      `input/output/error`. **The kubelet half does not exist**: rustkube-node
      serves no `/exec`, `/attach` or `/portForward` (rustkube-node#56).
- [x] `oc adm` — every verb run against a live apiserver
      (`test/e2e/oc-adm.sh`); the checklist is in docs/oc-compatibility.md
      (#69). Open from it: OpenShift authorization reviews for `who-can` and
      `adm new-project` (#106, served 2026-10-07), aggregated discovery for `inspect` (#107, served 2026-10-07),
      `nodes/proxy` for `node-logs` (#108, served 2026-10-08; kubelet `/logs/` is
      rustkube-node#198); `top` needs #83
- [x] `oc scale` — `/scale` served (#86), rig `test/e2e/scale.sh`.
- [x] Projects (#97): `project.openshift.io/v1` Project + ProjectRequest over
      Namespaces, owned by their requester (`admin` RoleBinding), listed only
      to members; `admin`/`edit`/`view`/`basic-user`/`self-provisioner`
      bootstrapped. Verified by `test/e2e/projects.sh` (oc 4.22, two users).
      Namespace *writes* stay cluster-scoped until RBAC escalation
      prevention exists (#98).
- [ ] Routes, DeploymentConfig, ImageStream, BuildConfig, SCC — the
      genuinely OpenShift-only half. Whether these are in scope at all is a
      decision nobody has made (#70), and `route.openshift.io/v1` is already
      *served* with nothing routing for it, which is the inconsistency that
      forces the question; `oc` without them is `kubectl` with better
      ergonomics, which may be the right target.

