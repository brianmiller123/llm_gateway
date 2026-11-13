---
report_kind: qc
reviewer: qc-specialist-3
reviewer_index: 3
plan_id: "2026-11-12-long-stream-and-admin-fixes"
verdict: "Request Changes"
generated_at: "2026-11-12"
---

# Code Review Report

## Reviewer Metadata
- Reviewer: @qc-specialist-3
- Runtime Agent ID: qc-specialist-3
- Runtime Model: deepseek/deepseek-v4-flash
- Review Perspective: Performance and reliability risk（热路径开销、超时矩阵、故障可观测性、资源生命周期、LDAP/迁移成本）
- Report Timestamp: 2026-11-12

## Scope
- Changed scope: `a65384d..3152498`（15 文件，+1189/−45）中与性能/可靠性直接相关的 hunks：
  - `src/service/proxy.rs` priming 循环与 deadline `select!`、`SseErrorTail`、`SseErrorContext`、`wrap_stream`/`BillingStream`/`BillingOnDrop`、`StreamTimeoutStream`、`PingKeepaliveStream`、`send_upstream` 超时接线
  - `src/config.rs` 超时配置与默认值；`src/service/console.rs` 登录路径；`src/state.rs` `reload_admin_ids`；`src/api/console.rs` `update_user` 刷新与审计；`src/store/users.rs` 查询与测试；`migrations/0033_admin_source.sql`；`README.md` 配置表
- Reused evidence: 计划 §3 RCA / §4 Design / §6 Verification / §7 Deferred；`docs/plans/review/taskA.md`、`taskB.md` 的已处置结论（不复用其判断，仅避开已登记的处置项）；实现者 build/test/e2e 声明按 L1 证据对待，未重跑。
- plan_id: `2026-11-12-long-stream-and-admin-fixes`
- Review range / Diff basis: `git diff a65384d..3152498`（base `a65384d` = main，head `3152498`）
- Working branch (verified): `fix/long-task-admin`
- Review cwd (verified): `/home/pop/AI/code/llm_gateway.wt/integration`
- Files reviewed: 10 个 diff 文件打开/检索（proxy.rs、config.rs、state.rs、api/console.rs、service/console.rs、service/session.rs、store/users.rs、0033_admin_source.sql、README.md、计划文件）
- Truncated coverage: `web/src/views/Users.vue`、`web/src/api/types.ts`、`scripts/mock_upstream.py` 未打开（不在性能/可靠性焦点，属 qc1/qc2 覆盖面）；`docs/plans/review/taskA.md` / `taskB.md` 仅按 grep 读取结论行。budget 内停止，结论仅覆盖上述已读范围。
- Commit range: `a65384d..3152498`（与 Review range 相同）
- Analysis methods: git-diff、read、grep（未运行 build / test / lint / e2e）

## Findings

### 🔴 Critical
（无）

### 🟡 Warning

- **F-001 提交后（post-commit）上游流失败不再进入熔断：deadline commit 无条件记 success，之后的首字节/静默超时以错误帧收尾且永不 `record_failure` → “200 但无产出”的故障供应商永不被隔离，每个请求吃满默认 300s 窗口并占住并发槽** -> 在三处收紧：①`prime_deadline_commits` 提交时把“本次是否见过产出性事件”带出 priming 循环；②`SseErrorContext` 增加 `provider_id` + `productive_seen`，`SseErrorTail` 在“终止性 `Err` 且 `is_timeout_error` 且 `!productive_seen`”时调用 `st.breaker.record_failure(provider_id)`（需把 `AppState`/breaker 句柄按 `UsageCapture` 的方式共享给 tail）；③（可选，防无界挂起）为“持续中性帧、永无产出”增加一个跨 chunk 的墙钟上限，而不是仅靠会被每个中性帧重置的 idle 窗口
  - Verification: read + grep 调用点普查——`record_failure` 仅出现在 `proxy.rs:1189/1283/1299/1391/1412/1434/1491/1807/1900/1917/1927`（全部在提交前或其他链路），`wrap_stream`/`SseErrorTail` 内没有任何 `record_failure`；`record_success` 在提交后无条件执行（`proxy.rs:1497`），而 deadline 提交块 `proxy.rs:1355-1370` 在零产出输入下同样落到 1497；`StreamTimeoutStream::arm_idle` 每个 chunk 都重置 idle 定时器（`proxy.rs:67-77`、`104-110`），纯注释帧/角色帧同样重置
  - Expected vs observed: 期望——“不改动……路由/熔断”（计划 §4.5）且 priming 窗口内的零产出故障继续计失败；实际——deadline（默认 2s）提交即 `record_success`，随后 `stream first byte timeout`/`stream idle timeout` 只被 `SseErrorTail` 转成一条 SSE 错误帧（`proxy.rs:3095-3112`），熔断器对该 provider 只见成功；上游持续发中性帧（`: ping`、`role` delta）时 idle 永不触发，连接无网关侧上界
  - 影响链：并发 guard 绑定响应体生命周期（README 实时监控口径），一条挂起请求会把“每用户×模型在途”槽位与一条上游连接占满 `first_byte/idle`=300s；多个挂起请求把该用户打到并发上限，用户侧表现为持续排队/无输出。计划登记过的代价只有“deadline 后错误帧不切候选”，未登记“不计熔断 / 无界中性帧挂起”，因此不属重复争论已决设计。
  - 配置相关性：默认 `GATEWAY_STREAM_PRIME_TIMEOUT_MS=2000` 下，2xx 后卡死必然先被提交（不可能再走 504 + 候选切换），故本项的暴露面等于“所有 2xx 后卡死”，而非小概率路径。

