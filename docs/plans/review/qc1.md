---
report_kind: qc
reviewer: qc-specialist
reviewer_index: 1
plan_id: "N/A (docs/plans/2026-11-12-long-stream-and-admin-fixes.md)"
verdict: "Request Changes"
generated_at: "2026-11-12"
---

# QC1 — plan QC tri-review

- Reviewer: @qc-specialist（reviewer_index=1，focus=架构一致性/可维护性）
- Review Perspective: architecture coherence & maintainability risk
- Report Timestamp: 2026-11-12

## Scope

- Changed scope: `git diff a65384d..3152498` — 15 文件，+1189/−45。重点读取：`src/service/proxy.rs`（priming deadline、超时分类、SSE 错误尾帧、`wrap_stream`）、`src/config.rs`（新增配置面）、`src/store/users.rs`（双来源管理员）、`src/store/groups.rs`（既有 LDAP 建档路径）、`src/service/console.rs`、`src/api/console.rs`、`src/state.rs`、`migrations/0033_admin_source.sql`、`README.md`、`web/` diff、`docs/plans/2026-11-12-long-stream-and-admin-fixes.md`。
- Reused evidence: L2 的 taskA/taskB 结论按 Assignment 摘要采信（未重新打开原文）；实现者 build/test/e2e 证据按 Assignment 采信（未重跑，规则禁止）。
- plan_id: N/A（no-plan mode，计划载体 `docs/plans/2026-11-12-long-stream-and-admin-fixes.md`）
- Review range / Diff basis: `git diff a65384d..3152498`（base=main, head=integration HEAD）
- Working branch (verified): `fix/long-task-admin` @ `/home/pop/AI/code/llm_gateway.wt/integration`（`git rev-parse --show-toplevel` 确认）
- Files reviewed: 11（其中 `web/`、READMe、计划以 diff 形式读取）
- Truncated coverage: `scripts/mock_upstream.py` 新增模式、`docs/plans/review/taskA.md` / `taskB.md` 原文、`web/src/views/Users.vue` 除 diff 外的上下文、`users.rs:264-381` 的 sqlx 测试体（其余测试断言通过 grep 抽查）、0033 之前的历史迁移回放未验证。
- Analysis methods: git-diff / read / grep（绝对路径复核工作树，未运行 test/build/lint）

## Verdict: Needs-fix

## Summary
| Severity | Count |
|----------|-------|
| 🔴 Critical | 0 |
| 🟡 Warning | 3 |
| 🟢 Suggestion | 4 |
| ⚪ Unconfirmed | 0 |

**Verdict**: Request Changes（= Needs-fix；Critical 0，但存在 3 条未处置 Warning）

## Findings

### 🔴 Critical

`findings: []`（无 Critical）

### 🟡 Warning

- **W1 — 透传链路的 SSE 错误帧与转换链路是两套错误分类学，同一客户端方言出现两种形状** — `src/service/proxy.rs:3013-3075`（`sse_error_frame`）vs `src/service/anthropic/stream.rs:629-637`（`error_event`）与 `src/service/responses/stream.rs:371-380`（`record_transport_error`）。
  - 证据：同一失败类（上游流 `Err`）在 Anthropic 方言下，透传路径输出 `{"type":"error","error":{"type":"api_error"}}`，转换路径输出 `{"type":"error","error":{"type":"stream_error"}}`；Responses 方言下透传路径 `response.error.code = "upstream_error"`，转换路径 `"stream_error"`。同时透传路径用 `format!`/`json!` 手写帧，绕开既有构造器 `anthropic::anthropic_error_body`（`src/service/anthropic/mod.rs:71-77`）与 `responses::dto::sse_frame`。
  - 影响：同一 `/v1/messages` 或 `/v1/responses` 客户端，按 `error.type` / `error.code` 分支的 SDK 会看到两套词表；帧形状有三份实现，任一侧演进即漂移。计划 §4 W1.3 要求“转换链路现有错误帧行为保持不变并复用同一判定”，实现复用了判定（`is_timeout_error`）但复制了帧。
  - 建议修复方向：把三种方言的帧构造收敛到各自模块的单一构造器（`anthropic::stream::error_event` 等价物 / `responses::dto` 的 failed-event builder），proxy 侧只保留方言选择 + 一次调用；同时统一 `error.type`/`code` 词表（`stream_error` 或 `api_error` 二选一，写进计划）。
  - Verification: 双路径对比（`grep -n "stream_error" src/service/anthropic/stream.rs src/service/responses/stream.rs` / 读 `proxy.rs:3013-3075`）/ diff 锚点。
  - Expected vs observed: 同一方言同一失败类应只有一种帧形状与一种错误码 vs 实测两套（`api_error` vs `stream_error`、`upstream_error` vs `stream_error`）。

