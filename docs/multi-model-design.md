# §N 多模型策略（Multi-Model Routing）

> 状态：设计中（2026-08-18）
> 关联：`agent-base/src/llm/openai.rs`（`with_model()`）、`phi-agent/src/config/llm.rs`（`resolve_llm_config`）

## 1. 问题

phimint 当前**一个模型贯穿全部**：主循环、decompose 嵌套调用、子 agent 调查、上下文压缩——全用同一个 `OpenAiClient`（由 `LLM_MODEL` 决定）。

这意味着：
- 子 agent 做只读搜索+汇报，用的模型和主循环推理一样贵
- decompose 做一个 JSON 分类任务（串行/并行 + 切片），用的模型和写代码一样贵
- 上下文压缩做摘要，用的模型和架构决策一样贵
- 用户想用 Opus 处理复杂任务时，所有子任务也跟着用 Opus，成本爆炸

**核心矛盾**：编码 agent 的任务链路中，推理密度差异极大，但模型能力没有跟着分层。

## 2. 目标

1. **降本**：低推理密度的任务用小模型，省 token（子 agent、decompose、压缩）
2. **提质**：复杂任务能用强模型，不被默认模型拖累
3. **可控**：用户能显式选择模型，也能依赖合理默认值
4. **最小侵入**：改动集中在 phimint 层，phi-agent 框架尽量不改

## 3. 模型分层定义

| 层级 | 代号 | 定位 | 典型模型 |
|------|------|------|----------|
| **strong** | 强模型 | 深度推理、架构决策（自动升级目标） | claude-opus-4 / deepseek-v4-pro |
| **default** | 默认模型 | 主循环常规编码、decompose 切片质量 | claude-sonnet-4 / deepseek-chat |
| **fast** | 快模型 | 子 agent 调查、压缩摘要、简单分类 | claude-haiku-4 / deepseek-lite |

### 3.1 各组件的模型分配

| 组件 | 当前 | 改后 | 自动升级？ | 理由 |
|------|------|------|-----------|------|
| 主循环（react loop） | default | **default** | ✅ 触发条件见 §3.4 | 常规任务 Sonnet 够用；复杂任务自动升 strong |
| decompose 嵌套调用 | default | **default** | ✅ 随主循环升级 | 切片质量影响下游所有子 agent，不能太弱 |
| 子 agent（`spawn_agent`） | default | **fast** | ❌ 固定 | 只读调查：搜+读+汇报，推理需求低 |
| 上下文压缩（`SummarizingMiddleware`） | default | **fast** | ❌ 固定 | 纯摘要任务 |
| merge（合并验证） | default | **default** | ✅ 随主循环升级 | 需要理解多份报告 + 判断冲突，推理密度中等 |

### 3.2 为什么 decompose 不用 fast

decompose 的输入是**完整任务描述 + 仓库结构**，输出是**切片策略 + 每个子任务的上下文**。切错了，子 agent 全白跑。这是一个需要理解全局的推理任务，不能用太弱的模型。

如果未来发现 decompose 的质量瓶颈不在模型而在 prompt，可以再考虑降级。

### 3.3 为什么主循环默认不用 strong

- 大部分编码任务（改个函数、加个接口、修个 bug）Sonnet 够用
- Opus 比 Sonnet 慢 2-3 倍，常规任务用 Opus 反而体验差
- 通过自动升级机制（§3.4），复杂任务不需要用户手动干预

### 3.4 自动升级：何时切换到 strong 模型

> ⚠️ 本节是核心设计，仍在讨论中。分类体系、触发逻辑、路由规则均待斟酌。

#### 3.4.1 核心问题

默认模型（Sonnet）处理大部分编码任务够用。但**设计型任务**（架构重构、系统设计）需要 strong 模型（Opus）的深度推理能力。

问题：**怎么判断一个任务是"设计型"？**

#### 3.4.2 两条实现路径

> ⚠️ 二选一，待讨论。推荐路径 B。

**路径 A：在已有工具里嵌入分类（改动小，但耦合）**

在 `decompose` 的输出里加 `task_type` 字段，在 `update_plan` 的参数里加 `task_type` 字段。middleware 读这些字段决定是否升级。

