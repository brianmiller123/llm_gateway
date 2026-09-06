# Coding Plan 模型作用域（model_scope）设计

> 状态：已实现（迁移 `migrations/0024_plan_model_scope.sql`）。
> 关联：`docs/plans/hourly-reset-design.md`（周期与重置）、`README.md`「Coding Plan 周期用量限制与小时级重置」。

## 1. 背景与目标

Coding Plan（套餐配额）此前对所有模型无差别生效：绑定用户请求任何模型都会计入该 Plan 的
计数器、受其超额策略约束。网关多租户场景下需要按模型区分套餐，例如：

- 「Claude 专用包」：只对 `claude-*` 生效，用户请求 `gpt-4o` 走全局月度配额而非该包；
- 「廉价模型不限量」：对 `deepseek-*`、`gpt-4o-mini` 放开，但排除旗舰模型；
- 「灰度家族覆盖」：新家族上线路由后，前缀通配 `claude-*` 自动覆盖带日期后缀的变体
  （`claude-sonnet-4-5-20250929`）。

目标：

1. Plan 配置新增可选 `model_scope` 字段，支持精确匹配与前缀通配，白名单 + 黑名单，
   冲突时黑名单优先；
2. 未配置 = 对所有模型生效，**完全向后兼容**（存量 Plan 行为逐字节不变）；
3. 多 Plan 同时命中当前模型时有确定性的择优规则，冲突可追溯；
4. 配置写入与加载两处均做格式/合法性校验，拼写错误明确报错；
5. 切换模型即时重评估，无需重启会话或重新加载配置。

## 2. 配置形状

`coding_plans.model_scope` 为可空 JSONB：

```json
{ "allow": ["claude-*", "gpt-4o"], "deny": ["*-preview"] }
```

| 字段    | 类型       | 缺省         | 语义                                       |
| ------- | ---------- | ------------ | ------------------------------------------ |
| `allow` | `string[]` | 缺省/空 = 不限 | 白名单：模型需命中至少一条才对该 Plan 生效   |
| `deny`  | `string[]` | 空           | 黑名单：命中任一条即排除，**优先级高于 allow** |

- `NULL`（不配置）= 对所有模型生效；
- `{}` 或双空数组在解析期归一化为 `NULL`（等价不限制，避免存储歧义形态）；
- deny-only（`{"deny": ["gpt-*"]}`）= 除黑名单外全部生效；
- 条数上限：allow + deny 合计 ≤ 32；单条 pattern ≤ 128 字符。

### 2.1 pattern 语法

- **精确匹配**：`gpt-4o`、`claude-sonnet-4-5`、`gemini-2.5-pro`、`deepseek-chat`；
- **前缀通配**：仅支持**尾缀单个 `*`**（与模型路由 `model_pattern` 同语义），
  `claude-*` 命中 `claude-sonnet-4-5` / `claude-3-5-sonnet-20241022` 等全家族；
- **大小写不敏感**：pattern 在解析期归一化为小写，匹配对请求模型串同样大小写不敏感
  （`GPT-4o` ≡ `gpt-4o`）；
- 合法字符集：字母、数字、`.`、`_`、`+`、`:`、`-`、`/` 与尾缀 `*`
  （`/` 覆盖 OpenRouter 式 `anthropic/claude-sonnet-4` 与 Google 式 `models/gemini-2.5-pro`）。

明确拒绝（写入期 400）：

| 输入           | 原因                                                     |
| -------------- | -------------------------------------------------------- |
| `""` / 空格    | 空或含空白                                                |
| `gpt-*-mini`   | 中间 `*`：只实现尾缀通配，拒绝半实现语义                  |
| `**` / `*-x`   | 多个 `*` 或前缀 `*`                                       |
| 裸 `*`         | 等价于不配置作用域，显式写出只会制造歧义                  |
| 未知字段       | 如 `{"white": [...]}`——容器拼写错误直接报错              |
| 黑白名单同条目 | deny 优先下该 allow 条目永不命中，属无效配置              |

### 2.2 「拼写错误的模型名」的报错策略

两层防线：

