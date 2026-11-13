---
report_kind: qc
reviewer: qc-specialist
reviewer_index: 1
plan_id: "N/A (docs/plans/2026-11-12-long-stream-and-admin-fixes.md)"
verdict: "Request Changes"
generated_at: "2026-11-12"
---

# QC1 — targeted re-review（fix wave 2）

- Reviewer: @qc-specialist（reviewer_index=1，focus=架构一致性/可维护性）
- Previous report: `docs/plans/review/qc1.md`（Request Changes；W1–W3 + S1–S4）
- Re-review scope: `git diff 3152498..49d3aff`（上一轮 head → 当前 integration HEAD，13 文件 +830/−215）＋ 自己上一轮 7 条 findings 的闭环判定

## Scope

- Working branch (verified): `fix/long-task-admin` @ `/home/pop/AI/code/llm_gateway.wt/integration`
- Diff basis: `git diff 3152498..49d3aff`；直接接口核对面：`src/service/anthropic/stream.rs`、`src/service/responses/stream.rs`、`src/service/breaker.rs`、`migrations/0033_admin_source.sql`、`src/api/console.rs:1296-1400`、`src/store/users.rs`、`src/store/groups.rs`
- 复用证据：PM 侧 `cargo build` / `cargo test --bin llm_gateway` 315/315 / `web npm run build` 零错（未重跑，规则禁止）；L2 `taskA/taskB` 结论沿用上一轮口径（未重开原文）
- Files opened: 11（2 角色/方法 skill、qc1.md、qc3.md 相关段、config.rs、README、0034/0033 迁移、groups.rs、users.rs、state.rs、api/console.rs、proxy.rs 段）+ breaker.rs 段
- Truncated coverage: `web/` 侧本轮零改动（未复核）；`scripts/mock_upstream.py` 与 e2e 脚本未读；`cargo test` 未运行（规则禁止），单测**行为**仅静态推演；迁移在真实/存量库上的 `ADD CONSTRAINT` 校验未实测（无 DB 权限，且 PM 已声明存量违反行数为 0）；转换链路 `wrap_chat_stream_to_anthropic` / 转换 Responses 的整段未重读，只核对其错误帧构造点。
- Analysis methods: git-diff / read / grep（未运行 test/build/lint）

## Verdict: Needs-fix

Verdict enumeration 按 `references/qc-specialist/report-template.md`（frontmatter `Request Changes` = 正文 `Needs-fix`）。

## Summary

| Severity | Count |
|----------|-------|
| 🔴 Critical | 0 |
| 🟡 Warning | 1 |
| 🟢 Suggestion | 3 |
| ⚪ Unconfirmed | 0 |

上一轮 7 条：**闭环 5**（W1、W2、W3、S1、S2），**部分闭环 1**（S4：§4 已改、§6 未改），**闭环 1**（S3：改为规范 `event: error`，不再伪造 response 对象）。fix delta 引入 1 条新 Warning（熔断记账重复计数）与 3 条 Suggestion。

## Findings

### 🔴 Critical

`findings: []`

### 🟡 Warning

- **W1-new — 提交后错误帧的熔断失败与提交时的熔断成功对同一请求重复记账，目标故障供应商反而永不被隔离** — `src/service/proxy.rs:1526`（提交时 `record_success`）vs `src/service/proxy.rs:3115`（`SseErrorTail` 提交后 `Err` 记 `record_failure`），口径在 `src/service/breaker.rs:123-146`（`record_success`：`consec_failures = 0` @129、`window_requests += 1` @146）、`src/service/breaker.rs:149-175`（`record_failure`：`window_requests += 1; window_failures += 1`）。
  - 证据：
    - 同一 SSE 请求在 commit 时无条件记成功（`proxy.rs:1525-1526`，注释即「提交：熔断记成功」），随后上游断流/首字节/静默超时经 `SseErrorTail` 记失败（`proxy.rs:3115`）→ 一次请求进窗口 **2 次 requests、1 次 failure**。
    - 连续维度：每次请求都被 `record_success` 归零 → `consec_failures` 恒为 1，默认 `failure_threshold=4` 永不触发。
    - 错误率维度：`(1 failure)/(2 requests) = 0.5` 恒定 **低于默认 `failure_rate=0.6`**（`breaker.rs:41`）→ 永不触发。
    - 结论：对一个「2xx 后必断流」的供应商（正是本次新增计数要隔离的类别，qc3 F-001），熔断器在连续与错误率两个维度**都不会打开**；而对混合故障供应商，率值被系统性稀释约 2×（50% 真实失败率显示为 0.33），反而比改动前更难熔断。
    - 单测无法发现：`sse_error_tail_records_one_breaker_failure_per_stream`（`proxy.rs:4094-4131`）用裸 `Breaker` 只驱动 tail，没有经过 commit 处的 `record_success`，因此「只记一次」成立但「能否累积」未覆盖。
    - 范围不对称：转换链路（Anthropic/Responses conversion）同样在 `proxy.rs:1526` 记成功，但提交后 `Err` 由其自身收尾、不记失败 → 同一上游故障在转换路径上完全不计失败。计划 §7 新增行「仅中性帧的无限流 … 且错误已记熔断」的表述因此只对原生透传路径成立。
  - 最小修复方向（不扩范围）：让「成功」与「失败」在同一流内互斥——把 SSE 透传链路的 `record_success` 从 commit（`proxy.rs:1526`）移到流正常终止（`SseErrorTail` 的 `Poll::Ready(None)` 分支，`proxy.rs:3127-3130`，`ctx` 已持有 `breaker`），`Err` 分支保留 `record_failure`；这样每个请求对熔断器只产生一个信号，4 条断流即熔断，错误率也不再被稀释。若同时采纳 qc3 F-001 原文的 `is_timeout_error || !productive_seen` 收窄，需在 `SseErrorContext` 增加 `productive_seen` 并保持与成功信号互斥。
  - Verification: read `proxy.rs:1520-1534`（commit 记成功）+ `proxy.rs:3096-3135`（tail 记失败）+ `breaker.rs:149-175`（两计数器语义）+ `breaker.rs:36-48`（默认 `failure_threshold=4` @39 / `failure_rate=0.6` @41 / `min_requests=10` @42）+ 读该单测构造方式 / 单测未覆盖 commit 成功。
  - Expected vs observed: 期望「200 + 错误帧」的供应商在连续 N 次后进入熔断、被降级链跳过 vs 实测无论多少次都不会触发任一维度。