- 优点：改动最小（改 prompt + 加字段），不增加 LLM 调用
- 缺点：分类逻辑耦合在工具里；decompose 不被调用时没有分类；update_plan 的分类是 agent 顺带判断，准确性取决于当前模型

**路径 B：独立的分类 Focus（解耦，更灵活）** ⭐ 推荐方向

用 `agent-works` 的 `Focus` 抽象（详见下文 §3.4.3）做独立的任务分类器。分类和分解完全解耦，每个 Focus 独立选模型。

- 优点：分类器独立于任何工具；每个 Focus 独立选模型；可独立测试和优化 prompt；未来可扩展更多 Focus（风险评估等）
- 缺点：多一次 LLM 调用（Haiku，成本低）；需要决定调用时机

#### 3.4.3 Focus：分类的正确抽象（路径 B 的基础）

`agent-works` 已有 `Focus` 抽象（`agent-works/src/focus/`），设计哲学与我们的需求完全对齐：

```rust
/// A focused LLM call.
/// Each instance is bound to a system prompt and dedicated to one specific
/// judgment question.
pub struct Focus {
    client: Arc<dyn StreamClient>,
    system_prompt: String,
}

impl Focus {
    pub async fn ask<T: DeserializeOwned>(
        &self, input: &(impl FocusInput + ?Sized), timeout: Duration,
    ) -> Result<FocusOutput<T>, FocusError>
}
```

Focus 的设计哲学：

> Throwing all judgments at the LLM at once overwhelms it. Especially for generalized concerns, we need to decompose — the LLM should **focus on one thing at a time**, keeping it simple enough to handle. If the business decomposition is good, **even a weak model can do one thing well**.

**Focus 和多模型的天然契合**：每个 Focus 实例构造时绑定 `client`（即绑定模型），不同 Focus 可以用不同模型。不需要额外的路由机制——模型选择在 Focus 构造时就决定了。

用 Focus 做分类：

```rust
// 分类 Focus：用 Haiku，快且便宜
let classify_task = Focus::new(
    fast_client,
    "你是一个编码任务分类器。根据任务描述和仓库结构，判断任务类型：\n\
     - execution: 目标明确，按步骤做（加字段、修 bug、加测试）\n\
     - coordination: 跨多文件/多模块，需要保持一致性（统一格式、重命名接口）\n\
     - design: 需要先想清楚怎么做（架构重构、设计新系统、插件化）"
);
```

**decompose 本质上也是一个手写的 Focus**——它没用 `Focus` 这个抽象，但做的事情一样（专注的 LLM 调用 + 结构化输出）。如果选路径 B，decompose 也可以用 Focus 重构。

**路径 B 的调用时机**：

| 时机 | 优点 | 缺点 |
|------|------|------|
| decompose 调用前 | 和 decompose 联动，自然 | 不调 decompose 时没分类 |
| agent 首次思考时 | 所有任务都有分类 | 多一次调用，简单任务也要付成本 |
| update_plan 调用时 | 和 plan 联动 | 不调 plan 时没分类 |

> **待讨论（路径 B）**：
> 1. 分类 Focus 的调用时机？
> 2. 分类结果如何传递给 middleware？（ToolContext 共享状态？事件总线？）
> 3. 是否用 Focus 重构 decompose 本身？

#### 3.4.4 task_type 分类体系

> ⚠️ 以下分类是初稿，维度和边界都需要讨论。路径 A 和路径 B 共用同一套分类。

分类的关键维度不是"复杂不复杂"，而是**需要什么类型的推理能力**：

| 推理类型 | 特征 | 对模型的要求 |
|---------|------|------------|
| 模式匹配 | 照着已有模式做，机械性 | 低（Sonnet 甚至 Haiku） |
| 一致性维护 | 改多处，要保持互相一致 | 中（Sonnet） |
| 深度推理 | 架构决策、权衡取舍、抽象建模 | 高（Opus） |

**候选方案 A：三级分类**

