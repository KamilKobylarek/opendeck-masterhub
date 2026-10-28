#!/usr/bin/env python3
"""Gitea release steps for .gitea/workflows/release.yml (Python standard library only).

  release.py upload <file>   add <file> to the release of the pushed tag, replacing an asset of the same name;
                             the release is created as a pre-release, so it isn't "latest" while incomplete
  release.py universal       merge the platform zips into opendeck-masterhub.zip, check every platform the
                             release builds is in it, upload it and make the release a normal one

Environment (set by Gitea Actions): GITHUB_SERVER_URL, GITHUB_REPOSITORY, GITHUB_REF_NAME, and RELEASE_TOKEN (a
Gitea access token with repository write access). The token only ever goes into a request header, never onto a
command line.
"""

import io
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zipfile

PLUGIN = "pl.kobylarek.masterhub.sdPlugin"
UNIVERSAL = "opendeck-masterhub.zip"
# what the release jobs build; the universal zip must carry all of them
TRIPLES = ["aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
           "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"]
TAG = re.compile(r"^v\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$")


def env(name):
    value = os.environ.get(name)
    if not value:
        sys.exit(f"{name} is not set")
    return value


TAG_NAME = env("GITHUB_REF_NAME")
if not TAG.match(TAG_NAME):
    sys.exit(f"tag {TAG_NAME!r} is not a version like v1.2.3")
API = f"{env('GITHUB_SERVER_URL')}/api/v1/repos/{env('GITHUB_REPOSITORY')}"
AUTH = {"Authorization": f"token {env('RELEASE_TOKEN')}"}


def call(method, url, body=None, headers=None):
    request = urllib.request.Request(url, data=body, method=method, headers={**AUTH, **(headers or {})})
    with urllib.request.urlopen(request, timeout=300) as response:
        data = response.read()
    return json.loads(data) if data and response.headers.get_content_type() == "application/json" else data


def release():
    """The tag's release, created as a pre-release the first time (another job may create it meanwhile)."""
    by_tag = f"{API}/releases/tags/{urllib.parse.quote(TAG_NAME)}"
    try:
        return call("GET", by_tag)
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
    try:
        body = json.dumps({"tag_name": TAG_NAME, "name": TAG_NAME, "prerelease": True}).encode()
        return call("POST", f"{API}/releases", body, {"Content-Type": "application/json"})
    except urllib.error.HTTPError:
        return call("GET", by_tag)


def upload(path):
    rel = release()
    name = os.path.basename(path)
    for asset in rel.get("assets", []):
        if asset["name"] == name:  # a re-run: replace, don't add a second one
            call("DELETE", f"{API}/releases/{rel['id']}/assets/{asset['id']}")
    boundary = uuid.uuid4().hex
    with open(path, "rb") as f:
        content = f.read()
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"{name}\"\r\n"
            f"Content-Type: application/zip\r\n\r\n").encode() + content + f"\r\n--{boundary}--\r\n".encode()
    call("POST", f"{API}/releases/{rel['id']}/assets?name={urllib.parse.quote(name)}", body,
         {"Content-Type": f"multipart/form-data; boundary={boundary}"})
    print(f"uploaded {name} to release {TAG_NAME}")
    return rel


def universal():
    rel = release()
    parts = [a for a in rel.get("assets", []) if a["name"].startswith("opendeck-masterhub-") and a["name"].endswith(".zip")]
    merged = io.BytesIO()
    found = set()
    with zipfile.ZipFile(merged, "w", zipfile.ZIP_DEFLATED) as out:
        seen = set()
        for asset in parts:
            data = call("GET", f"{API}/releases/{rel['id']}/assets/{asset['id']}")
            request = urllib.request.Request(data["browser_download_url"], headers=AUTH)
            with urllib.request.urlopen(request, timeout=300) as response:
                part = zipfile.ZipFile(io.BytesIO(response.read()))
            for info in part.infolist():
                name = info.filename.replace("\\", "/")
                if not name.startswith(PLUGIN + "/") or ".." in name.split("/"):
                    sys.exit(f"{asset['name']}: unexpected path {name!r}")
                bits = name.split("/")
                if len(bits) > 3 and bits[1] == "bin":
                    found.add(bits[2])
                if name in seen:
                    continue  # the files every platform shares (manifest, icons, pages)
                seen.add(name)
                info.filename = name
                out.writestr(info, part.read(info))
    missing = [t for t in TRIPLES if t not in found]
    if missing:
        sys.exit(f"not every platform built, missing {', '.join(missing)}: no {UNIVERSAL} for {TAG_NAME}")
    os.makedirs("dist", exist_ok=True)
    path = os.path.join("dist", UNIVERSAL)
    with open(path, "wb") as f:
        f.write(merged.getvalue())
    rel = upload(path)
    body = json.dumps({"prerelease": False}).encode()
    call("PATCH", f"{API}/releases/{rel['id']}", body, {"Content-Type": "application/json"})
    print(f"release {TAG_NAME} complete: {', '.join(sorted(found))}")


if __name__ == "__main__":
    if sys.argv[1:2] == ["upload"] and len(sys.argv) == 3:
        upload(sys.argv[2])
    elif sys.argv[1:] == ["universal"]:
        universal()
    else:
        sys.exit(__doc__)
