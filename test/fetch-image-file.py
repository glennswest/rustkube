#!/usr/bin/env python3
"""Copy one file out of a container image, over the registry's HTTP API (#173).

    test/fetch-image-file.py <registry/repo:tag> <path in image> <out>

The e2e rigs that run upstream binaries (the CSI hostpath driver and
sidecars, the snapshot-controller) used to `podman pull` their images at run
time. Neither the build box nor a test pod has podman (stormcentral#121), and
a test pod should not reach the internet, so test/build.sh fetches the files
here, at image-build time: the linux/amd64 manifest (from an index if the tag
is one), then the layers top-down until one holds the path. Anonymous pulls
only; a registry that asks for a bearer token gets one from its realm.
Standard library only.
"""
import io, json, os, sys, tarfile, urllib.parse, urllib.request

ACCEPT = ", ".join([
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
])


class Registry:
    def __init__(self, host, repo):
        self.base, self.repo, self.token = f"https://{host}/v2/{repo}", repo, None

    def get(self, path, accept=ACCEPT):
        for attempt in range(2):
            req = urllib.request.Request(self.base + path, headers={"Accept": accept})
            if self.token:
                req.add_header("Authorization", "Bearer " + self.token)
            try:
                with urllib.request.urlopen(req, timeout=120) as r:
                    return r.read()
            except urllib.error.HTTPError as e:
                if e.code != 401 or attempt:
                    raise
                self.authenticate(e.headers.get("WWW-Authenticate", ""))

    def authenticate(self, challenge):
        # Bearer realm="…",service="…",scope="…"
        fields = dict(
            p.strip().split("=", 1) for p in challenge.removeprefix("Bearer ").split(",") if "=" in p
        )
        fields = {k: v.strip('"') for k, v in fields.items()}
        realm = fields.pop("realm")
        fields.setdefault("scope", f"repository:{self.repo}:pull")
        with urllib.request.urlopen(realm + "?" + urllib.parse.urlencode(fields), timeout=60) as r:
            body = json.load(r)
        self.token = body.get("token") or body.get("access_token")


def main(ref, path, out):
    host, rest = ref.split("/", 1)
    repo, tag = rest.rsplit(":", 1)
    reg = Registry(host, repo)
    manifest = json.loads(reg.get(f"/manifests/{tag}"))
    if "manifests" in manifest:
        pick = [m for m in manifest["manifests"]
                if m.get("platform", {}).get("os") == "linux"
                and m.get("platform", {}).get("architecture") == "amd64"]
        if not pick:
            sys.exit(f"{ref}: no linux/amd64 image")
        manifest = json.loads(reg.get(f"/manifests/{pick[0]['digest']}"))
    want = path.lstrip("/")
    whiteout = "/".join(want.split("/")[:-1] + [".wh." + want.split("/")[-1]]).lstrip("/")
    for layer in reversed(manifest["layers"]):
        blob = reg.get(f"/blobs/{layer['digest']}", accept="*/*")
        with tarfile.open(fileobj=io.BytesIO(blob), mode="r:*") as t:
            names = {m.name.lstrip("./"): m for m in t.getmembers()}
            if whiteout in names:
                break
            m = names.get(want)
            if m is None:
                continue
            while m.issym() or m.islnk():
                target = m.linkname if m.islnk() else "/".join(want.split("/")[:-1] + [m.linkname])
                m = names.get(target.lstrip("./"))
                if m is None:
                    sys.exit(f"{ref}: {path} links outside its layer")
            data = t.extractfile(m).read()
        with open(out, "wb") as f:
            f.write(data)
        os.chmod(out, 0o755)
        print(f"{ref}:{path} -> {out} ({len(data)} bytes)")
        return
    sys.exit(f"{ref}: {path} not found in any layer")


if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    main(*sys.argv[1:])
