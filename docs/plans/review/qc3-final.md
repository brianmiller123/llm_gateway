---
report_kind: qc
reviewer: qc-specialist-3
reviewer_index: 3
plan_id: "2026-11-12-long-stream-and-admin-fixes"
verdict: "Approve"
generated_at: "2026-11-12"
---

# Code Review Report — Final targeted re-review（F-001 净记账闭环确认）

## Reviewer Metadata
- Reviewer: @qc-specialist-3
- Runtime Agent ID: qc-specialist-3
- Runtime Model: deepseek/deepseek-v4-flash
- Review Perspective: Performance and reliability risk（熔断记账净额、热路径开销、锁与竞态）
- Report Timestamp: 2026-11-12T21:35:00+08:00

## Scope
- Changed scope: `49d3aff..eef9957` 的代码 delta —— `src/service/proxy.rs`（commit 结算、`SseErrorTail` EOF/Drop、脱敏、priming 日志、3 个新测试）、`src/service/breaker.rs`（`release_probe`）、`src/state.rs`（`admin_ids_gen` 代际守卫）、`README.md` / 计划 §3/§4.4/§6/§7 文案；复核对象是本席位上一轮 F-001（QC1 `W1-new` / QC2 `F1` 指出的净记账缺陷）。
- Reused evidence: 上两轮 `qc3.md` / `qc3-rereview.md` 的 F-002..F-004 结论、PM 声明 L1 证据（`cargo build` 通过 + 2 条基线 dead_code 警告、`cargo test --bin llm_gateway` 320/320）。本轮未复跑 build/test/lint/e2e。
- plan_id: `2026-11-12-long-stream-and-admin-fixes`（no-plan mode）
- Review range / Diff basis: `git diff 49d3aff..eef9957`（integration worktree，分支 `fix/long-task-admin`）
- Working branch (verified): `fix/long-task-admin`
- Review cwd (verified): `/home/pop/AI/code/llm_gateway.wt/integration`
- Files reviewed: 5（`proxy.rs`、`breaker.rs`、`state.rs`、`README.md`、计划文档）
- Truncated coverage: 转换链路（Anthropic / Responses）转换器的既有错误帧实现逐行细节未复核（不在本 delta，仅按控制流确认其分支提前返回）；`web/` 前端与 e2e 脚本改动不在本轮范围；计划 §5/§8 未复核。
- Commit range: `49d3aff..eef9957`（含 `5bd3a95` 熔断结算、`23b2c33` admin 代际守卫、两次 merge）
- Analysis methods: git-diff、read、grep、控制流与并发时序推理（未运行任何 test/build/lint）

## Revalidation（F-001 复核过程与结论）

上一轮我判 F-001 **Closed**，QC1/QC2 指出该判定不成立：提交处仍有 `record_success`，尾部的 `record_failure` 与之叠加 → 每请求 **2 requests / 1 failure**，连续计数被成功清零、错误率恒 0.5 < 0.6，两个维度都不熔断。**我接受该纠正**：上一轮只核对了"失败记一次"，未核对"成功是否也被记一次"，属我的漏判。本轮逐项复核 fix wave：