- **W2 — 本次最险的并发逻辑（priming deadline 与中性帧洪水）零单测，且计划 §6 明确要求了它** — `src/service/proxy.rs:144`（`prime_deadline_commits`）、`1341-1369`（`biased` select 排序）无任何测试；本次新增测试只有 `is_timeout_error`（3870）、`SseErrorTail`（3914）、方言帧（3954、4010）。
  - 证据：全仓 `prime_deadline_commits|stream_prime_deadline` 仅命中定义与调用点，无 `#[test]`；计划 §6 W1 要求“单测：priming deadline 到期提交（空缓冲 SSE）、有缓冲不提前提交、超时→`saw_timeout`”。
  - 影响：`biased` + 分支顺序正是防止“中性帧同步就绪洪水饿死 deadline”的唯一保障（注释 `1341-1352` 自述）。未来任何一次 `select!` 去 `biased`、换序或改回 `stream.next()` 都会静默复活 RCA S3（无响应头 → 客户端首事件超时），而现有自动化不会失败；e2e 是手工脚本，不是回归门。
  - 建议修复方向：为 `prime_deadline_commits` 加真值表单测；加一个 tokio 测试，用恒就绪中性帧流（如 `stream::repeat(Ok(role_delta))`）断言 deadline 内提交；`saw_timeout` 的接线（`1424-1439`）用一条断言覆盖。
  - Verification: grep 全仓测试符号 / 与计划 §6 判据逐条对照。
  - Expected vs observed: 计划要求的 3 个单测存在 vs 0 个。

- **W3 — 四个流式/超时开关无语义边界与优先级，默认值让 `GATEWAY_STREAM_FIRST_BYTE_TIMEOUT` 对 SSE 退化为近似空转** — `src/config.rs:53-58`（字段）、`140-148`（默认 300/300/2000）、`README.md:132-133`。
  - 证据：`stream_prime_deadline`（`proxy.rs:136`）在 SSE 上恒先于 first-byte 到期（默认 2s ≪ 300s），pre-commit 阶段由 PRIME 决定；post-commit 阶段 first-byte（t0 起算）与 idle（逐 chunk 重置）默认值相等（300/300），对“无字节”场景不可区分。因此 FIRST_BYTE 只在 `PRIME=0` 或响应体非 SSE 时才有独立可观测效果，但 README 与 config 注释仍按“首字节超时”描述它。
  - 影响：三处半重复开关（PRIME / FIRST_BYTE / IDLE）之外再加 `provider.timeout_ms` 与 `non_stream_timeout_secs`，共 5 个时间窗，无一处文档声明优先级；且 `PRIME ≥ FIRST_BYTE` 是静默失效配置——运维把 PRIME 调大而忘记同步 FIRST_BYTE，症状（无响应头 + 504/502）原样复发，无日志提示。
  - 建议修复方向：在 `config.rs` 字段注释与 README 行内写明优先级（`PRIME(pre-commit) < FIRST_BYTE(=fallback) ≈ IDLE(post-commit)`）；加载期在 `prime_timeout_ms >= first_byte_timeout_secs*1000` 且两者非 0 时 `tracing::warn!`（或 clamp）。若要减法：SSE 路径只保留 PRIME + IDLE，把 FIRST_BYTE 明确降级为“非 SSE / PRIME=0 兜底”并在文档里如此命名。
  - Verification: 读 config 默认值 + `stream_timeouts`/`stream_prime_deadline` 的取值点 / 交叉核对 README 两行。
  - Expected vs observed: 每个开关有唯一且文档化的语义域 vs 存在恒不触发的组合且无提示。

### 🟢 Suggestion