1. **写入期（API，400 硬错误）**：精确条目（非通配）必须命中当前任一路由规则的
   `model_pattern`（大小写不敏感）。例：路由表只有 `claude-*` / `gpt-4*` /
   `gemini-*` 时提交 `allow: ["kimi-k2-coder"]`（未建路由的家族）→
   `400 unrecognized model 'kimi-k2-coder' in model_scope.allow: no routed model
   matches it (typo?); add a route first or use a wildcard pattern like 'kimi-*'`。
   - **边界**：前缀路由无法识别家族内的拼写差异——路由 `claude-*` 会命中
     `claude-sontnet-4-5`（拼写错误），此类笔误只能在请求 404「模型未路由」时暴露，
     作用域校验不误报也不漏报（它只保证「配了就路由得到」）；
   - 无任何路由规则时跳过该校验（空环境/测试不误伤），留 warn 日志；
   - 通配条目无法证伪（家族可能尚未建路由），只做格式校验。
2. **加载期（运行时，fail-closed）**：绕过 API 手改 DB 写入的损坏 scope 在
   `load_plan_runtimes` 解析失败时 `tracing::error!` 留痕并**剔除该 Plan**（宁可可修正地
   不限额，不可静默地错误限额），修复后下个 reload 周期自动恢复。

## 3. 匹配与择优规则

请求期解析（`store::plans::resolve_plan`，`AppState.plans` 内存热路径）：

```
候选 = 该用户的全部启用 Plan（分组 ∪ 直连双通道）
     → 过滤 1：当前处于生效时段（active_window，服务器本地墙钟）
     → 过滤 2：model_scope 命中当前请求模型（无模型上下文时跳过本过滤）
     → 择优键：(作用域具体度, priority, plan_id) 取最大
```

**作用域具体度**（specificity，越大越具体）：

| 具体度 | 含义                                   |
| ------ | -------------------------------------- |
| 2      | 配置了作用域且全部条目为精确匹配        |
| 1      | 配置了作用域且含通配条目                |
| 0      | 未配置作用域（对全部模型生效，最不具体）|

规则要点：

- **作用域更具体的 Plan 优先**——specificity 压过 `priority`：Claude 专用包（精确）会
  在 Claude 请求上压过不限模型的高优全局包；这使「通用套餐 + 专用叠加包」组合无需
  折腾 priority 数值；
- 同具体度按存量语义 `(priority, plan_id)` 最大（优先级高者胜，同分 plan_id 大者胜）；
- **无模型上下文**的调用（记账后告警、控制台不带 `model` 参数）完全沿用存量
  `(priority, plan_id)` 语义，作用域不参与过滤与排序；
- 多 Plan 计量仍不叠加：命中多个时只取一个生效（与存量「多分组取最高」一致）。

### 3.1 冲突可追溯

过滤后仍有 ≥2 个候选命中同一 (用户, 模型) 时，`resolve_plan` 输出一条结构化 warn：

```
WARN multiple coding plans match this model; picked by (model_scope specificity, priority, plan_id)
     user_id=42 model="claude-sonnet-4-5" chosen_plan_id=7 chosen_plan="claude-pro"
     chosen_specificity=2 overridden=[(5, "global", 0), (6, "claude-family", 1)]
```

排查「这个模型为什么没走我配的 Plan」时，按 user_id/model grep 日志即可还原完整择优过程。
该日志每请求最多一条，不做去重——冲突配置本身就是要消除的异常态。

## 4. 动态生效

无需重启、无需重新加载配置文件：

1. **切换模型**：作用域在请求期按当次请求的 `model` 字段求值（`check_plan` →
   `resolve_plan(..., Some(model))`），同一会话从 `claude-sonnet-4-5` 切到 `gpt-4o`
   的下一个请求立即按新模型重评估各 Plan 激活状态；
2. **改配置**：Plan CRUD / 成员变更后 `AppState::reload()` 即时刷新内存运行时
   （与存量行为一致）；另有 30s 周期 reload 兜底（`GATEWAY_RELOAD_INTERVAL`）。

## 5. 记账一致性

检查与记账**同源**：`service::plans::check_plan` 解析出的生效 Plan（`PlanDecision.plan`）
直接作为该请求的计量载荷（`UsageMeta.plan`），代理层不再二次解析。否则模型作用域下
二次解析可能选中另一个 Plan（或因不匹配漏记），把用量记到错误的配额上。超额降级
（`downgrade`）产生的用量同样记入**触发降级的 Plan**——降级是该 Plan 的处置动作。