```rust
enum TaskKind {
    /// 执行型：目标明确，按步骤做就行
    /// 例："给 User 加个 email 字段"、"修个 typo"、"加个单元测试"
    Execution,

    /// 协调型：跨多文件/多模块，需要保持一致性
    /// 例："统一所有 API 返回格式"、"重命名一个公共接口"
    Coordination,

    /// 设计型：需要先想清楚怎么做，再动手
    /// 例："把单体拆成微服务"、"设计插件系统"、"重构状态管理"
    Design,
}
```

**候选方案 B：两级分类（更简单，边界更清晰）**

```rust
enum TaskKind {
    /// 执行型：有明确目标，照着做
    Execution,

    /// 设计型：需要先想清楚怎么做
    Design,
}
```

**候选方案 C：加风险维度**

有些任务虽然不难，但做错了代价很高：

```rust
enum TaskKind { Execution, Coordination, Design }

enum RiskLevel {
    Low,    // 改内部工具函数，错了就错了
    High,   // 改核心路径 / 安全相关 / 公共接口
}
```

输出 `{ task_kind, risk_level }`，路由时组合判断。

> **待讨论**：
> 1. 分几级？两级简单但粗糙，三级精细但边界模糊
> 2. 要不要加风险维度？加了更准但分类更复杂
> 3. 分类的 prompt 怎么写才能让 LLM 稳定输出？
> 4. 路径 A 时，decompose 和 update_plan 用同一套分类还是各用各的？

#### 3.4.5 路由规则（基于 task_type，待定）

以方案 A（三级）为例：

| task_kind | 路由 | 理由 |
|-----------|------|------|
| Execution | 默认模型（Sonnet） | 模式匹配，Sonnet 够用 |
| Coordination | 默认模型（Sonnet） | 一致性维护，Sonnet 够用 |
| Design | 强模型（Opus） | 需要深度推理、架构权衡 |

以方案 C（三级 + 风险）为例：

| task_kind | risk_level | 路由 |
|-----------|-----------|------|
| Execution | Low | Sonnet |
| Execution | High | Sonnet（任务简单，风险靠 verify 兜底） |
| Coordination | Low | Sonnet |
| Coordination | High | Opus（多处一致性 + 高风险） |
| Design | 任意 | Opus |

> **待讨论**：
> 1. Coordination + High 需要 Opus 吗？还是 Sonnet + verify 够了？
> 2. 用户能不能覆盖路由规则？（比如"我这个 Design 任务用 Sonnet 就行"）
> 3. 路由规则应该硬编码还是可配置？

#### 3.4.6 边界情况

- **手动指定 `--model opus` 时**：跳过自动升级逻辑，用户显式指定优先
- **`LLM_STRONG_MODEL` 未配置时**：自动升级完全不生效，无论 task_type 是什么
- **分类失败时**（Focus 超时/解析错误）：回退到默认模型，不升级
- **分类误判时**：误判为 Execution（漏升级）→ verify 循环兜底；误判为 Design（多升级）→ 多花钱但质量不降

## 4. 配置方案

### 4.1 环境变量

```bash
# .env
LLM_API_KEY=sk-...
LLM_BASE_URL=https://api.anthropic.com/v1

# 主模型（default 层）—— 主循环、decompose、merge
LLM_MODEL=claude-sonnet-4

# 快模型（fast 层）—— 子 agent、压缩摘要
# 未设置时回退到 LLM_MODEL（行为与现在完全一致）
LLM_FAST_MODEL=claude-haiku-4

# 强模型（strong 层）—— 复杂任务自动升级（可选）
# 未设置时：不启用自动升级，行为与现在完全一致
LLM_STRONG_MODEL=claude-opus-4
```

### 4.2 CLI 参数

```bash
# 默认：主循环用 LLM_MODEL，子 agent/压缩用 LLM_FAST_MODEL，自动升级用 LLM_STRONG_MODEL
cargo run

# 显式指定主模型（覆盖 LLM_MODEL，跳过自动升级）
cargo run -- --model claude-opus-4

# 显式指定快模型（覆盖 LLM_FAST_MODEL）
cargo run -- --fast-model claude-haiku-4

# 显式指定强模型（覆盖 LLM_STRONG_MODEL）
cargo run -- --strong-model claude-opus-4

# 全部用同一个模型（等于现在的行为，兼容）
cargo run -- --model deepseek-chat --fast-model deepseek-chat --strong-model deepseek-chat
```