| # | 复核问题 | 证据 | 结论 |
|---|---|---|---|
| 1a | 每请求是否只产生一个信号 | `settle_commit_signal`（`proxy.rs:3099-3122`）在 `!anthropic_converted && !converted && content_type.contains("text/event-stream")` 时不记 success；该谓词与"会装上 `SseErrorTail`"的安装点（`proxy.rs:1734-1739`，`is_sse_body = content_type.contains("text/event-stream")`，`sse_error` 由它构造）严格对齐；`anthropic_converted`/`converted` 的两组转换分支均在 `:1553`/`:1623`/`:1649`/`:1700` 提前 `return`，不可能到达 `:1731` 的原生路径 | **成立** |
| 1b | 三条终止路径互斥且不重复 | `Poll::Ready(Some(Err))`（`:3143-3161`）先置 `finished = true` 再 `record_failure`；`Poll::Ready(None)`（`:3163-3169`）置 `finished` 后 `record_success`；下一次 `poll_next` 被 `if this.finished { return Ready(None) }` 短路，不再记账；`Drop`（`:3177-3190`）仅在 `!finished` 时 `release_probe`。Err→后续 EOF 不会补记 success，EOF→drop 不会重复释放；客户端断开无 Err → 只释放探测名额、不计数 | **成立**，互斥 |
| 1c | 净记账算术 | 旧行为：commit `record_success`（`consec_failures = 0`、`window_requests += 1`）+ tail `record_failure`（`window_requests += 1`、`window_failures += 1`）→ 2/1。新行为：透明 SSE 只有 tail 的 1 次失败或 1 次成功 → 1/1 或 1/0；"2xx 后必断流"渠道连续 4 条即达 `failure_threshold`，且错误率升到 1.0 | **成立**（`breaker.rs:123-176` 口径未变） |
| 1d | 热路径开销 | `settle_commit_signal` 只做 3 个 bool/`str` 判定，零分配；`poll_next` 新增的只有 EOF 分支里一次既有 mutex 取锁的 `record_success`（原先在 commit 处的同一次取锁被移走，净额不变）；逐 chunk 路径**无**新增分配、无计时器重建；`Drop` 仅在未结算流被丢弃时取一次锁（异常路径） | **零新增分配，无每帧开销** |
| 2a | 测试判别力（旧实现"恒放行"） | `committed_sse_stream_failures_open_breaker`（`:4210`）按"提交结算 + 尾部首个 Err"驱动 4 条流并断言 `!allow(7)`：① 旧的无条件 commit-success → 连续计数每次归零、错误率 0.5、`min_requests = 1000` 关掉率维度 → `allow` 为真 → 测试失败；② 只删 commit-success 而不加 EOF-success 也不影响本测；③ **若恢复成"commit success + tail failure"双重记账**（每条流 1 success + 1 failure，连续计数封顶 1）→ 同样恒放行 → 测试失败。**能同时抓住"漏记"和"重复记账"** | **判别力足够** |
| 2b | EOF 成功复位有判别力 | `committed_sse_normal_eof_resets_consecutive_failures`（`:4231`）：2 败 + 1 正常 EOF + 2 败，阈值 4。若 EOF 不补记 success → 累计 4 连败 → 熔断 → 断言失败；旧实现（commit 恒记 success）则恒放行而**通过**——本测主要锁新实现的 EOF 分支，不承担抓旧实现的责任（item 2a 已覆盖） | **成立（新实现守卫）** |
| 2c | 半开名额归还判别力 | `dropped_unsettled_sse_tail_releases_half_open_probe`（`:4262`）用 `failure_threshold = 1 / open_secs = 0 / min_requests = 1000`：`allow` 发探测（第二次 `allow` 拒绝）→ `pending` 流 20ms 超时后 drop → 再次 `allow` 断言放行。删掉 `Drop` 实现则探测名额永久占用 → 断言失败 | **判定力明确** |
| 2d | `drive_committed_sse` 是否走生产路径 | 调用的结算函数就是生产 `settle_commit_signal`（同函数，非拷贝），`SseErrorTail` 字段装配与 `wrap_stream`（`:3199-3212`，`error_ctx: sse_error`）一致；但**未**经 `wrap_stream`/`with_ping_keepalive`，且 content-type 字面量硬编码，因此"哪些请求装 tail"与"哪些不记 commit success"两侧的一致性只由生产代码结构保证，测试无法捕获该结构性事实被破坏 → 见 S2（非阻塞） | **基本成立，有测试形态缺口** |
| 3 | `admin_ids_gen` 竞争/热路径 | 代际只在两处出现（`state.rs:224-230` reload、`:243-244` sync）；请求侧旁路仍只读 `admin_ids` RwLock，**未进入任何 per-request 路径**。`apply_admin_id`（`:366-379`）在 `ids.write()` 内 `fetch_add`，`commit_admin_snapshot`（`:384-396`）在**同一把写锁内** `load` 比较 → 两个写者被同一锁串行化，不存在"读到旧代际再覆盖"的交错（这是本守卫成立的关键，实现正确）。权限变更/JIT 是低频路径，`reload` 仍是一次原子读 + 一次既有写锁，无新增锁竞争；丢弃陈旧快照时只多一条 `debug!` | **无热路径开销、无新锁竞争** |
| 4a | priming-deadline 日志与实际字段一致 | `proxy.rs:1388-1394` 新增 `request_id = %request_id, model = %model`，二者在该作用域可用（同处用于 `SseErrorContext` 构造），字段语义与 tail warn（`:3153-3160`）一致 | **一致** |
| 4b | 脱敏与实际泄漏面一致 | priming 传输错误体（`:1453-1461`）改为 `redact_secrets(format!(...), &provider_key)` 并同时用于 `last_failure` 客户端错误体与 `warn!`；`redact_secrets`（`:2818-2823`）对空 key 早退、否则字面替换 → 未新增第二条泄漏面 | **一致** |
| 4c | README 504 配方与实现一致 | README 改为 `FIRST_BYTE < PRIME`（如 `=1`）或 `PRIME=0`，取等时 PRIME 先提交。实现侧：`PrimeDeadline::new`（调用点 `:1376`）构造即 `tokio::time::sleep(d)`（`:159-161`），而 `StreamTimeoutStream` 的首字节 `Sleep` 在**首次 poll** 才武装（`:92-95`）→ PRIME deadline 严格早于 FIRST_BYTE，且 `next()` 用 `biased` 先轮询 deadline（`:172-190`）→ 取等也由 PRIME 提交。声明与实际行为一致 | **一致** |
| 4d | 计划 §3a / §2 / §7 与代码一致 | §3a"透明 SSE 不在 commit 记成功，EOF→success / Err→failure / 未结算 drop→归还探测名额"与 `:3099-3190` 逐条对应；§2 与 §6 的 `first_byte < prime` 已改；§7 新增"转换链提交后失败不计熔断"与代码结构一致（`sse_error` 仅原生路径构造） | **一致** |

