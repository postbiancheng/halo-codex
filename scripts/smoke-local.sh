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

# Match Agent2API builtin Codex client catalog (product token in UA).
CLIENT_UA = os.environ.get(
    "HALO_CODEX_SMOKE_UA",
    "codex_cli_rs/0.160.1 (Halo-Codex-smoke; darwin)",
)
H = {
    "Authorization": f"Bearer {key}",
    "Content-Type": "application/json",
    "User-Agent": CLIENT_UA,
}


def call(method, path, body=None, timeout=120, stream=False, headers=None):
    data = None if body is None else json.dumps(body).encode()
    req_headers = dict(H)
    if headers:
        req_headers.update(headers)
    req = urllib.request.Request(
        f"{HOST}{path}", data=data, headers=req_headers, method=method
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            if stream:
                chunks = []
                while True:
                    line = resp.readline()
                    if not line:
                        break
                    text = line.decode("utf-8", "replace")
                    chunks.append(text)
                    if text.strip() == "data: [DONE]":
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

# Rewrite public name/email from nested JWT claims (fixes stale "Halo Codex" labels).
status, refreshed = call("POST", "/api/accounts/refresh", {"id": account_id}, timeout=90)
if status != 200:
    fail(f"account.refresh HTTP {status}: {refreshed}")
status, accounts = call("GET", "/api/accounts?includeHidden=true")
if status != 200:
    fail(f"accounts recheck HTTP {status}: {accounts}")
account = next(
    (
        item
        for item in ((accounts.get("data") or accounts).get("accounts") or [])
        if item.get("id") == account_id
    ),
    None,
)
if not account:
    fail("halo-codex account missing after refresh")
account_name = (
    account.get("displayName")
    or account.get("name")
    or account.get("email")
    or account.get("loginName")
    or ""
)
if account_name.strip() in {"", "Halo Codex"} or "@" not in account_name:
    fail(f"account display name still generic after refresh: {account_name!r}")
ok(f"account label {account_name}")

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


def extract_assistant_text(raw: str) -> str:
    parts = []
    for line in raw.splitlines():
        line = line.strip()
        if not line.startswith("data:"):
            continue
        payload = line[5:].strip()
        if not payload or payload == "[DONE]":
            continue
        try:
            event = json.loads(payload)
        except json.JSONDecodeError:
            continue
        for choice in event.get("choices") or []:
            delta = choice.get("delta") or {}
            content = delta.get("content")
            if isinstance(content, str) and content:
                parts.append(content)
            message = choice.get("message") or {}
            content = message.get("content")
            if isinstance(content, str) and content:
                parts.append(content)
    return "".join(parts)


luna = next((item for item in chat_models if item == "gpt-6-luna"), None)
luna = luna or next((item for item in chat_models if "luna" in item.lower()), None)
if not luna:
    fail("gpt-6-luna (or any luna chat model) missing from catalog")

long_seconds = float(os.environ.get("HALO_CODEX_LUNA_LONG_SECONDS", "40"))
long_timeout = max(180, int(long_seconds) + 120)
started = time.time()
messages = [
    {
        "role": "user",
        "content": (
            "请用中文详细讲解：什么是冒烟测试、它和回归测试的区别、"
            "以及在 Agent2API WASM 插件发版流程里为什么要做。请分点展开，"
            "每点至少写一段，尽量写充分，不要提前结束。"
        ),
    }
]
rounds = 0
chars = 0
while True:
    rounds += 1
    status, stream = call(
        "POST",
        "/v1/chat/completions",
        {
            "model": luna,
            "stream": True,
            "messages": messages,
            "max_tokens": 1200,
        },
        timeout=long_timeout,
        stream=True,
    )
    if status != 200:
        fail(f"luna long chat HTTP {status}: {stream}")
    if "data: " not in stream:
        fail(f"luna long chat empty stream: {stream[:400]}")
    if "error" in stream.lower() and '"content"' not in stream:
        fail(f"luna long chat error stream: {stream[:500]}")
    text = extract_assistant_text(stream)
    chars += len(text)
    elapsed = time.time() - started
    if elapsed >= long_seconds:
        break
    if rounds >= 8:
        fail(
            f"luna long chat only reached {elapsed:.1f}s after {rounds} rounds "
            f"(need >={long_seconds:.0f}s)"
        )
    messages.append({"role": "assistant", "content": text or "（上轮无文本，请继续）"})
    messages.append(
        {
            "role": "user",
            "content": "请继续展开下一点，再写一段更细的说明，不要重复前面内容。",
        }
    )

elapsed = time.time() - started
if elapsed < long_seconds:
    fail(f"luna long chat too short: {elapsed:.1f}s < {long_seconds:.0f}s")
ok(f"luna long chat on {luna}: {elapsed:.1f}s rounds={rounds} chars={chars}")

status, stats = call("GET", "/api/stats/requests?limit=30")
if status != 200:
    fail(f"stats.requests HTTP {status}: {stats}")
entries = ((stats.get("data") or stats).get("entries") or [])
recent = next(
    (
        item
        for item in entries
        if item.get("provider") == PROVIDER and item.get("accountId") == account_id
    ),
    None,
)
if not recent:
    fail("no recent halo-codex request in /api/stats/requests")
provider_label = recent.get("providerLabel") or ""
client = (recent.get("client") or "").lower()
client_label = recent.get("clientLabel") or ""
ua = ((recent.get("clientEvidence") or {}).get("userAgent") or "")
logged_account = recent.get("accountName") or ""
if provider_label != "Halo Codex":
    fail(f"providerLabel want Halo Codex got {provider_label!r}")
if logged_account.strip() in {"", "Halo Codex"} or "@" not in logged_account:
    fail(f"request accountName still generic: {logged_account!r}")
if client not in {"codex"} and "codex" not in client_label.lower():
    fail(
        f"client not matched as Codex: client={client!r} label={client_label!r} ua={ua!r}"
    )
if "codex_cli_rs" not in ua:
    fail(f"request UA missing codex_cli_rs: {ua!r}")
ok(
    f"request identity provider={provider_label} account={logged_account} "
    f"client={client_label or client}"
)
print("SMOKE PASS")
PY
