#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
NAME="$(sed -n 's/^ *"id": "\(.*\)".*/\1/p' pack.json | head -1)"
ZIP="${ROOT}/dist/${NAME}-${VERSION}.zip"
export AGENT2API_HOST="${AGENT2API_HOST:-http://127.0.0.1:3065}"
export HALO_CODEX_ZIP="$ZIP"

if [[ ! -f "$ZIP" ]]; then
  ./scripts/pack.sh
fi

python3 - <<'PY'
import base64, json, os, pathlib, urllib.parse, urllib.request, time

host = os.environ["AGENT2API_HOST"].rstrip("/")
zip_path = pathlib.Path(os.environ["HALO_CODEX_ZIP"])
key = os.environ.get("AGENT2API_API_KEY")
cfg = pathlib.Path.home() / ".agent2api" / "config.json"
if not key and cfg.exists():
    data = json.loads(cfg.read_text())
    key = data.get("apiKey") or data.get("api_key")
if not key:
    raise SystemExit("set AGENT2API_API_KEY or ~/.agent2api/config.json apiKey")

headers = {"Authorization": f"Bearer {key}", "Content-Type": "application/json"}
b64 = base64.b64encode(zip_path.read_bytes()).decode()

def post(url, body, timeout=120):
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as response:
        return json.load(response)

preview = post(f"{host}/api/plugins/install?preview=true", {"packB64": b64})
pack = (preview.get("data") or preview)["preview"]
print("preview", pack["id"], pack["version"])
qs = urllib.parse.urlencode({
    "background": "true",
    "expected_id": pack["id"],
    "expected_version": pack["version"],
    "expected_sha256": pack["sha256"],
})
post(f"{host}/api/plugins/install?{qs}", {"packB64": b64})
for _ in range(40):
    req = urllib.request.Request(f"{host}/api/plugins", headers=headers)
    with urllib.request.urlopen(req, timeout=30) as response:
        data = json.load(response)
    packs = ((data.get("data") or data).get("packs") or [])
    current = next(item for item in packs if item["id"] == pack["id"])
    phase = current.get("startup", {}).get("phase")
    print(f"v={current.get('version')} phase={phase}")
    if current.get("version") == pack["version"] and phase == "ready":
        break
    time.sleep(0.4)
else:
    raise SystemExit("install did not become ready")
print("installed", pack["id"], pack["version"])
PY