**结论：F-001 本轮真正闭环**（净额 1 次信号/请求；旧实现的两个失败形态都被 2a 覆盖）。上一轮 5 项 Suggestion 中 S1/S2/S3（README 504 配方、§6"有缓冲不提前提交"、§7 Responses 方言措辞）均已在本次文档 delta 修正；S4（priming-deadline 日志）与 S5（转换链一致性，已登记 §7 residual）分别修复与登记。未发现本次 fix wave 引入新的阻断或高风险。

## Findings

### 🔴 Critical
（none）

### 🟡 Warning
（none）

### 🟢 Suggestion
- **S1 — 透明 SSE 的半开探测名额现在会占用整条流的时长，计划/README 未记录该取舍** -> 在计划 §7 补一行（现象 + 兜底 + owner/触发条件）：探测请求若是长流（默认 IDLE 300s，或客户端长连接到自然结束），`half_open_probe_in_flight` 在 EOF/断开前保持占用，该渠道对这些时长内的其它请求返回 `allow = false`；`filter_candidates` 在全禁时仍回退全量候选，故不是硬故障。这是"探测结果未知前不得释放"的正确代价（旧行为在 commit 即释放，正是 F-001 的一半），但值得与已登记的"仅中性帧无限流"residual 并列，避免验收时误判。
  - Verification: read + 控制流推理（`breaker.rs:88-105` `allow`、`:123-147` `record_success`、`:179-190` `release_probe`；`proxy.rs:1529-1541` commit 不再记 success、`:3163-3190` tail 结算/Drop）
  - Expected vs observed: 期望——半开探测在结果未知前保持占用，且该行为在计划中有可追踪记录；实际——行为正确，但只有代码注释，计划 §7/README 无对应行