### 🟢 Suggestion

- **F-002 提交后错误帧的日志缺少 request_id / provider / model，无法与请求、供应商关联** -> 给 `SseErrorContext` 增加 `request_id: String`（`meta.request_id` 现成可用）与 `provider: String`，把 `tracing::warn!` 补成 `request_id = %ctx.request_id, provider = %ctx.provider, model = %ctx.model`；同样给 `proxy.rs:1360-1365` 的 priming-deadline info 日志补 `request_id`/`model`
  - Verification: read + grep 对照——`proxy.rs:3108-3111` 的 `tracing::warn!(error = %e, ...)` 只有 `error` 字段；`SseErrorContext`（`proxy.rs:2996-3002`）只有 `dialect/response_id/model`（`response_id` 仅在 Responses 方言里隐含 request_id，Chat 方言下整条错误帧无任何可关联标识）；同文件其他故障日志均带 `request_id`/`provider`/`model`（如 `proxy.rs:1540-1543`、`1834-1838`）
  - Expected vs observed: 期望——故障可观测性与既有日志口径一致，能回答“哪个请求/哪个供应商在提交后断流”；实际——只有一条裸错误串。提交后正常 EOF 走 `BillingOnDrop` 之外的 tail 分支时不会再有 warn（流正常结束），此为唯一留痕点。

- **F-003 `prime_deadline_commits` 的 `expired` 形参是死参数，且计划 §6 要求的 priming 单测未落地** -> 删除 `expired` 形参（唯一调用点已硬编码 `true`，`proxy.rs:1359`），或保留并补 `expired=false` 用例；补两条纯函数单测：SSE 到期提交 / 非 SSE 到期不提交；若采纳 §4.1 原文语义，再补“有缓冲不提前提交”
  - Verification: grep——`prime_deadline_commits` 仅在 `proxy.rs:144` 定义、`1359` 调用；新增测试只有 `is_timeout_error_walks_source_chain_and_texts`（3870）、`sse_error_tail_*`、`sse_error_frame_shapes_follow_client_dialect`，无 priming deadline / `stream_prime_deadline` / “ping 只在行边界注入”用例
  - Expected vs observed: 计划 §6 W1 明确列出“priming deadline 到期提交（空缓冲 SSE）、有缓冲不提前提交……ping 只在行边界注入”为单测判据；实际该三项无单测（`at_line_start` 行边界保护本身已存在于 `proxy.rs:186-258`，但无回归测试）。当前 e2e ① ② ③ 覆盖了提交路径，故定为 Suggestion 而非 Warning；`expired` 恒真使该 helper 的契约是虚构的（无分支被验证）。

