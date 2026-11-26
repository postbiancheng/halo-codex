# Halo Codex（Agent2API 插件）

独立仓库：https://github.com/postbiancheng/halo-codex  

Provider id 暂为 `codex-websocket`（兼容已装账号）。用 ChatGPT / Codex 订阅账号转发对话、查询用量与图片。

前身：https://github.com/postbiancheng/codex-websocket

## 能力

| 能力 | 说明 |
|---|---|
| 登录 | 设备授权 OAuth，或导入 access token |
| 模型 | 登录后 `GET /codex/models` 拉取；失败回退内置默认 |
| 对话 | 默认 WebSocket；可切 HTTP SSE |
| 用量 | `/wham/usage` + `/subscriptions`（含套餐到期） |
| 图片 | `gpt-image-2` / `gpt-image-1.5` → `/codex/images/generations|edits` |
| 设置 | 转发方式、clientVersion、originator、默认推理强度 |
| 错误 | classify / retry.decision：鉴权失效、额度/限流、内容策略 |

## 构建

```bash
./scripts/pack.sh
```

产物：`dist/agent2api-wasm-codex-websocket-<version>.zip`

## 安装

在 Agent2API 中安装上述 zip，或对本机已运行的实例：

```bash
# 示例：通过本地 API 安装（需 AGENT2API_API_KEY）
python3 - <<'PY'
import base64, json, urllib.request, os
key = os.environ["AGENT2API_API_KEY"]
zip_path = "dist/agent2api-wasm-codex-websocket-0.1.13.zip"
body = json.dumps({"packB64": base64.b64encode(open(zip_path, "rb").read()).decode()}).encode()
req = urllib.request.Request(
    "http://127.0.0.1:3065/api/plugins/install",
    data=body,
    headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    method="POST",
)
print(urllib.request.urlopen(req, timeout=60).read()[:200])
PY
```

## 阶段进度

- **0.1.5–0.1.10**：远程模型目录、转发方式、clientVersion、订阅到期映射（`credits` + `subscription.expireAt`）
- **0.1.11**：originator / 默认推理强度设置；收尾清理旧包与旧安装版本
- **0.1.12**：错误分类与重试策略（`classify` / `retry.decision`）。上游 Codex 接口强制 `store=false`，暂无法做 `previous_response_id` 会话链
- **0.1.13**：图片生成/编辑（`gpt-image-2` / `gpt-image-1.5`）。上下文压缩需宿主 compact 钩子，暂未接