### 🟢 Suggestion

- **S1-new — README 给出的 504 复现配方落在自己刚声明的退化边界上，且取等号时 PRIME 一定先赢** — `README.md:139`（「需令 `FIRST_BYTE ≤ PRIME`（如 `GATEWAY_STREAM_FIRST_BYTE_TIMEOUT=2`）」）；计划同口径 `docs/plans/2026-11-12-long-stream-and-admin-fixes.md:52`、`:84`（`first_byte <= prime`）。
  - 证据：同一段前句与 `src/config.rs:158-170` 的启动告警都以 `PRIME ≥ FIRST_BYTE` 为退化条件（`>=`，取等即告警）；实现上 `PrimeDeadline::new`（`proxy.rs:1376`）在 2xx 返回后立即武装 `Sleep`，而 first-byte 计时器是 `StreamTimeoutStream` 首次被 poll 时才武装（`proxy.rs:91-93`，`wait: None` 懒建），因此 `FIRST_BYTE == PRIME` 时 PRIME 的到期时刻**严格更早**，`biased` 先轮询 deadline（`proxy.rs:181`）→ 仍走 `200 + 错误帧`。故 `=2`（2000ms）复现不出 504，只有严格更小值（如 `=1`）或 `PRIME=0` 才行。
  - 影响：这是 PM Acceptance 直接照抄的复现配方（计划 §6 判据含「真卡死 … 在窗口内 504」），按现文案会得到与预期相反的结果并误判修复失败。
  - 建议修复方向：三处把 `≤` / `<=` 改为 `<`，示例改 `GATEWAY_STREAM_FIRST_BYTE_TIMEOUT=1`。
  - Verification: read README:139 + config.rs 告警条件 + `proxy.rs:91-93`/`1373`/`172-179` 的武装顺序。

- **S2-new — 计划 §6 的单测判据仍保留被 §4 明确推翻的「有缓冲不提前提交」** — `docs/plans/2026-11-12-long-stream-and-admin-fixes.md:83`。
  - 证据：§4 W1.1（计划 `:50`）已改为「到期且 `is_sse` 时立即提交（缓冲为空或仅中性帧同样提交，缓冲原样前置）」，代码 `proxy.rs:1382-1397` 与之一致；但 §6 仍写「单测：priming deadline 到期提交（空缓冲 SSE）、**有缓冲不提前提交**、…」。这是上一轮 S4(a) 的另一半，属未被这次措辞同步覆盖的残留假命题。
  - 建议修复方向：`有缓冲不提前提交` → `中性帧缓冲到期同样提交（缓冲按原序前置）`；顺带把 §6「`cargo build` 零警告」对齐为「仅基线既有 dead_code 警告」（与 PM 实际证据一致）。
  - Verification: read 计划 §4 W1.1 与 §6 首条 / 对照 `proxy.rs:1382-1397`。