### 4.3 回退规则

```
fast_model   = --fast-model CLI
               ?? LLM_FAST_MODEL env
               ?? LLM_MODEL（回退到主模型，行为不变）

strong_model = --strong-model CLI
               ?? LLM_STRONG_MODEL env
               ?? 未设置 → 自动升级不启用
```

不设置 `LLM_FAST_MODEL` 时，phimint 的行为和现在**完全一致**——零破坏性变更。
不设置 `LLM_STRONG_MODEL` 时，自动升级逻辑不启用——零破坏性变更。

## 5. 实现方案

### 5.1 已有基础设施（不改框架）

**`OpenAiClient::with_model()`**（`agent-base/src/llm/openai.rs:74`）：

```rust
// 共享连接池，只换模型名。已有代码，直接用。
pub fn with_model(&self, model: &str) -> Self { ... }
```

**`LlmEngine::set_client()`**（`agent-base/src/engine/runtime/llm_engine.rs:33`）：

```rust
// 运行时换 client。已有代码。
pub fn set_client(&self, client: Arc<dyn StreamClient>) { ... }
```

**`Focus`**（`agent-works/src/focus/core.rs:118`）：

```rust
// 专注的 LLM 调用，绑定系统提示，返回结构化 JSON。
// 每个 Focus 实例可独立选择 client（即独立选择模型）。
pub struct Focus {
    client: Arc<dyn StreamClient>,
    system_prompt: String,
}
```

Focus 是 task_type 分类的最佳载体——构造时绑定 prompt + client，调用时只传输入，返回结构化结果。详见 §3.4.3。

### 5.2 改动点（4 处，phimint 内 + 框架最小改动）

#### 改动 1：`main.rs` — 构建 fast_client + strong_client

```rust
// 现状
let llm = resolve_llm_config(cli.model.as_deref(), cli.base_url.as_deref())?;
let llm_client = Arc::new(OpenAiClient::new(llm.api_key, llm.model, Some(llm.base_url)));

// 改后
let llm = resolve_llm_config(cli.model.as_deref(), cli.base_url.as_deref())?;
let llm_client = Arc::new(OpenAiClient::new(
    llm.api_key.clone(), llm.model.clone(), Some(llm.base_url.clone()),
));

// 快模型：CLI > env > 回退到主模型
let fast_model = cli.fast_model
    .or_else(|| std::env::var("LLM_FAST_MODEL").ok())
    .unwrap_or_else(|| llm.model.clone());
let fast_client: Arc<dyn StreamClient> = Arc::new(llm_client.with_model(&fast_model));

// 强模型：CLI > env > None（不启用自动升级）
let strong_client: Option<Arc<dyn StreamClient>> = cli.strong_model
    .or_else(|| std::env::var("LLM_STRONG_MODEL").ok())
    .map(|m| Arc::new(llm_client.with_model(&m)) as Arc<dyn StreamClient>);
```

Cli struct 加两个字段：

```rust
/// Fast model for sub-agents and summarization (overrides LLM_FAST_MODEL env)
#[arg(long)]
fast_model: Option<String>,

/// Strong model for auto-upgrade on complex tasks (overrides LLM_STRONG_MODEL env)
#[arg(long)]
strong_model: Option<String>,
```

#### 改动 2：`agent.rs` — 接收 fast_client + strong_client，分发给各组件

> 以下代码以路径 A（middleware 读工具输出的 task_type）为例。如果选路径 B（独立 Focus），middleware 的构造方式不同，但 `agent.rs` 的接口不变。

