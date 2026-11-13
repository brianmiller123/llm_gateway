---
report_kind: qc
reviewer: qc-specialist-3
reviewer_index: 3
plan_id: "2026-11-12-long-stream-and-admin-fixes"
verdict: "Approve"
generated_at: "2026-11-12"
---

# Code Review Report — Re-review（F-001..F-004 复核）

## Reviewer Metadata
- Reviewer: @qc-specialist-3
- Runtime Agent ID: qc-specialist-3
- Runtime Model: deepseek/deepseek-v4-flash
- Review Perspective: Performance and reliability risk（热路径开销、熔断/超时语义、资源生命周期、缓存同步成本、迁移锁）
- Report Timestamp: 2026-11-12

## Scope
- Changed scope: `3152498..49d3aff` 中与本席位焦点及上一轮 findings 相关的 hunks：
  - `src/service/proxy.rs`：`PrimeDeadline`/`PrimePoll`、priming 循环、`SseErrorContext`/`SseErrorTail`/`wrap_stream`、`is_timeout_error` 注释、错误帧形状与新增单测
  - `src/state.rs`（`sync_admin_id`/`apply_admin_id`/`reload_admin_ids`）、`src/api/console.rs`（`update_user` 同步点、互斥判定）、`src/service/console.rs`（LDAP JIT 同步）、`src/store/users.rs`、`src/store/groups.rs`、`migrations/0034_admin_invariant.sql`、`src/config.rs`、`README.md`
  - 跨检查：`src/service/breaker.rs`（`record_failure`/`record_success` 语义）、`src/service/anthropic/stream.rs:635`、`src/service/responses/stream.rs:36/284-286`（转换链错误词表）、`scripts/test_conformance.py:244`
- Reused evidence: 上一轮 `docs/plans/review/qc3.md`（F-001..F-004 判定与行号）、PM 声明 L1 证据（`cargo build` 通过、`cargo test --bin llm_gateway` 315/315、`web npm run build` 零错）。本轮未复跑任何 build/test/lint/e2e。
- plan_id: `2026-11-12-long-stream-and-admin-fixes`
- Review range / Diff basis: `git diff 3152498..49d3aff`（上一次 QC head → 当前 integration HEAD；含 fix wave 提交 `e591e3a`/`a162a2b`/`5ca196a`/`8b173fe` 及两次 merge）
- Working branch (verified): `fix/long-task-admin`
- Review cwd (verified): `/home/pop/AI/code/llm_gateway.wt/integration`
- Files reviewed: 12（diff 全量读 8 个业务文件 + 定点读 breaker / 两条转换链 / conformance 脚本）
- Truncated coverage: 未审 `web/src/views/Users.vue`、`web/src/api/types.ts`、`migrations/0033_admin_source.sql` 正文、`docs/plans/review/qc1.md`/`qc2.md`/`taskA.md`/`taskB.md` 全文（仅按需 grep），以及 8b173fe 中与本席位焦点无关的 chat/anthropic 语义细节（属 qc1/qc2 覆盖面）。budget 内停止；本报告结论只覆盖上述已读范围。
- Commit range: `3152498..49d3aff`（与 Review range 相同）
- Analysis methods: git-diff、read、grep（无 test/build/lint/e2e）
- 报告载体说明：按 PM Assignment 写入独立文件 `qc3-rereview.md`；原 `qc3.md` 保留为上一轮记录，其 `Request Changes` 结论已被本文件取代。

## Revalidation（过程记录）