- **S3-new — §7 residual 对 Responses 方言的「方言内已无分歧」表述过强（错误码已对齐，终止事件名仍按路径不同）** — `docs/plans/2026-11-12-long-stream-and-admin-fixes.md:105`（「透明链路与转换链路错误构造器统一（类型已对齐 `stream_error`）| 方言内已无分歧」）。
  - 证据：Anthropic 侧确已完全一致——透传 `event: error` + `{"type":"error","error":{"type":"stream_error",…}}`（`proxy.rs:3062-3073`）与转换链路 `anthropic/stream.rs:630-638` 逐字段相同；Responses 侧两边**错误码**同为 `stream_error`，但**事件名/形状**仍不同：透传为规范的 `event: error` + 顶层 `{type,code,message}`（`proxy.rs:3074-3085`），转换链路仍为 `response.failed`（`responses/stream.rs:373-381` 经 `finalize` 输出完整 response 对象）。两者各自都符合规范，且透传侧改为 `event: error` 正是上一轮 S3 的正解（无权威 response id / 无累计 output 时不该伪造），因此**不作为 finding 重开**，只要求把 residual 的理由列改为「错误码已对齐；终止事件名仍按路径不同（`error` vs `response.failed`），构造器各自维护」，避免下一个维护者据此认为已无差异而删掉任一侧。
  - Verification: read `proxy.rs:3051-3085` / `anthropic/stream.rs:630-638` / `responses/stream.rs:371-381`。

## Revalidation（逐条）

| ID（qc1.md） | 判定 | 证据 / 说明 |
|---|---|---|
| **W1** 错误分类学收敛 | **Closed** | Anthropic 透传改 `stream_error`（`proxy.rs:3062-3073`），与 `anthropic/stream.rs:635` 逐字段一致；Responses 改 `event: error` + `{type,code,message}`（`proxy.rs:3074-3085`），删掉伪造的 `ResponsesResponseOut`/`ResponsesStreamEventOut`/`unix_now`（连带 `SystemTime` 导入一并移除，无死代码），与转换链路同为 `stream_error`。构造器未共享已确认为 §7 已登记 residual（理由列表述见 S3-new）。 |
| **W2** priming deadline 单测 | **Closed（附观察）** | `PrimeDeadline` 结构（`proxy.rs:148-187`）构造一次、跨 chunk 复用同一 `Sleep`（`Option<Pin<Box<Sleep>>>`，`disarm` 置 None）→ 「每次 chunk 新建 Sleep」的回归被抓。测试 `prime_deadline_preempts_synchronously_ready_chunk_flood`（`proxy.rs:4138-4166`）用 `stream::repeat_with` 同步就绪洪水 + 睡过窗口，第二/三条断言「到期必须先赢」+「已到期 deadline 不得被后续帧重置」确实能抓住去 `biased`（分支换序必失败）与计时器重建（重建后 Pending → 返回 Item 必失败）。观察：a) 去掉 `biased` 时该断言依赖 tokio 的随机分支顺序，属概率性失败而非确定性失败；b) 洪水流每次重新生成同一帧，因此不含「deadline 胜出时丢 chunk」的探针（正确性靠 `StreamExt::next` 取消安全语义 + `SseErrorTail`/`wrap_stream` 不预读来保证）。两者均不阻塞，后续可加 `tokio::time::pause()` 与 `iter([a,b])` 型流收紧。 |
| **W3** 配置边界 | **Closed** | 告警条件 `prime>0 && first_byte>0 && prime >= first_byte*1000`（`config.rs:158-166`，`saturating_mul` 防溢出）、文案明确写出退化后果与两条解法；`0` 语义自洽（PRIME=0 → 不限、first_byte 独立生效；FIRST_BYTE=0 → 禁用、不告警）；README `:139` 与 config 字段注释（`config.rs:52-62`）口径一致。唯一残留是复现配方取等号问题 → S1-new。 |
| **S1** `is_timeout_error` 注释 | **Closed** | 新注释（`proxy.rs:203-213`）与生产者一致：`StreamTimeoutStream` 产出 `Box::from(format!("upstream stream {first byte\|idle} timeout"))`（`proxy.rs:97-101`），`send_upstream` 的 header 超时是 `"upstream header timeout".to_string()`（`proxy.rs:2952`），并显式声明「文案匹配是必需判定」。核对无误。 |
| **S2** `0034` CHECK | **Closed（DB 行为未实测）** | `migrations/0034_admin_invariant.sql` 加 `CHECK (is_admin = COALESCE(admin_override, admin_ldap))`，`admin_ldap` 为 `NOT NULL DEFAULT FALSE`（0033）→ 表达式恒非 NULL，不存在「NULL 通过 CHECK」的漏洞。逐路径推演全部满足：`create_local_user`（`users.rs:65-66`，`is_admin=$4, admin_override=$4`）、`upsert_ldap_user`（`users.rs:89-100`，insert `$5/$5`、conflict `COALESCE(users.admin_override, EXCLUDED.admin_ldap)`）、`set_is_admin`（`users.rs:164`）、`reset_admin_source`（`users.rs:177`，`is_admin=admin_ldap`）、`groups.rs upsert_sync_user`（`groups.rs:129-135`，仅靠默认值，**不触碰** admin 列，ON CONFLICT 也只更新 email/display_name/ldn）→ 无误伤 groups/seed（seed 走 `create_local_user`，`grep "INTO users"` 全仓仅这 3 处写入）。`groups.rs:117-121` 新增注释已把该路径与不变式的关系写清。存量违反行数为 0 属实现者声明，本席位未连库复核。 |
| **S3** Responses 帧 | **Closed** | 见 W1 行：不再伪造 `response.id`/`output: []`，改为规范 `event: error`；单测断言 `v.get("response").is_none()`（`proxy.rs:4027`）锁定该回归。 |
| **S4** 计划措辞 | **部分闭环** | (a) §4 W1.1 ✓ 已改（计划 `:50`），但 §6 判据未同步 → S2-new；(b) §4 W2.1 回填 `source <> 'ldap'` ✓（计划 `:59`，与迁移一致）；(c) §4 W2.2 已从「两处」改为六条写路径 + 0034 兜底 ✓（计划 `:60-63`）。 |

