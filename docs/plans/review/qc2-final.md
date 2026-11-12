---
report_kind: qc
reviewer: qc-specialist-2
reviewer_index: 2
plan_id: "2026-11-12-long-stream-and-admin-fixes"
verdict: "Approve"
generated_at: "2026-11-12"
---

# Code Review Report（QC2 — final targeted re-review）

## Reviewer Metadata

- Reviewer: @qc-specialist-2（`role_id=qc-specialist-2`, `reviewer_index=2`）
- Runtime Agent ID: QC2Final
- Runtime Model: deepseek/deepseek-v4-flash
- Review Perspective: **安全与正确性**（授权/并发原子性、熔断记账、错误面信息暴露）
- Report Timestamp: 2026-11-12

## Scope

- Changed scope: fix wave 3 delta 与本席上轮 owned findings：`F1`（提交后失败不计熔断）、`F2`（reload 快照竞态）、`F3`（计划 §7/§4.4 措辞漂移）、`F4`（pre-commit 失败体脱敏），以及该 delta 是否引入新的阻断/高风险。
- Review range / Diff basis: `git diff 49d3aff..eef9957`（5 文件，+288 / −29：`src/service/proxy.rs`、`src/service/breaker.rs`、`src/state.rs`、`README.md`、`docs/plans/2026-11-12-long-stream-and-admin-fixes.md`）；文档 delta `eef9957..5637441`（仅 3 个 QC 复核报告，无规范面改动）。逐 commit：`23b2c33`（generation 守卫）、`5bd3a95`（净记账 + `release_probe`）、`5388825` / `eef9957`（合并点）、`5637441`（报告落盘）。
- plan_id: `2026-11-12-long-stream-and-admin-fixes`（no-plan mode，产物 `docs/plans/`）。
- Working branch（verified）: `fix/long-task-admin`；`git rev-parse --show-toplevel` = `/home/pop/AI/code/llm_gateway.wt/integration`；`HEAD` = `5637441`。
- Reused evidence: 上轮 `qc2-rereview.md` 的计数推演、L2 `taskA.md`/`taskB.md`、PM 已核 L1（`cargo build` 通过、`cargo test --bin llm_gateway` 320/320）——本轮**未重跑**任何 build/test/lint/静态分析。
- Files reviewed: 5（`src/service/proxy.rs`、`src/service/breaker.rs`、`src/state.rs`、`README.md`、主计划）。
- Analysis methods: `git diff` + read + grep + 手工推理（熔断计数推演、锁序/时序枚举）。
- Truncated coverage: 未重审 QC1/QC3 所属条目、`web/src/**`、`src/service/anthropic|responses/stream` 转换器内部实现；本轮判定不依赖这些范围。

## Findings

### 🔴 Critical

无（`findings: []`）。

### 🟡 Warning

无（`findings: []`）。

### 🟢 Suggestion

- **S-1（预存、非本 delta 引入）** pre-commit 客户端错误体中「上游自报文本」路径未脱敏。F4 的修复只覆盖本网关生成的传输出错串（`proxy.rs:1456`，reqwest `Display` 含 URL/key）；`first_chunk_error`（`:2507-2578`）与 `PrimeSignal::Error`（`:1435-1447`）把**上游返回的 message** 原样交给 `upstream_error_envelope`（`:1416-1432`）后进入客户端 body / `last_failure`。若上游在错误消息里回显渠道 key（如 "Incorrect API key provided: sk-…"）则会泄漏。修复：两处同样套 `redact_secrets(&msg, &provider_key)`（约 2 行 + 1 条断言）。
  - Verification: read + diff anchor（`proxy.rs:1416-1447,1450-1462,2507-2578,2818-2821`）
  - Expected vs observed: 期望所有客户端可见错误文本统一脱敏 vs 观察为仅传输出错串脱敏（上游自报文本未处理）
- **S-2（预存、非阻塞）** `release_probe` 的探测名额归属未绑定到具体请求。`breaker.rs:176-187` 只按 `provider_id` 清 flag；在「全部候选熔断 → 回退全量候选」路径（`proxy.rs:1216` `check_breaker=false` 旁路，`filter_candidates` 返回 `None` 时用全量候选）上，非探测请求的 `SseErrorTail` 若未结算被 drop，会清掉另一在途探测请求的名额，允许 >1 并发探测。它不改变开/闭判定：`release_probe` 从不触碰 `open_until`/`half_open_successes`，也不抑制任何失败信号。
  - Verification: read + 双路径比对（`breaker.rs:104-187`；`proxy.rs:1216,3177-3190`）
  - Expected vs observed: 期望半开「单探测名额」严格 vs 观察为可被并行 drop 提前释放（影响仅限探测并发度）
