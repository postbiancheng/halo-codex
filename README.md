# Halo Codex

Agent2API WASM 插件仓库：https://github.com/postbiancheng/halo-codex

用 ChatGPT / Codex 订阅账号转发对话、查询用量与生成图片。  
Provider id：`halo-codex`。许可证：AGPL-3.0-only。

## 能力

| 能力 | 说明 |
|---|---|
| 登录 | 设备授权 OAuth，或导入 access token / 完整 `auth.json`（含 refresh） |
| 续期 | 有 `refreshToken` 时自动续期（到期前约 5 分钟） |
| 模型 | 登录后 `GET /codex/models` 拉取；失败回退内置默认 |
| 对话 | 默认 WebSocket；可切 HTTP SSE |
| 用量 | 订阅窗口（`/wham/usage` + `/subscriptions`）；额外点数可在窗口用尽后兜底 |
| 图片 | `gpt-image-2` / `gpt-image-1.5` → `/codex/images/generations\|edits` |
| 设置 | 转发方式、clientVersion、originator、默认推理强度 |
| 错误 | classify / retry.decision：鉴权失效、额度/限流、内容策略 |

## 构建 / 安装 / 冒烟

```bash
./scripts/pack.sh
./scripts/checksum.sh
export AGENT2API_API_KEY=...   # 或 ~/.agent2api/config.json 的 apiKey
./scripts/install-local.sh
./scripts/smoke-local.sh       # 真实账号：模型刷新 + 用量 + 短对话 + gpt-6-luna ≥40s 长测
```

产物：`dist/agent2api-wasm-halo-codex-<version>.zip`

**提交 / 推送 / 打 tag / Release 前必须先通过 `smoke-local.sh`。** 仅单元测试或插件 `ready` 不算过关。长测时长可用 `HALO_CODEX_LUNA_LONG_SECONDS` 覆盖（默认 40）。

## 工程化

- 本地：`cargo test`、`cargo fmt --check`、`cargo clippy --target wasm32-unknown-unknown -- -D warnings`、`./scripts/smoke-local.sh`
- CI：push / PR 跑 format、clippy、test、pack（`.github/workflows/ci.yml`）
- 发版：冒烟通过后，对齐 `Cargo.toml` 与 `pack.json` 版本再：

```bash
git tag v0.2.7 && git push origin v0.2.7
```

`release.yml` 会上传 zip 与 `.sha256`。

## 版本摘要

- **0.2.7**：导入完整 `auth.json`/refresh；LICENSE + CI/Release/checksum/本地安装脚本
- **0.2.6**：图片/对话冷却隔离；JWT 邮箱账号名；统一 `User-Agent`；有 tools 时默认 `tool_choice=auto`
- **0.2.5**：恢复额外点数兜底；`prompt_cache_key` 最长 512
- **0.2.0–0.2.4**：更名 Halo Codex、宿主控制面、工具/推理/图片与订阅错误对齐
- **0.1.x**：远程模型目录、WebSocket/HTTP、用量与图片初版