| ID | 上一轮要求 | 现状态 | 结论 |
|----|-----------|--------|------|
| F-001（🟡） | 提交后失败计入熔断；①②③ | `SseErrorTail` 在终止性 `Err` 分支调用 `ctx.breaker.record_failure(ctx.provider_id)`（`proxy.rs:3115`），`finished` 置位保证单次；正常 EOF/`None`/纯字节流不计数 | **Closed**（③ 按我的条件落为 §7:102 residual，见下） |
| F-002（🟢） | 日志补 `request_id`/`provider`/`model` | tail warn 已补齐三字段并脱敏（`proxy.rs:3119-3126`）；priming-deadline info 日志仍未补（`proxy.rs:1388-1393`） | **部分关闭** → 见 S4 |
| F-003（🟢） | 删死参数、补防饥饿单测 | `prime_deadline_commits` 已整体删除（逻辑内联）；`biased` deadline-first + 单 `Sleep` 复用保留；新增 `prime_deadline_preempts_synchronously_ready_chunk_flood`（`proxy.rs:4138`）与 `prime_deadline_disabled_reads_items_only` | **Closed** |
| F-004（🟢） | 计划/注释与实现一致 | §4.1 已改为「到期即提交，缓冲原样前置」，与代码一致；但 §4.5、§6:83 仍与实现相反 | **部分关闭** → 见 S1/S2/S3 |

F-001 逐项核对（本席位原要求）：
1. **提交后 `Err` 记一次失败** —— 是。`record_success` 在提交点（`proxy.rs:1526`）先执行，随后 tail 的 `record_failure`（`:3115`）递增 `consec_failures`，净效果为「该请求最终计一次失败」；提交后不再切候选，隔离只作用于后续请求（符合 §4.1 冻结决策）。
2. **正常 EOF / 客户端 drop 不记** —— 是。`Poll::Ready(None)` 直接 `finished` 返回；客户端断开时上游流被 drop、不产生 `Err`。单测 `sse_error_tail_records_one_breaker_failure_per_stream`（`:4094`）对 `None` 流与「有字节后 EOF」两条路径断言 `breaker.allow(7)` 仍为真。
3. **多次 failure 阈值行为** —— 测试用 `failure_threshold=2` 断言「第一条流后仍放行、第二条流后熔断」，同时证明「每条流只计一次」（否则首条流即会熔断）。
4. **是否引入每 chunk 额外开销** —— 无。`SseErrorTail` 仅 `Err` 分支变化；`PrimeDeadline::next` 每轮仍只构造一个 `Next` 栈上 future + 一次 `select!`，`Sleep` 在流创建时分配一次并跨轮复用，`Option::as_mut()` 判空为常数开销；无新增堆分配、无新增计时器注册。
5. **实现选择与我的建议不同但可接受** —— 落地的是「任何终止性 `Err` 都计失败」，而非我建议的「`is_timeout_error && !productive_seen`」。该口径更简单且覆盖面更广（传输错误同样计入）；有产出后尾部出错也计一次失败，但连续阈值（默认需 N 连败）与 1000 样本的错误率下限使其不会误伤偶发单次抖动。`productive_seen` 因此无需引入。

