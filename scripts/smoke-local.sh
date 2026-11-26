#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
export HALO_CODEX_EXPECTED_VERSION="${HALO_CODEX_EXPECTED_VERSION:-$VERSION}"
export AGENT2API_HOST="${AGENT2API_HOST:-http://127.0.0.1:3065}"

python3 - <<'PY'
import json, os, pathlib, sys, time, urllib.error, urllib.request

HOST = os.environ["AGENT2API_HOST"].rstrip("/")
EXPECT = os.environ["HALO_CODEX_EXPECTED_VERSION"]
PROVIDER = "halo-codex"
PACK = "agent2api-wasm-halo-codex"

key = os.environ.get("AGENT2API_API_KEY")
cfg = pathlib.Path.home() / ".agent2api" / "config.json"
if not key and cfg.exists():
    data = json.loads(cfg.read_text())
    key = data.get("apiKey") or data.get("api_key")
if not key:
    creds = pathlib.Path.home() / ".dsh" / ".credentials.yaml"
    if creds.exists():
        try:
            import yaml  # type: ignore

            refs = (yaml.safe_load(creds.read_text()) or {}).get("refs") or {}
            key = refs.get("AGENT2API_API_KEY")
        except Exception:
            key = None
if not key:
    print(
        "FAIL: set AGENT2API_API_KEY, ~/.agent2api/config.json apiKey, or .dsh credentials",
        file=sys.stderr,
    )
    raise SystemExit(2)

H = {"Authorization": f"Bearer {key}", "Content-Type": "application/json"}


def call(method, path, body=None, timeout=120, stream=False):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"{HOST}{path}", data=data, headers=H, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            if stream:
                chunks = []
                while True:
                    line = resp.readline()
                    if not line:
                        break
                    chunks.append(line.decode("utf-8", "replace"))
                    if len(chunks) >= 80:
                        break
                return resp.status, "".join(chunks)
            raw = resp.read().decode()
            return resp.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as err:
        raw = err.read().decode("utf-8", "replace")
        try:
            payload = json.loads(raw) if raw else raw
        except json.JSONDecodeError:
            payload = raw
        return err.code, payload


def fail(msg):
    print(f"FAIL: {msg}", file=sys.stderr)
    raise SystemExit(1)


def ok(msg):
    print(f"OK: {msg}")


status, plugins = call("GET", "/api/plugins")
if status != 200:
    fail(f"plugins HTTP {status}: {plugins}")
packs = ((plugins.get("data") or plugins).get("packs") or [])
pack = next((item for item in packs if item.get("id") == PACK), None)
if not pack:
    fail(f"pack {PACK} not installed")
phase = (pack.get("startup") or {}).get("phase")
if pack.get("version") != EXPECT or phase != "ready":
    fail(f"pack version/phase want {EXPECT}/ready got {pack.get('version')}/{phase}")
ok(f"plugin {EXPECT} ready")

status, models = call("GET", "/v1/models")
if status != 200:
    fail(f"models HTTP {status}: {models}")
rows = (models.get("data") or []) if isinstance(models, dict) else []
owned = [row for row in rows if row.get("owned_by") == PROVIDER]
chat_models = [
    row["id"]
    for row in owned
    if row.get("id") and row.get("kind", "chat") != "image"
]
if not chat_models:
    fail("no halo-codex chat models in /v1/models")
ok(f"/v1/models has {len(owned)} halo-codex models; chat={chat_models[0]}")

status, refresh = call("POST", "/api/models/refresh", {"provider": PROVIDER, "force": True})
if status != 200:
    fail(f"models.refresh HTTP {status}: {refresh}")
ok(f"models.refresh => {json.dumps(refresh, ensure_ascii=False)[:180]}")

status, accounts = call("GET", "/api/accounts?includeHidden=true")
if status != 200:
    fail(f"accounts HTTP {status}: {accounts}")
account = next(
    (
        item
        for item in ((accounts.get("data") or accounts).get("accounts") or [])
        if item.get("provider") == PROVIDER and item.get("enabled")
    ),
    None,
)
if not account:
    fail("no enabled halo-codex account")
account_id = account["id"]
ok(f"account {account_id} enabled canRefresh={account.get('canRefresh')}")

status, usage_start = call(
    "POST",
    "/api/accounts/usage/start",
    {"accountIds": [account_id]},
)
if status != 200:
    fail(f"usage.start HTTP {status}: {usage_start}")

usage_ok = False
last = None
for _ in range(40):
    status, snap = call("GET", "/api/accounts/usage/snapshot")
    if status != 200:
        time.sleep(0.5)
        continue
    data = snap.get("data") or snap
    results = data.get("results") or []
    if isinstance(results, dict):
        row = results.get(account_id) or {}
    else:
        row = next((item for item in results if item.get("id") == account_id), {}) or {}
    usage = row.get("usage") if isinstance(row.get("usage"), dict) else row
    last = usage if isinstance(usage, dict) else row
    if row and not row.get("error") and isinstance(last, dict) and (
        last.get("windows")
        or last.get("credits")
        or last.get("subscription")
        or last.get("quotaSources")
        or last.get("plan")
        or last.get("available") is not None
    ):
        usage_ok = True
        break
    queries = data.get("queries") or []
    active = next((q for q in queries if q.get("id") == account_id), None)
    if active and active.get("error"):
        fail(f"usage query error: {active.get('error')}")
    if active and active.get("status") in {"queued", "running", None}:
        time.sleep(0.5)
        continue
    time.sleep(0.5)
if not usage_ok:
    fail(f"usage snapshot empty/failed: {json.dumps(last, ensure_ascii=False)[:300]}")
ok(
    f"usage available={last.get('available')} windows={len(last.get('windows') or [])} "
    f"credits={bool(last.get('credits'))}"
)

model = chat_models[0]
status, stream = call(
    "POST",
    "/v1/chat/completions",
    {
        "model": model,
        "stream": True,
        "messages": [{"role": "user", "content": "只回：ready"}],
        "max_tokens": 16,
    },
    timeout=180,
    stream=True,
)
if status != 200:
    fail(f"chat HTTP {status}: {stream}")
if "data: " not in stream:
    fail(f"chat stream empty: {stream[:400]}")
if '"content"' not in stream and "content" not in stream:
    # still accept finish-only if model replied via delta elsewhere
    if "error" in stream.lower() and "ready" not in stream.lower():
        fail(f"chat stream looks like error: {stream[:500]}")
ok(f"chat stream on {model} ({len(stream)} chars)")
print("SMOKE PASS")
PY