```rust
pub fn build(
    llm_client: Arc<OpenAiClient>,                   // 主模型
    fast_client: Arc<dyn StreamClient>,               // 快模型 ← 新增
    strong_client: Option<Arc<dyn StreamClient>>,     // 强模型 ← 新增
    approval: Arc<dyn ApprovalHandler>,
    policy: Option<Arc<dyn ToolPolicy>>,
    shell_timeout_ms: u64,
    workspace_root: PathBuf,
    writes_possible: bool,
) -> Result<(PhiAgent, SkillResolver)> {
    let llm: Arc<dyn StreamClient> = llm_client.clone();

    // ... builder 注册 ...

    // decompose 用主模型（切片质量重要）；自动升级后随主循环切到 strong
    builder = builder
        .register_tool(DecomposeTool::new(llm, tracker.clone(), workspace_root.clone()));

    // 子 agent 用快模型（只读调查，推理需求低）
    builder = builder.with_multi_agent(MultiAgentConfig {
        child_client: Some(fast_client.clone()),   // ← 新增字段（见改动 4）
        // ... 其余不变 ...
    });

    // 压缩摘要用快模型
    builder = builder.summarizer_client(fast_client);  // ← 新增方法（见改动 4）

    // 自动升级中间件（仅在 strong_client 配置时启用）
    if let Some(strong) = strong_client {
        builder = builder.middleware(ModelUpgradeMiddleware::new(strong));
    }

    // ...
}
```

#### 改动 3：task_type 分类 + 自动升级 middleware

> ⚠️ 两种路径二选一，待讨论。推荐路径 B（§3.4.2）。

**路径 A：decompose 输出带 task_type（改动小，但耦合）**

在 decompose 的 prompt 里加一句"同时判断任务类型"，输出 JSON 加一个 `task_type` 字段。middleware 读这个字段决定是否升级。

优点：改动最小（改 prompt + 加一个字段）
缺点：分类逻辑耦合在 decompose 里，decompose 不被调用时没有分类

路径 A 的 middleware：

```rust
// src/model_upgrade.rs — 路径 A 版本
impl Middleware for ModelUpgradeMiddleware {
    fn on_tool_result(&self, ctx: &ToolResultContext) -> MiddlewareAction {
        if self.upgraded.load(Ordering::Relaxed) {
            return MiddlewareAction::Continue;
        }
        let should_upgrade = match ctx.tool_name {
            "decompose" => {
                ctx.result.get("task_type")
                    .and_then(|v| v.as_str())
                    .map_or(false, |t| t == "design")
            }
            "update_plan" => {
                ctx.args.get("task_type")
                    .and_then(|v| v.as_str())
                    .map_or(false, |t| t == "design")
            }
            _ => false,
        };
        if should_upgrade {
            self.upgraded.store(true, Ordering::Relaxed);
            ctx.llm_engine.set_client(self.strong_client.clone());
            tracing::info!("auto-upgraded to strong model");
        }
        MiddlewareAction::Continue
    }
}
```

**路径 B：独立的分类 Focus（解耦，更灵活）** ⭐ 推荐方向

用 agent-works 的 `Focus` 做一个独立的任务分类器（详见 §3.4.3）：

```rust
// src/task_classifier.rs
use agent_works::focus::Focus;

pub fn build_task_classifier(fast_client: Arc<dyn StreamClient>) -> Focus {
    Focus::new(
        fast_client,  // 分类用 Haiku，快且便宜
        r#"你是一个编码任务分类器。根据任务描述和仓库结构，判断任务类型。

分类标准：
- execution: 目标明确，按步骤做。例："给 User 加个 email 字段"、"修个 typo"
- coordination: 跨多文件/多模块，需要保持一致性。例："统一所有 API 返回格式"
- design: 需要先想清楚怎么做。例："把单体拆成微服务"、"设计插件系统"

输出 JSON：{"kind": "execution|coordination|design", "reason": "..."}"#,
    )
}
```

路径 B 的 middleware（在 decompose 调用前先做分类）：