额外确认（Assignment 追加项）：
- **`SseErrorContext` 新字段的每请求生命周期/内存**：每 SSE 请求新增 4 个短 String（`request_id`/`provider`/`model`/`provider_key`）与 1 个 `Arc<Breaker>` clone，全部随流 drop 释放；结构体无 `Debug` derive，`provider_key` 只作脱敏 needle 使用，不进日志、不进客户端帧；无环引用（`Arc<Breaker>` 不持有流）。相对旧实现净增 2 个小分配/请求，可忽略。`redact_secrets` 对空 key 有早退保护（`proxy.rs:2801-2806`），空 key 渠道不会退化为逐字符替换。
- **`sync_admin_id` 高频权限变更成本**：一次 `RwLock::write` + `HashSet::insert/remove`，O(1)，**不查库、不 clone 整个集合**（`state.rs:225-241`、`:347-355`、`apply_admin_id`）；取代了旧路径每次变更一次全量 `reload_admin_ids()`（一次 `SELECT id FROM users WHERE is_admin AND status = 1` + 整集替换），高频场景成本下降。代理读侧只在建立/判定旁路时取读锁，无新增热点。
- **缓存与周期 reload 的竞态**：`reload()` 的 admin 节是「load → write」背靠背（`state.rs:276-279`），stale 窗口约一次查询时长；理论上仍可能与 PATCH 的增量写 last-writer-wins，但该竞态在改动前同样存在（旧路径也是整集替换），本次未放大，且撤销的暴露面上界仍是一个 reload 周期。不作为 finding。
- **迁移 0034 的影响面**：`ADD CONSTRAINT ... CHECK` 会以全表扫描校验现存行并短暂持有强锁（`users` 表小，可接受）；关键是五条写路径都满足等式，逐条核对：`create_local_user`（`users.rs:65-66`，`is_admin`/`admin_override` 同绑 `$4`）、`upsert_ldap_user`（`users.rs:89-98`，冲突分支 `is_admin = COALESCE(users.admin_override, EXCLUDED.admin_ldap)`）、`set_admin_override`（`users.rs:164`，两列同绑 `$1`）、`reset_admin_source`（`users.rs:177`，`admin_override=NULL, is_admin=admin_ldap`）、`upsert_sync_user`（`groups.rs:129-130`，`is_admin=FALSE` + 两列默认值）。**运行时不会因约束产生 500。**
- **错误帧契约变更（8b173fe）**：Anthropic 错误类型 `api_error → stream_error` 与转换链 `anthropic/stream.rs:635` 一致（对齐正确）；Responses 由伪造 `response.failed` 改为 `event: error` + `{type,code:stream_error,message}`，属 Responses 规范的合法流内错误事件，且「透传链没有权威 response id / 累计 output，伪造会与客户端已收到的流矛盾」的理由成立；`scripts/test_conformance.py:244` 断言的是「不产出 `response.failed`」，本次改动与该既有期望同向，**不破坏 conformance**。分支未发布，无对外兼容包袱。
- **非 SSE 提交后错误仍不计熔断**：`sse_error` 仅对 `text/event-stream` 构造，非 SSE 流 `Err` 仍交 hyper 且不记 breaker。属改动前既有边界（本席位 F-001 原就限定 SSE 透传链），仅记录不升格。

## Findings

### 🔴 Critical
（无）

### 🟡 Warning
（无）

### 🟢 Suggestion

- **S1 计划文本两处与实现相反：§4.5「不改动……熔断」、§6:83「有缓冲不提前提交」** -> `docs/plans/2026-11-12-long-stream-and-admin-fixes.md:55`（§4.5）改为「不改动：整流白名单、路由、非流路径与 `non_stream_timeout_secs`；熔断仅在提交后失败时补记一次 failure（见 §7 中性帧行）」；`:83` 删除「有缓冲不提前提交」，改为「deadline 到期提交且缓冲原样前置」并把新增的单测判据（`PrimeDeadline` 防饥饿 / `disarm`）写入该行
  - Verification: diff/read anchor——`proxy.rs:3115` 新增提交后 `record_failure`，`proxy.rs:1385-1400` 的到期分支对 `is_sse` 无条件 `committed = true`（不再看缓冲）；计划 §4.1（`:51`）已同步为「缓冲为空或仅中性帧同样提交」，与 §4.5/:83 自相矛盾
  - Expected vs observed: 期望——计划内部一致且与代码一致（熔断确有变更、到期提交不看缓冲）；实际——§4.5 声明熔断未改，§6 的验收判据仍要求一个代码不满足的行为（PM Acceptance 按 §6 执行会产生假失败）

- **S2 错误帧「帧形状」对齐的说法对 Responses 方言过claim（代码注释 + 计划 §7:103）** -> `proxy.rs:3048-3050` 改为「错误码词表与转换链路一致（`stream_error`）；Responses 帧形状有意不同——透传链无权威 response id / 累计 output，不伪造 `response.failed`」；计划 `:103` 的「方言内已无分歧」改为「错误码词表已对齐（`stream_error`），Responses 帧形状有意不同」
  - Verification: read anchor——透传 Responses 帧为 `event: error` + `{type:error,code:stream_error}`（`proxy.rs:3074-3081`）；转换链 `responses/stream.rs:36`（`EVENT_FAILED = "response.failed"`）与 `:284-286`（`finalize_inner` 发 `response.failed` + 完整 `response` 体）仍不同
  - Expected vs observed: 期望——注释/计划只声明真实成立的对齐度（错误码）；实际——注释称「帧形状……同词表」、计划称「方言内已无分歧」，而同一方言两条链发的 event 名不同