- **S-3（预存、非本 delta 引入）** 提交前客户端断开会永久占用半开探测名额。`allow()` 在第 1216 行按候选取得探测名额后，若请求在 priming / header 等待阶段被客户端断开（future 被 drop），既无 `record_success` 也无 `record_failure`；`release_probe` 只挂在 post-commit 的 `SseErrorTail::Drop`，因此该渠道此后 `allow()` 恒 false（其他渠道健康时不会走全量回退自愈）。建议在外层请求 drop 守卫归还名额（或让 `allow` 返回一次性 token）。本次 delta 未引入该缺口（修复前同样存在），但 `release_probe` 的“不会永久占位”注释目前只对 post-commit 成立。
  - Verification: read + 时序枚举（`breaker.rs:104-187`；`proxy.rs:1216,3177-3190`；`grep release_probe` 仅 3 处：定义、tail drop、测试）
  - Expected vs observed: 期望任意中断都能归还探测名额 vs 观察为仅 post-commit drop 归还

### ⚪ Unconfirmed

无（`findings: []`；证据通道完整，未出现不可读/不可复现范围）。

## F1–F4 闭环确认

| ID | 上轮结论 | 本轮证据 | 闭环 |
|---|---|---|---|
| **F1** | 提交处 `record_success` 与流内 `record_failure` 同请求内互相抵消，默认参数下连续 4 条「2xx 后卡死」两个维度都不熔断 | ① `settle_commit_signal`（`proxy.rs:3100-3120`）对透明 SSE **不再**记 success；调用点 `:1537`；② 唯一装上 `SseErrorTail` 的路径就是该条件：`anthropic_converted`/`converted` 两条分支在到达透传段前已 return（`:1563-1723`），故提交处两个 flag 恒为 false → `content_type.contains("text/event-stream")` ≡ `is_sse_body`（`:1739`），条件精确匹配，无双记/漏记；③ `SseErrorTail`：首个 `Err` 记一次 failure（`:3140-3157`，`finished` 置位后互斥），EOF 记一次 success（`:3162-3171`），未结算 drop 只 `release_probe`（`:3177-3190`）；④ 数值复核（默认 `failure_threshold=4`、`min_requests=10`、`rate=0.6`）：每个坏请求 1 个 failure 样本，`consec_failures` 1→2→3→4 → 第 4 条后开闸，错误率维度也由 0.5 抬到 1.0；⑤ 回归测试按真实序列（`drive_committed_sse` = 提交结算 + tail，`:3981-3996`）覆盖：`committed_sse_stream_failures_open_breaker`（4 连败熔断）、`committed_sse_normal_eof_resets_consecutive_failures`（EOF 复位）、`dropped_unsettled_sse_tail_releases_half_open_probe`（drop 释放且不计数）；`test_breaker(4)` 用默认连续阈值（`min_requests=1000` 隔离错误率维度，合理） | **已闭环** |
| **F1 追问：新的漏算** | — | 提交前的失败路径仍走既有 `record_failure`（`:1422/:1443/:1470`）；**pre-commit 客户端断开**不产生信号且不释放探测名额 —— 预存缺口，非本 delta 引入，见 S-3；post-commit 客户端断开按设计不计（无 `Err` 可归因），只归还名额。**无新增漏算** | 无新阻断 |
| **F2** | reload 读快照后整份覆盖可回滚已提交的撤销（≤1 个 reload 周期越权窗口） | `seen_gen` 锁外读（`state.rs:225`）→ DB 快照 → `commit_admin_snapshot`（`:384-395`）在 **ids 写锁内**比较代际；`apply_admin_id`（`:364-378`）在**同一写锁内** `fetch_add(1)`。四类交错逐一枚举：gen 变更 → 丢弃快照（缓存保留增量真值）；快照先写回、增量后应用 → 终态正确；sync 的 DB 提交必先于 gen bump，因此 gen 未变时快照必已包含该变更；启动/周期 reload 由单任务串行 `await`（`state.rs:341-360`）+ 启动期一次 `init`，无并发 reload 互相覆盖。`u64` 回绕需 2^64 次写，无现实性。新增 `stale_admin_snapshot_is_rejected_after_incremental_revoke` / `fresh_admin_snapshot_is_committed` 覆盖守卫两向 | **已闭环** |
| **F3** | §7「禁用用户不即时刷新 admin_ids」已失效、§7「错误已记熔断」在 F1 修复前不成立、§4.4 表述过强 | 计划 delta：该行已删除；§7 改为「终止性错误按净记账进熔断」，并新增「转换链（Anthropic/Responses）提交后传输失败不计熔断」与构造器统一行（与代码一致：转换链在提交处仍记 success、不装 tail）；§4.4 补「reload 用版本号守卫，在途快照不得覆盖并发变更」；§6 判据补净记账与陈旧快照；README/计划统一为 `FIRST_BYTE < PRIME`（取等时 `PRIME` 的 `Sleep` 在 `PrimeDeadline::new`（`:1376`）即创建，`FIRST_BYTE` 的计时器在首次 poll 流时（`:88-90`）才创建，且 `select! biased` 先轮询 deadline → 取等时 PRIME 先提交，措辞成立） | **已闭环，无新漂移** |
| **F4** | pre-commit 传输错误体直出未脱敏的 `e` | `proxy.rs:1450-1462`：同一脱敏串同时用于客户端 body 与日志；`redact_secrets`（`:2818-2821`）字面替换覆盖 URL/key 中的渠道密钥（空 key 短路）；分类判定用未脱敏的 `e`（`is_timeout_error(e.as_ref())`）→ **504/502 语义未变**。相邻未覆盖类（上游自报 message）见 S-1，预存且非本 delta 引入 | **已闭环** |