```rust
// src/model_upgrade.rs — 路径 B 版本
pub struct ModelUpgradeMiddleware {
    classifier: Focus,               // 分类 Focus（Haiku）
    strong_client: Arc<dyn StreamClient>,
    upgraded: AtomicBool,
}

impl Middleware for ModelUpgradeMiddleware {
    fn on_tool_call(&self, ctx: &ToolCallContext) -> MiddlewareAction {
        if self.upgraded.load(Ordering::Relaxed) || ctx.tool_name != "decompose" {
            return MiddlewareAction::Continue;
        }
        // 在 decompose 执行前，先用 Focus 分类
        let task = ctx.args.get("task").and_then(|v| v.as_str()).unwrap_or("");
        let input = Context::new().add("task", task);
        // 注意：on_tool_call 是同步的，需要 block_on 或改为 on_tool_call_async
        // 具体实现取决于框架 middleware 是否支持 async
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                self.classifier.ask::<TaskType>(&input, Duration::from_secs(5))
            )
        });
        match result {
            Ok(output) if output.result.kind == TaskKind::Design => {
                self.upgraded.store(true, Ordering::Relaxed);
                ctx.llm_engine.set_client(self.strong_client.clone());
                tracing::info!("classifier: design task, auto-upgraded to strong");
            }
            Err(e) => {
                tracing::warn!("classifier failed, staying on default: {}", e);
            }
            _ => {} // Execution/Coordination → 不升级
        }
        MiddlewareAction::Continue
    }
}
```

优点：
- 分类和分解完全解耦
- 分类器可以独立调用（不依赖 decompose 是否被调用）
- 每个 Focus 独立选模型（分类用 Haiku，分解用 Sonnet）
- 未来可以加更多 Focus（风险评估、优先级判断等）

缺点：
- 多一次 LLM 调用（Haiku，成本低）
- 需要决定调用时机（decompose 之前？并行？agent 首次思考时？）
- middleware 的 `on_tool_call` 可能不支持 async（需确认框架）

> **待讨论**：
> 1. 路径 A 还是路径 B？
> 2. 如果路径 B，middleware 是否支持 async？如果不支持，分类 Focus 的调用放在哪？
> 3. 分类 Focus 的结果怎么传给 middleware？（上面的实现是 middleware 直接调 Focus，另一种方案是通过 ToolContext 共享状态）

#### 改动 4：phi-agent 框架（最小改动，同之前）

**a) `MultiAgentConfig` 加 `child_client` 字段**

```rust
pub struct MultiAgentConfig {
    // ... 现有字段 ...
    /// Optional: use a different LLM client for child agents.
    /// If None, children inherit the parent's client (current behavior).
    pub child_client: Option<Arc<dyn StreamClient>>,
}
```

**b) `SummarizingMiddleware` 支持指定 client**

```rust
pub fn with_summarizer_client(self, client: Arc<dyn StreamClient>) -> Self { ... }
```

### 5.3 框架改动的侵入性评估

| 改动 | 文件 | 行数 | 破坏性 |
|------|------|------|--------|
| `MultiAgentConfig` 加字段 | agent-works 或 phi-agent | ~5 行 | 无（`..default()` 兼容） |
| 子 agent spawn 用 child_client | phi-agent agent factory | ~10 行 | 无（None 走原逻辑） |
| `SummarizingMiddleware` 接受自定义 client | phi-agent compression | ~15 行 | 无（不传走原逻辑） |
| **任务分类 Focus**（路径 B） | phimint `src/task_classifier.rs` | ~40 行 | **无（phimint 内，复用 agent-works Focus）** |
| **model upgrade middleware** | phimint `src/model_upgrade.rs` | ~40 行 | **无（phimint 内）** |

总计约 30 行框架改动 + 80 行 phimint 新增，全部向后兼容。

如果选路径 A（decompose 输出带 task_type），phimint 新增减少到 ~40 行（只加 middleware），但分类和 decompose 耦合。

## 6. 未来演进（本期不做）

### 6.1 更精细的路由规则

当前路由是 task_type → 模型的简单映射。未来可以做更精细的判断：

- **基于 verify 失败频率**：如果默认模型阶段 verify 连续失败 N 次，自动升到 strong（结果驱动，不是预判）
- **基于仓库规模**：大仓库（>100 文件）的 Coordination 任务可能需要 strong
- **用户可配置路由表**：让用户自定义 task_type → 模型的映射（`~/.phimint/model_routing.toml`）

### 6.2 按 provider 分层

