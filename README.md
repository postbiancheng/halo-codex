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
export AGENT2API_API_KEY=...   # 也可使用本机已配置的 Agent2API API key
./scripts/install-local.sh
./scripts/smoke-local.sh       # 真实账号：模型刷新 + 用量 + 短对话 + gpt-6-luna ≥40s 长测
```

产物为 `dist/agent2api-wasm-halo-codex-<version>.zip` 和同名 `.sha256` 文件。也可在 [GitHub Releases](https://github.com/postbiancheng/halo-codex/releases) 下载后，通过 Agent2API 插件页面安装 ZIP。

**提交 / 推送 / 打 tag / Release 前必须先通过 `smoke-local.sh`。** 仅单元测试或插件 `ready` 不算过关。长测时长可用 `HALO_CODEX_LUNA_LONG_SECONDS` 覆盖（默认 40）。

## 工程化

- 本地使用 rustup。项目、CI 和打包共用 `rust-toolchain.toml` 中固定的 Rust 版本、工具和 WASM target。进入仓库后运行 `rustup toolchain install --no-self-update` 可安装这一套工具。macOS 若另有 Homebrew Rust，将 `~/.cargo/bin` 放在 Homebrew 的 bin 之前。
- 本地检查使用 `cargo fmt --all -- --check`、`cargo test --locked --all-targets`、本机和 WASM Clippy，以及 `./scripts/smoke-local.sh`。
- 推送 `main` 和提交 PR 会运行构建检查。发版前对齐 `Cargo.toml`、`Cargo.lock` 和 `pack.json` 的版本，确认候选包冒烟通过，再推送对应标签。

```bash
git tag v0.2.10
git push origin v0.2.10
```

发行工作流会先检查标签对应的提交，再上传 ZIP 与 `.sha256`。

## 版本摘要

- **0.2.10**：统一 Rust 1.100.0 和锁定构建；覆盖乱序工具事件、结束帧、凭据格式、模型上下文上限、空额度响应和图片地址校验。
- **0.2.9**：收紧冒烟检查；修复流式错误、提前续期、原生 Responses 参数和图片错误状态；替换停止维护的时间依赖
- **0.2.8**：修复嵌套 JWT `auth`/`profile` 解析；账号展示邮箱；冒烟用 Codex UA 便于客户端识别
- **0.2.7**：导入完整 `auth.json`/refresh；LICENSE + CI/Release/checksum/本地安装脚本
- **0.2.6**：图片/对话冷却隔离；JWT 邮箱账号名；统一 `User-Agent`；有 tools 时默认 `tool_choice=auto`
- **0.2.5**：恢复额外点数兜底；`prompt_cache_key` 最长 512
- **0.2.0–0.2.4**：更名 Halo Codex、宿主控制面、工具/推理/图片与订阅错误对齐
- **0.1.x**：远程模型目录、WebSocket/HTTP、用量与图片初版