记账后的阈值告警（`after_usage_record`）按代理路径传来的 `plan_id` 找回运行时，
不再无模型二次解析——避免告警挂到错误 Plan 的计数器上。

## 6. API

- `POST /api/admin/plans` / `PATCH /api/admin/plans/{id}`：新增 `model_scope` 字段
  （形状见 §2）。PATCH 三态：absent=保留 / `null`=清空（恢复全模型生效）/ 对象=设置；
  校验失败 400 并附明确原因（§2.1 / §2.2）。审计快照（变更前后）含该字段。
- `GET /api/me/plan`：新增可选 `?model=<客户端模型名>`——按该模型做作用域感知解析
  （与请求期 `check_plan` 同一语义），响应 `plan.model_scope` 字段返回解析后的作用域；
  不带 `model` 时行为与存量一致（忽略作用域）。

## 7. 配置示例

```jsonc
// Claude 专用包：全部 Claude 系列，排除旧的 2.x 家族
{ "allow": ["claude-*"], "deny": ["claude-2*"] }

// OpenAI 旗舰双模型精确包（具体度 2，同族通分包压不过它）
{ "allow": ["gpt-4o", "o3"] }

// 廉价模型包：黑名单式「除旗舰外全放行」
{ "deny": ["gpt-4o", "claude-opus-*", "gemini-2.5-pro"] }

// Google 家族（兼容 models/ 前缀方言）
{ "allow": ["gemini-*", "models/gemini-*"] }

// OpenRouter 斜杠命名
{ "allow": ["anthropic/claude-*"] }
```

## 8. 冲突排查指南

| 症状                                   | 排查                                                                                                |
| -------------------------------------- | --------------------------------------------------------------------------------------------------- |
| 配了 Plan 但某模型请求没被限额          | 1) 该模型是否命中作用域：`GET /api/me/plan?model=<模型>` 看 `plan.model_scope`；2) `claude-*` 不命中 `claude`（无分隔符前缀不命中）；3) 黑名单是否覆盖；4) grep warn 日志「multiple coding plans match this model」看择优过程 |
| 作用域配了但精确条目压不过别家 Plan     | 具体度只在**同模型**间比较；确认命中双方的 specificity（warn 日志含各候选具体度）                      |
| Plan 在管理页显示启用但完全不限额       | 存量 scope 损坏被加载期剔除：grep error「invalid coding plan model_scope; plan excluded」，按日志修 DB 或走 PATCH 重写 |
| 写入 400 `unrecognized model`           | 精确条目未命中任何路由 pattern：先建路由，或改用 `<家族>-*` 通配                                      |
| 写入 400 `unknown model_scope field`    | 字段名拼写错误（只接受 `allow` / `deny`）                                                            |
| 切换模型后限额立即变化是预期吗          | 是。作用域按每请求的 model 字段求值（§4）                                                            |

## 9. 测试覆盖

- 单元（`src/store/plans.rs` tests）：
  - `model_pattern_matching_across_vendors`：Anthropic / OpenAI / Google / DeepSeek /
    OpenRouter 五家命名规范；精确、前缀、日期后缀变体、大小写、`models/` 与斜杠方言；
  - `validates_model_patterns` / `parses_model_scope`：全部拒绝项与归一化；
  - `model_scope_blacklist_over_whitelist`：黑白名单冲突时黑名单优先、deny-only、具体度；
  - `resolve_plan_model_scope_specificity`：精确 > 通配 > 不限、priority 压不过具体度、
    黑名单排除回退、全不命中 → None、无模型上下文回存量语义。
- DB 集成（`#[sqlx::test]`）：`model_scope_crud_runtime_and_resolution`——CRUD 回读、
  运行时携带解析后作用域、按模型解析、PATCH 三态、损坏 scope fail-closed 剔除。
- 端到端（`scripts/test_plan_model_scope.py`）：API 校验报错、`/api/me/plan?model=`
  精确/通配/黑白名单/未配置/切模型动态生效、PATCH 即时生效与审计留痕。