- **S1 — `is_timeout_error` 的注释对 priming 路径的 downcast 能力描述与会话事实相反** — 注释 `src/service/proxy.rs:161-171` 称“priming 路径（流内 `Err`）：`StreamTimeoutStream` 用 `Box::from(e)` 保留了 reqwest 类型，downcast 生效，类型判定可靠”；实际生产者 `src/service/proxy.rs:96-101` 对超时分支构造的是 `Box::from(format!("upstream stream {what} timeout"))`（`String`），downcast 永不命中，仅 `timeout_text` 生效；保留 reqwest 类型的是**内层** reqwest 错误分支（`110-114`）。
  - 影响：注释是字符串匹配被保留的唯一理由说明；维护者据此判定“downcast 已覆盖”而删掉 `timeout_text`，会让全部网关侧流超时退回 502/连接错误分类。
  - 建议修复方向：改注释为“两条路径的超时均是字符串错误，类型判定只覆盖 reqwest 自身超时；文本匹配是主路径”，或（更稳）让 `StreamTimeoutStream` 产出私有 `struct StreamTimeout(&'static str)` 实现 `Error`，downcast 与文本匹配二者之一即成立。
  - Verification: read 生产者 + 消费者两处，`grep -n "fn is_timeout_error"`。
  - Expected vs observed: 注释描述的判定机制 vs 代码实际可命中的判定分支。

- **S2 — LDAP 双来源不变式没有单一维护点：5 处写入点、无 DB 约束，`groups.rs` 仍按旧模型写** — 写入点：`src/store/users.rs:65`（`create_local_user`）、`89-101`（`upsert_ldap_user`）、`163`（`set_is_admin`）、`176`（`reset_admin_source`）、`src/store/groups.rs:124`（`upsert_sync_user`）；`migrations/0033_admin_source.sql` 只加列不加约束。
  - 证据：`upsert_sync_user`（`groups.rs:118-136`）的 INSERT 仍是 `(…, source, is_admin) VALUES (…, 'ldap', FALSE)`，`admin_ldap`/`admin_override` 靠列默认值成立；其文档注释仍写“不触碰 is_admin/status/source”，读者无法从该路径看出新模型。计划 §4 W2.2 只声明“两处写路径”，实际 5 处。
  - 影响：`load_admin_ids`（`users.rs:139-146`）与控制台中间件直接信任物化列 `is_admin`，不变式漂移不会表现为可见 bug，而是静默的授权错误（越权或误降权）；新增第 6 处写路径时没有任何编译期/DB 期拦截。
  - 建议修复方向：在 0033 追加 `CHECK (is_admin = COALESCE(admin_override, admin_ldap))`（已核对 5 处写入点与回填结果均满足，可安全加）；`upsert_sync_user` 显式写出 `admin_ldap = FALSE` 并更新注释为“同步不判定管理员：`admin_override` 保持 NULL、`admin_ldap` 恒 FALSE，等 JIT 登录按目录组决定”。
  - Verification: 读 5 处 SQL + 迁移 + `load_admin_ids`；对回填规则逐行推演不变式。
  - Expected vs observed: 不变式有单一强制点 vs 仅靠 5 处手写约定，其中 1 处隐性依赖默认值。

- **S3 — 透传 Responses 方言的 `response.failed` 用网关自造 id + 空 `output`，与客户端已收到的流自相矛盾** — `src/service/proxy.rs:3137-3142`（`response_id: format!("resp_{}", meta.request_id)`）、`3037-3075`（`output: Vec::new()`）。
  - 证据：native Responses 透传时，上游自己的 `response.created` 已把**上游** `response.id` 发给客户端；终止帧却使用网关伪造的 `resp_<request_id>`。转换链路相反：`responses/stream.rs` 的 `finalize` 会带着累积的 `output` 收尾。
  - 影响：按 `response.id` 关联终止事件的客户端会拿到未知 id；`output: []` 抹掉了客户端已经看到的增量输出，终止对象与已交付状态不一致。
  - 建议修复方向：`SseErrorTail` 已经看到每一块字节，可在首帧 `response.created` 中捕获真实 id 并复用（或把 failed-response 构造下沉到 `responses/stream.rs` 复用 finalize 语义）；若成本不可接受，登记为 residual 并在帧注释里写明该偏差。
  - Verification: read 构造点 + 对照 `responses/stream.rs:371-380` 的 finalize 路径。
  - Expected vs observed: 终止事件引用客户端已知的 response 与已产出的 output vs 伪造 id + 空 output。