- **S2 — 结算谓词与 tail 安装点的一致性没有测试或断言保护** -> 二选一：把"是否由 tail 结算"收敛成单一谓词函数（例如 `fn tail_settles(content_type, anthropic_converted, converted) -> bool` 由 `settle_commit_signal` 与 tail 构造处共用），或在原生路径加 `debug_assert!(sse_error.is_some() == !tail_settles)`。当前该一致性依赖"转换链分支全部提前 return"这一结构性事实，`drive_committed_sse` 把 content-type 与两个 flag 都硬编码，无法发现"未来某条转换分支改为落到原生路径"或"`is_sse_body` 判定被放宽"导致的重新双重记账。
  - Verification: read（`proxy.rs:1734-1739`、`:3099-3122`、`:1553`/`:1623`/`:1649`/`:1700` 的提前 return、`:3983-4000` 测试辅助）
  - Expected vs observed: 期望——"提交是否记 success"与"是否装 tail"由同一处判定派生，改动不可能单边漂移；实际——两处各自判定，靠控制流结构隐式对齐

- **S3 — 代际守卫的正确性依赖 `seen_gen` 必须在 DB 查询之前读取，该时序无测试锁定** -> 两个新单测（`state.rs:437-476`）直接调用 helper，隐含"先 load 再变更再 commit"的顺序，但没有覆盖 `reload_admin_ids` 的读取时序；若把 `load` 移到 `load_admin_ids` 之后，守卫会静默失效（查询期间发生的撤销仍会被陈旧快照覆盖），而现有测试全绿。建议补一个 sqlx 测试或对 `reload_admin_ids` 的时序加结构性注释 + 断言（例如把快照读取包成 `async` 闭包，测试里在闭包内推进代际，断言写回被拒绝）。
  - Verification: read（`state.rs:223-232`、`:384-396`、`:437-476`）
  - Expected vs observed: 期望——守卫的失效模式（读序颠倒）有可执行证据；实际——仅靠注释与代码顺序，测试对 helper 的调用顺序无法代表生产时序

### ⚪ Unconfirmed
（none）

## Source Trace
- F-001 / S1 / S2 — Source Type: git-diff + read + 控制流推理；Source Reference: `src/service/proxy.rs:1386-1394,1450-1461,1529-1541,1553,1623,1649,1700,1731-1739,3099-3190,3973-4000,4210-4300`；`src/service/breaker.rs:100-190`；Confidence: High
- S3 — Source Type: read + 测试覆盖判断；Source Reference: `src/state.rs:213-232,366-396,437-476`；Confidence: High
- 文档一致性 — Source Type: doc-rule + 实现对照；Source Reference: `README.md:139`、计划 `:52,:84,:102-108`；Confidence: High

## Summary
| Severity | Count |
|----------|-------|
| 🔴 Critical | 0 |
| 🟡 Warning | 0 |
| 🟢 Suggestion | 3 |
| ⚪ Unconfirmed | 0 |

**Verdict**: Approve —— F-001 已真正闭环：`settle_commit_signal` 使透明 SSE 在提交处不再记成功，`SseErrorTail` 的 EOF/Err/Drop 三条路径互斥结算（EOF→1 success、Err→1 failure、未结算 drop→仅 `release_probe`），净额恒为每请求 1 个信号；热路径零新增分配、半开探测无永久占位；三个新测试对"恒放行"与"重复记账"两种旧形态均有判别力。`admin_ids_gen` 在同一把写锁内推进与比较，无热路径开销与竞争；日志/README 的修正与实际行为一致。剩余 3 项 Suggestion 均为可追踪性/测试形态改进（一处取舍未入 §7、一处谓词一致性无断言、一处时序无测试），不影响契约成立，不阻塞合并。
