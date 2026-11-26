# Halo Codex

Agent2API WASM 插件仓库：https://github.com/postbiancheng/halo-codex

用 ChatGPT / Codex 订阅账号转发对话、查询用量与生成图片。  
Provider id：`halo-codex`。许可证：AGPL-3.0-only。

## 能力

| 能力 | 说明 |
|---|---|
| 登录 | 设备授权 OAuth，或导入 access token / 完整 `auth.json`（含 refresh） |
| 模型 | 登录后 `GET /codex/models` 拉取；失败回退内置默认 |
| 对话 | 默认 WebSocket；可切 HTTP SSE |
| 用量 | 订阅窗口（`/wham/usage` + `/subscriptions`）；额外点数可在窗口用尽后兜底 |
| 图片 | `gpt-image-2` / `gpt-image-1.5` → `/codex/images/generations\|edits` |
| 设置 | 转发方式、clientVersion、originator、默认推理强度 |
| 错误 | classify / retry.decision：鉴权失效、额度/限流、内容策略 |

## 构建

```bash
./scripts/pack.sh
./scripts/checksum.sh   # 生成 dist/*.zip.sha256
```

产物：`dist/agent2api-wasm-halo-codex-<version>.zip`（含 `assets/logo.svg` 图标）

## 安装

在 Agent2API 中安装上述 zip，或对本机已运行的实例：

```bash
export AGENT2API_API_KEY=...   # 或写入 ~/.agent2api/config.json 的 apiKey
./scripts/install-local.sh
```

## 工程化

- 本地：`cargo test`、`cargo fmt --check`、`cargo clippy --target wasm32-unknown-unknown -- -D warnings`、`./scripts/pack.sh`
- GitHub Actions 草稿在本地 `.github/workflows/`（`ci.yml` / `release.yml`）；推送需 token 带 `workflow` scope
- 发版：改 `Cargo.toml` + `pack.json` 版本 → push → `git tag vX.Y.Z && git push origin vX.Y.Z`

```bash
gh auth refresh -h github.com -s workflow
git add .github/workflows && git commit -m "Add CI and release workflows." && git push
```

## 版本摘要

- **0.1.5–0.1.10**：远程模型目录、转发方式、clientVersion、订阅到期映射
- **0.1.11**：originator / 默认推理强度
- **0.1.12**：错误分类与重试（上游强制 `store=false`，暂无 previous_response_id 会话链）
- **0.1.13**：图片生成/编辑
- **0.2.0**：更名 Halo Codex（provider `halo-codex`），接入 OpenAI 风格 SVG 图标
- **0.2.1**：宿主控制面补齐（`is_error_response` / `refusal_markers` / `quota.sources`）、真实 `reasoning_patch`、`error_rules`
- **0.2.2**：用量改为仅订阅窗口（忽略额外点数）；工具支持 function / custom / web_search / namespace 与原生 `input`
- **0.2.3**：加密推理 `include`、web_search 自动声明、拒绝无状态历史字段、过长 call_id 哈希（本地已备 CI workflow，推送需 `workflow` 授权）
- **0.2.4**：目录补齐 serviceTiers/图片能力；私有 profile 回退 base_instructions 与 verbosity；HTTP `responses=experimental`；订阅配额错误码对齐
- **0.2.5**：恢复额外点数兜底；`prompt_cache_key` 最长 512；空 `tool_choice` 不再下发
- **0.2.6**：图片/对话冷却隔离；账号显示 JWT 邮箱；统一 `User-Agent`；有 tools 时默认 `tool_choice=auto`；授权失败带上游错误正文
- **0.2.7**：导入完整 `auth.json`/refresh；LICENSE + CI/Release/checksum/本地安装脚本