- **S4 — 计划 §4/§6 与落地代码三处不一致（供后续维护者误读的设计契约）** —
  - (a) §4 W1.1 “仅当 `is_sse && buffered.is_empty()` 时到期即提交” vs 代码 `prime_deadline_commits(expired, is_sse) = expired && is_sse`（`proxy.rs:144`，调用 `1359`）。实现更宽：缓冲里只可能是中性帧，且提交时按 `buffered.chain(stream)` 原序重放（`proxy.rs:1519-1523`），因此行为正确；但 §4 的措辞与 §6 的“有缓冲不提前提交”测试项已成假命题。
  - (b) §4 W2.1 回填写 `admin_override = CASE WHEN source='local' …` vs 迁移实现 `CASE WHEN source <> 'ldap' …`（`migrations/0033_admin_source.sql:17-19`；迁移注释已解释这是为了让不变式对任意 source 成立）。
  - (c) §4 W2.2 “两处写路径都必须维护三列一致” vs 实际 5 处（见 S2）。
  - 影响：§4 是后续维护者判断“有意设计 vs 缺陷”的唯一契约；三处偏差会让下一个改动者按错误前提修改。
  - 建议修复方向：把 §4 的 (a)(b)(c) 与 §6 对应测试项各改一行，并把 (a) 的新行为写成设计意图（“中性帧缓冲也提交，因为提交时按原序重放”）。
  - Verification: 计划原文逐句 vs 代码/grep。
  - Expected vs observed: 计划与实现一字不差 vs 三处偏差。

### ⚪ Unconfirmed

`findings: []`（证据通道正常；未复核项已在 Scope 的 Truncated coverage 中列明）

## Residual assessment

- W1 / W2 / W3 均可在**本次范围**内修复，且都是小改动：W1 是把三份帧构造收敛为一份 + 统一词表（约 40 行净减）；W2 是补 2–3 个单测（`prime_deadline_commits` 真值表 + 恒就绪中性帧流的 deadline 测试）；W3 是文档两行 + 加载期一条 `warn!`。
- 若 PM 决定带残余交付，这 3 条必须进入计划 §7 Deferred 表（owner + 触发条件 + 完成定义），因为它们**不在**已登记的四项代价内（已登记的只有：priming 后错误帧不切候选、300s 默认窗口、迁移回填语义、多候选 header 叠加、用户名大小写规范化）。W2 尤其不可只留 e2e：S3 型症状正是“无人值守时静默回归”。
- S1–S4 可随任一 fix wave 一并处理；S2 的 CHECK 约束建议在 0033 尚未进 main 前合并（越晚成本越高：需新迁移 + 存量校验）。

## Evidence check

- **build/test 声明自洽性**：实现者称 `cargo build` 仅 2 条基线既有 dead_code 警告——逐项核对本次新增符号均有非测试用途（`stream_prime_deadline`→1335、`prime_deadline_commits`→1359、`timeout_text`/`is_timeout_node`→`is_timeout_error`、`SseErrorDialect`/`SseErrorContext`/`sse_error_frame`/`SseErrorTail`→1688-1708/3137、`unix_now`→Responses 臂、`reload_admin_ids`→`state.rs:268` 与 `console.rs:60`、`admin_source`→`api/console.rs:1209,1616` 与 web，`ResponsesResponseOut`/`ResponsesStreamEventOut`→仅 Responses 臂）。无不一致。
- **`wrap_stream` 调用点**：仅 `proxy.rs:1701` 一处，签名新增参数已同步，无遗漏调用点。
- **方言归属自洽**：透传分支只在 `anthropic_converted=false` 且 `converted=false` 时可达（`proxy.rs:1740-1770` 的两个转换分支均提前 return），故“方言跟随客户端端点”成立；`Chat` 作兜底对当前端点集合无歧义。
- **LDAP 证据自洽**：`admin_ids` 三条刷新触发（30s 周期 → `state.rs:268` 复用 `reload_admin_ids`、PATCH → `api/console.rs:1372-1376` 只 warn 不 500、JIT 登录 → `service/console.rs:57-63` 按 `admin_ids_changed` 条件刷新）与计划 §4 W2.4 一致；`reset_admin_source` 的 `WHERE source='ldap'` 与 API 层 400 双保险一致；迁移回填推演后不变式成立（LDAP：`COALESCE(NULL, is_admin)=is_admin`；非 LDAP：`COALESCE(is_admin, FALSE)=is_admin`）。
- **文档一致性**：README 三个流式变量（`README.md:132-133`）+ 默认值 + “仅 SSE 生效 / 到期后不切候选” + 0033 升级提示（`README.md:135`）齐备，与迁移注释一致；缺口仅在优先级（W3）与计划措辞（S4）。
- **未被验证的声明**：e2e 数字（90s/150s/60s 无 502、3s 窗口 504、idle → 错误帧 + 正常 EOF、LDAP 3 次登录保持）为实现者 L1 证据，本席位未重跑（规则禁止），代码与其结论一致；`web npm run build` 同此。**风险点**：这些行为里只有“帧形状/分类函数”进了自动化测试，priming deadline 的实际提交时机完全依赖手工 e2e（见 W2）。
