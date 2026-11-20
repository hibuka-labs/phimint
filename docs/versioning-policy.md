# 版本策略（phimint 0.x → 1.x）

> **什么时候读：** 想知道 phimint 的版本号意味着什么、什么时候会到 1.0、升级会不会出事。
>
> 一句话：**0.x 是「能用的开发版」，1.0 是「承诺不乱改的稳定版」。**

## 1. 两条正交的轴，别混

版本策略里最容易混的是「发布通道」和「API 成熟度」。它们是两件事：

| | 0.12.0 | 1.0.0 |
|---|---|---|
| **API 成熟度**（版本号本身表达） | 不承诺稳定，公开 API 可能变 | 承诺稳定，1.x 内只做向后兼容的改动 |
| **发布通道**（`manifest.channel` 表达） | `stable` | `stable` |

**`stable` 说的是「这个版本能不能放心装」，不是「API 稳不稳」。**

一个 `0.x` 的版本完全可以是 `stable` 通道的——它指这个版本本身可靠（测试过、发布资产齐全、能自更新），
不代表它对库使用者承诺了 API 稳定性。反过来 `1.0.0-beta.1` 也不是「更不稳定」，它是「1.0 的预演版」。

看正文的 [§4](#4-发版通道机制) 讲通道的具体实现。

## 2. 0.x 阶段：承诺什么，不承诺什么

**承诺：**

- 每个发布的版本都能装、能跑、能自更新——`stable` 通道的语义就是这个
- `crates.io` / `npm` / `brew` 上的版本号单调递增，不复用、不撤回（yank 除外，且会公开说明）
- 破坏性变更会写进 CHANGELOG 的 `Dependencies` / `Changed` 段，并在对外稿件里单列兼容性说明

**不承诺：**

- **公开 API 的向后兼容。** `0.x` 阶段任何一次 minor 升级都可能改公开 API。
  对 `phimint` 来说，这意味着：如果你把它当库用（`use phimint::...`），
  锁 `Cargo.toml` 里的精确版本，别用 `^0.12.0` 那种会跟着 minor 跳的约束。

  ```toml
  # 库使用者：锁死
  phimint = "=0.12.0"
  # 应用使用者：跟随 minor 升级没问题（CLI 契约比 API 契约稳得多）
  phimint = "0.12.0"
  ```

- CLI 参数与配置文件的长期稳定。命令行契约的变更会写进 CHANGELOG，但 0.x 阶段不承诺不删。

## 3. 1.0.0 的判据

到 1.0.0 时，下面这几条要同时成立：

| 判据 | 现状 | 说明 |
|---|---|---|
| 公开 API 面已冻结 | 部分 | `src/lib.rs` 导出 20 个 module。要逐个过一遍，标出哪些是公开 API、哪些是内部实现 |
| 依赖链语义化版本 | ✅ | 家族 crate 已按 SemVer 发版，0.x 期的 breaking 抬版会连锁（见下） |
| `mandatory` 强制升级通道可用 | ⚠️ 只解析未消费 | 1.0 之后需要能推「必须升级」的版本（比如协议不兼容），这个开关现在还没有行为 |
| 稳定通道的客户端行为已验证 | ⚠️ 部分 | `Manifest.channel` 目前解析了但不消费；`beta` 通道还没有独立的 manifest 端点 |
| 至少一个完整 minor 周期无 API 破坏 | ❌ | 还需要时间 |

**依赖链的连锁反应**：phimint 依赖的 phi 家族 crate（`phi-agent`、`phi-kernel-tools`、`agent-base` 等）
在 0.x 阶段也可能 breaking。一旦它们到 1.0，phimint 的依赖声明会从 `version = "0.24.0"` 变成 `version = "1.0.0"`，
这是**依赖声明的变化，不是 phimint 自身的破坏性变更**——对使用者无感。

## 4. 发版通道机制

manifest（`manifest.json` / `manifest-gitee.json`）带两个扩展字段：

```json
{
  "version": "0.12.0",
  "channel": "stable",
  "mandatory": false,
  "platforms": { ... }
}
```

| 字段 | 取值 | 现在的状态 |
|---|---|---|
| `channel` | `"stable"` / `"beta"` | 会被解析，但**没有代码读它做决策**——预留字段 |
| `mandatory` | `true` / `false` | 会被解析，但**CLI 没有消费它**——预留字段 |

**当前只有一条通道在用**：stable。客户端只取一个 manifest URL。

**将来要发 `1.0.0-beta.1` 时，需要补的**：

1. 独立的 beta manifest 端点（`manifest-beta.json`），stable manifest 继续指向最新的正式版——
   否则 stable 用户会被提示去装 beta
2. `channel` 字段要有消费方（比如 CLI 显示当前通道，或按通道选源）
3. npm 用 `npm publish --tag next`，**`latest` 必须永远指向最新的 stable**
4. GitHub Release 勾上 prerelease 标记
5. 版本比较要区分通道——已在 `src/update/checker.rs` 处理：prerelease 用户转正时会被提示，
   但**跨 major 的降级不会**（`1.0.0-beta.1` 不会被提示「升级到 0.12.0」）

## 5. 什么时候升级

| 你是 | 建议 |
|---|---|
| 只想用 phimint 这个终端 agent | 直接跟最新 stable。CLI 契约够稳，0.x 的 breaking 主要是库 API 层面 |
| 把 phimint 当库用（`use phimint::...`） | 锁精确版本 `=0.12.0`，等 1.0 再放宽 |
| 想要 pre-release | 目前没有 beta 通道。发 1.0.0-beta.1 时会在 CHANGELOG 和对外稿里说明 |

## 6. 相关文档

- [CHANGELOG.md](../CHANGELOG.md) —— 每个版本的破坏性变更
- [SECURITY.md](../SECURITY.md) —— 安全问题报告渠道
- `notes/verify-gate-design.md` —— 验证门禁的关闭原因与恢复判据（内部文档，不进版本库）