不同模型可能来自不同 provider（Anthropic 直连 + OpenAI 兼容 + 本地 Ollama）。当前 `LlmClientBuilder` 已支持 provider 路由，但 `with_model()` 只换模型名不换 provider。未来可以让 fast_model 走 Ollama 本地推理（零成本）。

### 6.3 成本追踪

在 session 日志里记录每层模型的 token 用量和成本，让用户清楚"子 agent 省了多少"、"自动升级多花了多少"。

### 6.4 agent 自选模型（select_model 工具）

给主 agent 加一个 `select_model` 工具，agent 在推理过程中自行决定是否切换模型。当前的自动升级是中间件级别的（工具输出触发），`select_model` 是 agent 意识层面的（agent 主动选择）。两者可以共存——中间件做粗粒度自动升级，`select_model` 做细粒度手动调整。

### 6.5 per-slice 模型分配

当前设计中，子 agent 统一用 fast 模型。如果 decompose 的输出里每个 slice 都带 `task_type`，可以做到：简单 slice 用 fast，复杂 slice 用 default 甚至 strong。需要框架层支持子 agent 指定不同的 client。

### 6.6 Focus 组合模式（Focus Pipeline）

如果 §3.4.3 的 Focus 方案落地，未来可以构建 Focus 管道：

```
用户输入 → Focus("意图理解") → Focus("任务分类") → Focus("风险评估")
                                                    ↓
                                          路由器决定模型
                                                    ↓
                                          Focus("任务分解") → 主循环
```

每个 Focus 独立选模型、独立优化 prompt、独立测试。Focus 之间通过结构化 JSON 传递结果。

这比把所有判断塞进一个巨大的 prompt 里更可控、更可测、更省 token。

## 7. 验证

### 7.1 通用验证（与路径选择无关）

- **零配置回退**：不设 `LLM_FAST_MODEL` 和 `LLM_STRONG_MODEL`，行为与现在完全一致
- **分层生效**：设 `LLM_FAST_MODEL=claude-haiku-4`，spawn 子 agent 时日志显示模型名是 haiku；主循环仍是 sonnet
- **CLI 覆盖**：`--fast-model claude-sonnet-4` 覆盖 env
- **`--model opus`**：主循环切到 opus，子 agent 仍用 fast_model；自动升级逻辑跳过（手动指定优先）
- **单测**：`with_model()` 返回的 client 使用新模型名、共享连接池
- **回归**：`cargo test` 全绿、`cargo clippy` 干净、TUI / inline 两种模式正常

### 7.2 自动升级验证（路径 A：decompose 输出带 task_type）

- **decompose 触发**：设 `LLM_STRONG_MODEL=claude-opus-4`，给一个设计型任务，decompose 输出 `task_type: "design"`，日志显示 "auto-upgraded to strong model"
- **update_plan 触发**：agent 调 update_plan 且输出 `task_type: "design"`，日志显示升级
- **不触发**：简单任务（Execution 类型），全程 sonnet，无升级日志
- **只升一次**：连续多次触发，日志只出现一次升级记录
- **未配置不生效**：不设 `LLM_STRONG_MODEL`，无论 task_type 是什么都不切换

### 7.3 自动升级验证（路径 B：独立 Focus 分类器）

- **分类 Focus 触发**：设 `LLM_STRONG_MODEL=claude-opus-4`，给一个设计型任务，分类 Focus 输出 `kind: "design"`，日志显示 "auto-upgraded to strong model"
- **分类 Focus 不触发**：简单任务，分类 Focus 输出 `kind: "execution"`，全程 sonnet
- **分类 Focus 失败回退**：Focus 超时或解析错误，回退到默认模型，不升级，日志显示 warning
- **分类 Focus 模型正确**：分类 Focus 使用 fast_client（Haiku），不随主循环升级
- **未配置不生效**：同路径 A

## 8. 成本估算

以 Claude API 定价为例（假设 Sonnet 1x，Haiku 0.1x，Opus 3x）：

### 场景 A：简单任务（不触发自动升级）