- **F-004 与计划 §4.1 的字面语义漂移：deadline 在 `is_sse` 时无视 `buffered` 是否非空即提交** -> 二选一收敛：把 §4.1 与 §6 的措辞改成代码现语义（“SSE 到期一律提交，缓冲照常先透传”），或在 `buffered` 非空时保留原逻辑
  - Verification: read——`prime_deadline_commits(expired, is_sse) = expired && is_sse`（`proxy.rs:144-146`）不含缓冲条件；代码自己的注释（`proxy.rs:141-143`）已声明“缓冲为空或仅有中性帧同样提交”；提交时缓冲块仍按序透传（`proxy.rs:1520-1524` 的 `iter(buffered).chain(stream)`），无字节丢失
  - Expected vs observed: 计划 §4.1 期望“仅当 `is_sse && buffered.is_empty()` 时到期提交（保留‘上游 200 但回 JSON 体’的既有识别路径：已有缓冲则继续原逻辑）”；实际对 `is_sse` 且上游返回整段非 SSE JSON 的场景，以前会走“零产出断流 → 502 + 候选切换”，现在 2s 提交并把 JSON 体以 `text/event-stream` 透出。后者对客户端更友好，但属未登记的行为变化，需计划侧对齐。

### ⚪ Unconfirmed
（无）

## Source Trace
- F-001 — Source Type: git-diff + read + grep；Source Reference: `src/service/proxy.rs:1355-1370`（commit）、`:1497`（record_success）、`:3095-3112`（post-commit 错误帧）、`:67-77`（arm_idle）、`:1491` 与全文件 `record_failure` 调用点普查；`docs/plans/...-fixes.md` §4.5；Confidence: High
- F-002 — Source Type: read + grep；Source Reference: `src/service/proxy.rs:2996-3002`、`:3108-3111`、对照 `:1540-1543`；Confidence: High
- F-003 — Source Type: grep；Source Reference: `src/service/proxy.rs:144`、`:1359`、测试函数清单（`grep "fn .*deadline|priming|timeout|idle|first_byte"`）；计划 §6 W1 判据列表；Confidence: High
- F-004 — Source Type: read；Source Reference: `src/service/proxy.rs:141-146`、`:1520-1524`、计划 §4.1；Confidence: Medium（行为正确性无异议，争议只在计划/代码措辞是否一致）

## Residual assessment
- F-001：**可以在本次范围修复**，改动局限在 `proxy.rs`（`SseErrorContext` + `SseErrorTail` + `wrap_stream` 签名与 1 个调用点，约 10–15 行 + 1 条单测），不触碰计划已冻结的“deadline 后不切候选”决策。
  若 PM 判定延后：按 Durable Roadmap Gate 必须落成可追踪载体（计划 §7 或 store issue，写清“200-无产出供应商不计熔断 / 中性帧流无上界”这一具体现象、owner 与完成定义），届时本席位结论可降为 Approve-with-residuals；仅口头登记不接受。
- F-002 / F-003 / F-004：均为本次可修的建议项（各 < 1 个文件的局部改动）；F-004 亦可仅改计划措辞，无需改代码。
- 明确不作为 finding、但建议记录的成本（已核对，可接受）：LDAP 登录新增一次 `find_by_username`（唯一索引查询，非热路径，换取了“权限是否翻转”的精确判定）；`reload_admin_ids` 每次真实权限变更一次 `SELECT id FROM users WHERE is_admin AND status = 1`（小表 seq scan，且被 `admin_ids_changed` 与 `admin_action.is_some()` 双重收窄）；迁移 0033 的回填 `UPDATE` 会重写 `users` 全表（PG11+ `ADD COLUMN ... DEFAULT` 为元数据操作；本表规模下锁时间可忽略），且非幂等 + 回滚风险已在迁移文件与 README 内写明。