- **S3 计划 §7:105「禁用用户不即时刷新 `admin_ids`」已被本次实现覆盖，residual 行失效** -> 删除 `docs/plans/2026-11-12-long-stream-and-admin-fixes.md:105` 整行（或改写为「其它来源的 status 变更路径（当前不存在）需同样调用 `sync_admin_id`」）
  - Verification: grep anchor——`users.rs:117` 的 `set_status` 是唯一 status 写点，唯一调用方 `api/console.rs:1334`，其后紧接 `st.sync_admin_id(updated.id, updated.is_admin && updated.status == 1)`（`api/console.rs:1379`），禁用即移出代理旁路集合
  - Expected vs observed: 期望——residual 只保留仍然存在的缺口；实际——该行描述的缺口已在 fix wave 中关闭，继续留在 §7 会让后续计划重复投入

- **S4 F-002 残余：priming-deadline 的 info 日志仍缺 `request_id`/`model`** -> `proxy.rs:1388-1393` 补 `request_id = %request_id, model = %model`（两值在该作用域均可用，且已用于构造 `SseErrorContext`）
  - Verification: read anchor——该 `tracing::info!` 仅带 `provider`/`buffered_bytes`；同文件同类的提交后故障日志已带三字段（`proxy.rs:3119-3126`）
  - Expected vs observed: 期望——提交决策可在日志侧与请求/模型关联（我上一轮 F-002 的两处要求之一）；实际——只补了 tail warn 一处

- **S5 熔断记账仍只覆盖透传链；两条转换链提交后失败不计失败（非阻塞，与 F-001 同类）** -> 二选一：在转换链的流包装对终止性传输错误调用 `record_failure(provider.id)`，或在计划 §7 增一行「转换链提交后失败不计熔断」登记现象/owner/完成定义
  - Verification: grep anchor——本文件 `record_failure` 调用点普查为 `1231/1325/1341/1420/1441/1463/1520/1843/1936/1953/1963/3115`，除新增的 `3115` 外全在提交前的候选循环/取 key 路径；`record_transport_error` 在 `proxy.rs` 仅在注释中被引用（`:3050`），无任何流内调用点，故 Anthropic/Responses 转换链（`stream.rs:1476` 一带）提交后错误不经过 `SseErrorTail`
  - Expected vs observed: 期望——「200 + 错误帧的供应商被隔离」对三条链路同效（计划 §1「三条链路行为对齐」）；实际——只有透传链计入；属改动前既有边界，故不升格为 Warning

### ⚪ Unconfirmed
（无）

## Source Trace
- F-001（复核） — Source Type: git-diff + read + grep；Source Reference: `src/service/proxy.rs:3110-3126`、`:1526`、`:4094-4136`；`src/service/breaker.rs:149-174`；计划 `:102`；Confidence: High
- F-002（复核） — Source Type: read；Source Reference: `src/service/proxy.rs:3119-3126`（已补）、`:1388-1393`（未补）、`:2801-2806`（空 key 早退）；Confidence: High
- F-003（复核） — Source Type: git-diff + read；Source Reference: `src/service/proxy.rs:136-196`（`PrimeDeadline`/`PrimePoll`）、`:1385-1400`（内联后的到期分支）、`:4138-4172`（防饥饿与 `disarm` 单测）；Confidence: High
- F-004（复核） — Source Type: read + grep；Source Reference: 计划 `:51`（已同步）vs `:55`/`:83`/`:103`/`:105`（未同步）；`src/service/proxy.rs:3048-3050`；Confidence: High
- S5 — Source Type: grep；Source Reference: `src/service/proxy.rs` 全文件 `record_failure` 行号清单；`src/service/responses/stream.rs:371-380`；Confidence: High