| 组件 | 改前（全 Sonnet） | 改后 | 节省 |
|------|-------------------|------|------|
| 主循环 | $X | $X（Sonnet，不变） | 0 |
| 子 agent（~3 个） | $3Y | $0.3Y（Haiku） | ~90% |
| 压缩摘要 | $Z | $0.1Z（Haiku） | ~90% |
| 分类 Focus（路径 B） | — | $0.05Y（Haiku，~500 token） | 新增，成本极低 |
| **总计** | base | ~base × 0.7 | **~30%** |

### 场景 B：复杂任务（触发自动升级到 Opus）

| 组件 | 改前（全 Sonnet） | 改后 | 变化 |
|------|-------------------|------|------|
| 主循环（升级后） | $X | $3X（Opus） | +200% |
| decompose | $W | $3W（Opus，质量更高） | +200% |
| 子 agent（~5 个） | $5Y | $0.5Y（Haiku） | -90% |
| 压缩摘要 | $Z | $0.1Z（Haiku） | -90% |
| **总计** | base | ~base × 1.2 | **+20%** |

**关键**：复杂任务多花 20%，但 Opus 的一次做对率更高，**减少了 verify 循环的迭代次数**。如果 Sonnet 需要 3 轮 verify 才通过，Opus 只需 1 轮，实际总成本可能更低：

```
Sonnet 路径：$X × 3 轮 = $3X（含 verify 重试）
Opus 路径：  $3X × 1 轮 = $3X（一次做对）
→ 成本持平，但时间省 2/3
```

实际效果需要真机测试验证。

## 9. 分阶段计划

### Phase 1：fast 模型（子 agent + 压缩）— 确定有收益

**范围**：只做 fast 模型层，不动 strong 模型和自动升级。

- `main.rs`：构建 fast_client
- `agent.rs`：子 agent 用 fast_client、压缩用 fast_client
- 框架改动：`MultiAgentConfig.child_client` + `SummarizingMiddleware.with_client()`
- CLI：`--fast-model` 参数 + `LLM_FAST_MODEL` 环境变量

**收益**：子 agent 和压缩成本降 ~90%，零风险（不设 fast_model 时行为不变）。
**工期**：~2 天（含框架改动 + 测试）。

### Phase 2：strong 模型 + 分类逻辑 — 自动升级

**范围**：在 Phase 1 基础上，加 strong 模型层和自动升级。

- `main.rs`：构建 strong_client
- `agent.rs`：挂载 model upgrade middleware
- 分类逻辑：选路径 A（改 decompose prompt）或路径 B（独立 Focus）
- CLI：`--strong-model` 参数 + `LLM_STRONG_MODEL` 环境变量

**收益**：复杂任务自动升级到 Opus，省去用户手动 `--model opus`。
**工期**：~3 天（路径 A）或 ~4 天（路径 B，含 Focus 集成）。
**前置条件**：Phase 1 完成。

### Phase 3：演进（未来）

- per-slice 模型分配
- Focus Pipeline
- 更精细的路由规则
- 成本追踪

详见 §6。

## 10. 风险分析

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| **分类不准：误判为 Execution（漏升级）** | 复杂任务用 Sonnet，可能 verify 多迭代几轮 | verify 循环兜底；用户可 `--model opus` 手动覆盖 |
| **分类不准：误判为 Design（多升级）** | 简单任务用 Opus，多花钱但质量不降 | 成本多 ~3x，但不会产出错误结果 |
| **Focus 调用失败（超时/解析错误）** | 无法分类，可能错过升级机会 | 回退到默认模型，不升级；日志 warning |
| **`with_model()` 跨 provider 不兼容** | 不同 provider 的模型名格式不同 | 当前 `with_model()` 只换模型名，不换 provider；跨 provider 需要未来支持 |
| **框架改动引入回归** | `MultiAgentConfig` / `SummarizingMiddleware` 的改动影响现有功能 | 所有改动向后兼容（`..default()` / 不传走原逻辑）；充分单测 |
| **自动升级后 Sonnet → Opus 的延迟感知** | Opus 比 Sonnet 慢 2-3 倍，用户感知到变慢 | 升级时注入系统消息告知用户；只对 Design 类型任务升级 |