## 安全面：`release_probe` 能否绕过熔断

- **不能开启/闭合熔断**：`release_probe` 只清 `half_open_probe_in_flight`，不触碰 `open_until`、`half_open_successes`、窗口计数；只有 `record_failure`/`record_success` 能改变状态，二者均未被绕过。
- **客户端断开模式**：断开 = 无 `Err` = 无可归因的失败，属设计内「不惩罚渠道」；它不会掩盖任何已产生的失败信号（`Err` 路径先结算再发帧，`finished` 保证一次）。
- 可滥用面仅为探测名额的占用/释放：并行 drop 可提前释放他人名额（S-2，>1 并发探测），pre-commit 断连可永久占用名额（S-3，fail-closed 可用性损失）。两者都不把 open 渠道置回 closed，也不稀释真实失败的计数，**不构成熔断绕过**。

## Revalidation

- 复核对象：上轮 owned findings `F1`/`F2`/`F3`/`F4` 与 fix wave 3 delta（`49d3aff..eef9957`）+ 文档 delta（`eef9957..5637441`）。
- F1 → closed（净记账 + `release_probe` + 真实序列回归测试）；F2 → closed（锁内代际守卫 + 两向单测）；F3 → closed（三处措辞已改，无新漂移）；F4 → closed（脱敏与分类解耦），相邻预存类转 S-1。
- 未重跑任何 build/test/lint；未重审 QC1/QC3 条目、前端与转换器内部（见 `Truncated coverage`）。未重开已登记 residual。

## Source Trace

| ID | Source Type | Source Reference | Confidence |
|---|---|---|---|
| F1 | git-diff + read + arithmetic | `proxy.rs:1537,3100-3190,3981-4005,4200-4290`；`breaker.rs:104-187` | High |
| F2 | git-diff + read + ordering analysis | `state.rs:225,243,364-395,341-360`；`api/console.rs:1379`；`service/console.rs:49` | High |
| F3 | git-diff + doc-rule | `docs/plans/2026-11-12-long-stream-and-admin-fixes.md` §3a/§4.4/§6/§7；`README.md:139`；`proxy.rs:1361,1376,88-90` | High |
| F4 | git-diff + read | `proxy.rs:1450-1462,2818-2821`；分类 `is_timeout_error(e.as_ref())` | High |
| S-1 | read + grep | `proxy.rs:1416-1447,2507-2578` | Medium |
| S-2 | read + dual-path | `breaker.rs:176-187`；`proxy.rs:1216,3177-3190` | Medium |
| S-3 | read + timing enumeration | `breaker.rs:104-187`；`proxy.rs:1216,3177-3190`；`release_probe` 全仓 3 处调用 | Medium |

## Summary

| Severity | Count |
|----------|-------|
| 🔴 Critical | 0 |
| 🟡 Warning | 0 |
| 🟢 Suggestion | 3 |
| ⚪ Unconfirmed | 0 |

**Verdict**: Approve