### 其它 delta 抽查（仅直接影响契约面）

- **缓存增量同步（qc2 面）**：`AppState::sync_admin_id`（`state.rs:225-233`）写锁内单条增删，口径 `is_admin && status == 1` 与 `load_admin_ids` 一致；`update_user` 在写入后统一同步一次（`api/console.rs:1379`，含 status 变更 → 禁用即移出旁路），`set_is_admin` 的 `target.is_admin == grant` 短路不影响同步；LDAP 登录改走增量同步（`service/console.rs:49`），删除的 `admin_ids_changed` 与其单测无残留引用。全量 `reload_admin_ids` 仅剩启动/周期调用，语义未变。未见回归。
- **错误文本脱敏（qc2 面）**：`redact_secrets`（`proxy.rs:2801-2806`）对空 key 早退，不会出现 `str::replace("")` 的插字风险；客户端帧与日志同用脱敏文本。单测 `sse_error_tail_redacts_provider_key_from_client_frame` 覆盖。
- **`PrimeDeadline` 抽取后语义等价性**：非 SSE 到期仍走 `disarm()` + 继续等待（`proxy.rs:1393-1397`），SSE 到期 `committed = true` 后由 `while !committed` 退出、缓冲按 `iter(buffered).chain(stream)` 重放（`proxy.rs:1526-1529`）；`timed_out` 的 `StreamTimeoutStream` 已终止后不再被 poll。与上一轮判定一致，无新增忙循环/丢帧路径。

## Residual assessment

- **W1-new 建议本次修**：改动集中在 `proxy.rs`（把 1 处 `record_success` 从 commit 移到 tail 的 `Ready(None)` 分支，约 3 行）+ 给该单测补一条「经 `record_success` 后仍能累积到熔断」的断言。它不需要触碰已冻结的「deadline 后不切候选」决策，属本次代码自己引入的语义缺陷，不宜带残余交付。
- **S1-new / S2-new / S3-new**：均为 1–2 行文案改动，可随同一次 fix wave 处理；S1-new 优先级最高，因为计划 §6 的 504 判据是 PM Acceptance 的复现配方。
- 已登记 residual（计划 §7）未重开：中性帧无限流、错误构造器统一、目录管理员钉住、禁用不刷缓存、多候选 header 叠加、用户名大小写。仅要求修正 §7 中「方言内已无分歧」与「错误已记熔断」两处表述（分别见 S3-new 与 W1-new 的范围不对称说明）。

## Evidence check

- **上一轮 W1/W2/W3 的修复面自洽性**：`SseErrorContext` 字段（`dialect/provider_id/request_id/provider/model/provider_key/breaker`）在唯一构造点（`proxy.rs:1722-1735`）全部填齐，无 `unwrap`/占位；`wrap_stream` 签名同步（唯一调用点 `proxy.rs:1737`）；`responses::dto` 的导入移除后无残留使用（Responses 透传帧改为 `json!`）。未见编译期死代码风险，与 PM 的「仅 2 条基线既有 dead_code 警告」自洽。
- **未覆盖率声明**：单测**是否真的通过**、迁移在存量库上的 `ADD CONSTRAINT` 校验、e2e/前端均未运行/未读（见 Scope `Truncated coverage:`）。本轮判定基于 diff + read + grep，其中 W1-new 的结论（熔断永不触发）是**静态推演**（计数语义 + 默认阈值），不需要运行时即可成立；若 PM 需要运行时复核，最小复现是「同一 provider 连续 4 次 commit-then-break，观察 `breaker.allow` 是否转 false」。
- **预算**：本席位读取集中在 delta 与直接接口（11 个文件 + 3 段定向 grep/sed），未展开全仓扫描。