## Evidence check
- **每 chunk 热路径（焦点 1）**：`select!{ biased; deadline 先于 item }` 的分支顺序理由成立且注释与实现一致（`proxy.rs:1345-1368`）——deadline 先 poll 保证同步就绪的 chunk 洪水不会饿死 `Sleep` 的 waker 注册；到期后 `verified` 两条出口互斥（SSE → `committed=true` 退出；非 SSE → `prime_deadline=None` 后回到无 deadline 等待），已完成的 `Sleep` 不会再被 poll，无忙循环。每轮只新建一个 `Next` 栈上 future，`Sleep` 跨轮复用，无新增堆分配；`erroneous` 取消安全成立（deadline 先 Ready 时 `item` 分支根本未被 poll，不会丢 chunk）。`SseErrorTail` 状态机正确：`finished` 先于 inner poll 判断 → 错误帧之后不再触碰上游；`Poll::Pending` 经 `other => other` 原样传播（无自旋、无缓冲，背压直达 hyper）。`wrap_stream` 的 `Arc<UsageCapture>` 逐帧克隆口径未被本 diff 破坏。
- **超时矩阵（焦点 2）**：无“网关更短的暗时限”——`non_stream_timeout` 只在 `!streamed` 分支施加（`proxy.rs:2909-2912`），不会给长流加 600s 总闸；header 等待 = `provider.timeout_ms`（`2916`/`2924-2926`，文案 `upstream header timeout` 由本仓产生，文案匹配可靠）；first-byte/idle 默认 300/300；priming 2000ms；下行心跳 15s（`proxy.rs:30`）。因此对“真静默”的上游，网关是确定性的先动方，且因 t≈2s 起客户端就持续收到 `: ping`，客户端 300s 看门狗不会被喂到边缘（默认值与客户端同值但已被心跳解耦）。`is_timeout_error` 的类型判定能力与注释一致：流内超时错误是 `String` 支撑的 `Box`（`proxy.rs:95-101`），reqwest 错误经 `Box::from(e)` 保留类型（`proxy.rs:112-116`），header 路径被压平成字符串（`2916`）故只有文案匹配——两条路径都被 504 分类正确覆盖。
- **故障可观测性（焦点 3）**：usage 恰好一次——`recorded` 以 `SeqCst` swap 在“终态事件分支”“`once` 流尾兜底”“`BillingOnDrop::drop`”三处互斥（`proxy.rs:3150-3200`、`:2945-2961`）；错误帧是 `Ok(bytes)` 产生，仍会流经 `then` 的终态判定与流尾兜底，既不重复也不漏记。502/504 分类准确（`1430-1444`）：priming 内超时 → 504 且置 `saw_timeout`，传输错误/零产出断流 → 502 且置 `saw_connect_error`。日志字段缺口见 F-002。
- **资源（焦点 4）**：长静默/长预填充内存恒定——priming 缓冲上限 256KiB（`proxy.rs:1337`、`2522`、`2597` 三重约束），错误帧只在出错时一次性分配（Chat/Anthropic 帧 < 200B，Responses 帧为一个结构体序列化）；客户端中途断开由 `BillingOnDrop` 兜底且不遗留任务；上游 EOF / 候选耗尽路径均返回 stream 结束，无泄漏。仅 F-001 的“持续中性帧”场景是可用性（而非内存）风险。
- **LDAP 侧（焦点 5）**：成本口径见 Residual assessment，均可接受；三列不变式 `is_admin = COALESCE(admin_override, admin_ldap)` 的所有读方仍读有效列（`src/api/v1.rs:226`、`src/service/notify.rs:220`、`src/store/users.rs:186-192`），无读方回归。
- **实现者/L2 证据自洽性**：build/309 test/e2e 均为 L1 声明，本席位未复跑。与其声明**存在一处需要在 PM Acceptance 时对齐的配置前提**：在默认配置（prime 2000ms < first-byte 300s）下，“2xx 后真卡死”必然先被 deadline 提交，最终是 `200 + 错误帧`（300s 时）而不是 504；所以“真卡死 3s 窗口 → 504”只有在把 first-byte 窗口设到小于等于 priming deadline（或 `GATEWAY_STREAM_PRIME_TIMEOUT_MS=0` / 调大）时才可复现。这与 §1 判据（“未向客户端写出任何字节前失败 → 504”）并不矛盾，但 README/计划未写明该前置条件，验收复现时容易误判；建议在 §6 或 README 补充一句配置前提。除此之外，`taskB.md` 中 W1/W2/S1–S4/S6 的处置与代码现状一致（`reset_admin_source` 的 SQL 侧 `WHERE source='ldap'` 守卫 + API 侧 400 双保险），未见自相矛盾的证据。

## Summary
| Severity | Count |
|----------|-------|
| 🔴 Critical | 0 |
| 🟡 Warning | 1 |
| 🟢 Suggestion | 3 |
| ⚪ Unconfirmed | 0 |

**Verdict**: Request Changes —— 1 项 Warning（F-001：提交后失败不计熔断、零产出 2xx 供应商永不被隔离且挂起无网关侧上界），修复面小且落在本次范围内；F-002/F-003/F-004 为非阻塞建议。