## Evidence check
- **热路径（焦点 1）**：`PrimeDeadline` 把「每轮重建计时器 + 两条 `if let` 分支」收敛为「一个结构体 + 一次 `select!`」，`biased` 顺序（deadline 先）与上一轮一致，语义等价且注释与实现现在同源（单点定义，不再有 `prime_deadline_commits` 与循环双处描述）。
- **防饥饿单测的判别力**：`prime_deadline_preempts_synchronously_ready_chunk_flood` 使用 `repeat_with` 的**同步就绪无限流**，睡过窗口后断言 `Deadline` 胜出——若把分支顺序改为 item 优先，item 每轮 Ready 会使断言必然失败，故该测试确实锁定「item 分支不被优先 poll」；随后第二次调用再次返回 `Deadline` 锁定了「已到期的 `Sleep` 不被重建/重置」（若每轮重建，第二次会返回 `Item`）。`prime_deadline_disabled_reads_items_only` 覆盖 `None` 与 `disarm` 两条退化路径。上一轮 F-003 的「死参数」条件已消失（helper 删除，无死参数可言）。
- **取消安全**：deadline 胜出时 `let mut next = stream.next()` 从未被 poll（`biased` 保证），或仅 poll 至 Pending 后被丢弃；`StreamExt::next` 不在 poll 外消费流状态，字节不丢。SSE 到期分支 `committed = true; continue` 退出循环，到期 `Sleep` 不再被 poll，非 SSE 分支 `disarm()` 后回到无 deadline 等待，均无忙循环。
- **熔断链路（焦点 2）**：`record_success`（提交，`:1526`）与 `record_failure`（tail `:3115`）在同一请求内先后发生，`record_success` 在 map 不存在时直接返回、失败路径 `entry().or_default()` 必建条目，语义无死角；`record_failure` 只在 `poll_next` 内取一次 `std::sync::Mutex`、无 `await`、无 IO，不阻塞 runtime。
- **错误帧与脱敏（契约）**：客户端帧与日志使用同一个 `redact_secrets(...)` 结果；`provider_key` 不进 `Debug`、不进帧，`redact_secrets` 对空 needle 早退；Anthropic 词表与转换链一致，Responses 帧改用规范 `error` 事件，conformance 既有断言（`scripts/test_conformance.py:244`）不受影响。
- **管理员缓存（焦点 3）**：`sync_admin_id` 契约（「`is_admin` 必须是有效权限且 `status == 1` 的物化值」）在两个调用点都成立——`api/console.rs:1379` 用 `updated.is_admin && updated.status == 1`，`service/console.rs:49` 的 LDAP JIT 用 `user.is_admin && user.status == 1`；两处都在 DB 提交之后，撤销方向 fail-closed。JIT 路径删除了 `admin_ids_changed` 与额外 `find_by_username`，LDAP 登录省掉一次查询且不再可能因刷新失败影响登录。
- **迁移（焦点 4）**：见 Scope 中五条写路径核对；0034 无回填、只加约束，注释与 SQL 一致；`is_admin`/`admin_ldap`/`admin_override` 三列不变式在每条 SQL 内自洽，新增写路径会被约束拦截而非静默漂移。
- **L1 证据自洽性**：PM 声明的 build/test/web 证据未被本席位复跑，也未发现与 diff 冲突之处（新增单测签名与生产代码一致：`Breaker::new_with`、`PrimeDeadline::new/disarm/next`、`SseErrorContext` 字段齐全）。上一轮「默认配置下 2xx 卡死表现为 `200 + 错误帧`、复现 504 需 `first_byte <= prime`」的配置前提已由计划 §4.2（`:52`）与 README 补充，闭合。

## Summary
| Severity | Count |
|----------|-------|
| 🔴 Critical | 0 |
| 🟡 Warning | 0 |
| 🟢 Suggestion | 5 |
| ⚪ Unconfirmed | 0 |

**Verdict**: Approve —— 上一轮唯一的 Warning（F-001）已闭环：提交后终止性 `Err` 记一次熔断失败、正常 EOF/客户端断开不记、阈值行为有判别力单测，且未引入每 chunk 开销；F-002/F-003 的主体已修（F-003 完全关闭），F-004 的计划 §4.1 已与实现对齐。剩余 5 项 Suggestion 均为一行级计划/注释措辞与一处转换链一致性建议，不阻塞合并：其中 S1/S2/S3 建议在 PM Acceptance 前修掉，避免 §6 判据与代码相反导致验收假失败。
