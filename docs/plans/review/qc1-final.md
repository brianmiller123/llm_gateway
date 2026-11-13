---
report_kind: qc
reviewer: qc-specialist
reviewer_index: 1
plan_id: "N/A (docs/plans/2026-11-12-long-stream-and-admin-fixes.md)"
verdict: "Approve"
generated_at: "2026-11-12"
---

# QC1 — final targeted review（fix wave 3 闭环确认）

- Reviewer: @qc-specialist（reviewer_index=1，focus=架构一致性/可维护性）
- Previous report: `docs/plans/review/qc1-rereview.md`（Needs-fix；W1-new + S1-new/S2-new/S3-new）
- Reviewed delta: `git diff 49d3aff..eef9957`（代码：`src/service/proxy.rs` +173/−29、`src/service/state.rs` +109/−11、`src/service/breaker.rs` +12、`README.md`、计划）；`eef9957..5637441` 仅新增三份 re-review 报告（本席位自己写入的产物，不含代码）

## Scope

- Working branch (verified): `fix/long-task-admin` @ `/home/pop/AI/code/llm_gateway.wt/integration`
- Diff basis: `git diff 49d3aff..eef9957`；直接接口核对面：`proxy.rs` 提交点（1215–1240 / 1340–1360 / 1470–1610 / 1700–1800）、`SseErrorTail`（3117–3193）、`settle_commit_signal`（3103–3120）、`breaker.rs:109–190`、`state.rs` 代际守卫（216–260 / 361–400 / 测试段）、`README.md:139`、`src/config.rs:152–174`
- 复用证据：PM 侧 `cargo build` 通过（仅 2 条基线既有 dead_code 警告）/ `cargo test --bin llm_gateway` 320/320（未重跑，席位规则禁止）
- Files opened: 8（`qc1-rereview.md`、`proxy.rs` 五段、`breaker.rs`、`state.rs` diff、`config.rs` 段、`README.md`、计划 diff）
- Truncated coverage: `web/` 侧零改动（未复核）；`scripts/mock_upstream.py` / e2e / `scripts/test_conformance.py` 未读未跑；`migrations/` 本轮零改动（未复核，沿用上一轮结论）；单测**执行**与 L4 真实链路未验证（非本席位职责）；`proxy.rs` 中 `redact_secrets` 本体（2818）只核对签名未重读实现（上一轮已审）。
- Analysis methods: git-diff / read / grep（未运行 test/build/lint）

## Verdict: **Approve**

Verdict enumeration 按 `references/qc-specialist/report-template.md`。

## Summary

| Severity | Count |
|----------|-------|
| 🔴 Critical | 0 |
| 🟡 Warning | 0 |
| 🟢 Suggestion | 3 |
| ⚪ Unconfirmed | 0 |

上一轮 4 条：**全部闭环**（W1-new、S1-new、S2-new、S3-new）。本轮 delta 未引入新的阻断/高风险；新增 3 条非阻塞 Suggestion（1 条为 S2-new 副项未采纳的沿用）。

## Findings

### 🔴 Critical

`findings: []`

### 🟡 Warning

`findings: []`

### 🟢 Suggestion

- **S1-final — `settle_commit_signal` 内联了「是否装 tail」的判定，与该判定在调用点的真实来源重复，未来改一处会静默恢复双重记账** — `src/service/proxy.rs:3106-3120`（helper 内 `content_type.contains("text/event-stream")` + `!anthropic_converted && !converted`）vs `proxy.rs:1353`（`is_sse`）、`proxy.rs:1734-1739`（`is_sse_body.then(...)`，真正决定装 tail 的地方）。
  - 现状**正确**：`content_type` 在 `1331` 绑定后到 `1734` 无重新赋值（全仓 `content_type =` 仅 1331/1959/2155/2328，均在各自作用域），且到达 `wrap_stream` 的分支必然已排除 `anthropic_converted`（1557 提前返回）与 `converted`（1700 提前返回），因此 helper 的 `tail_settles` 与 `sse_error.is_some()` **当前逐值等价**。
  - 维护性风险：等价关系不在类型/表达式上表达，靠两处 predicate 手工同步；将来新增第三种转换链路或调整 SSE 判定（如按 `header` 而非 `content_type`）时，提交点会重新记 success 而 tail 又记一次，W1-new 的缺陷会以「无声」方式回归（单测 `committed_sse_*` 直接调 helper，抓不到该脱钩）。
  - 最小改法：调用点已有同源的 `is_sse`（1353，`1353–1537` 之间无遮蔽），把 helper 签名收窄为 `settle_commit_signal(breaker, provider_id, tail_settles: bool)`，在 1537 处传 `is_sse && !anthropic_converted && !converted`——判定只留一处，且与 `will_stream` 的表达式同屏。
  - Verification: read `proxy.rs:1353`、`1530-1545`、`1734-1739`、`3106-3120`；grep `content_type =`.

- **S2-final — `release_probe` 不是「名额归属」语义：任何未结算 drop 都会清掉该渠道当前的探测名额，即使该请求并未持有它** — `src/service/breaker.rs:176-186`、`src/service/proxy.rs:3178-3191`。
  - 触发条件很窄：`check_breaker = !breaker_bypassed && candidates.len() > 1`（`proxy.rs:1025`），单候选旁路 / `breaker_bypassed` 请求不调用 `allow()`、不持有名额；若同渠道此时正有探测请求 B 持有名额（由另一次多候选请求经 `allow()` 取得），B 的流未结算被 drop 时会把 A 的名额清掉。之后 A 正常 EOF 的 `record_success` 看到 `half_open_probe_in_flight == false`，走 `open_until = None` 分支（`breaker.rs:129-149`）——**一次成功即闭合**，绕开 `success_threshold=2` 的抖动防护。
  - 影响面：仅半开态的额外震荡（多一次放行），不涉及授权/计费/数据正确性；`release_probe` 的核心目标（避免名额永久占位）达成，因此**不阻塞**。
  - 方向（后续）：`allow` 返回探测 token / guard（RAII 归还在 drop），或注释中显式登记「名额非独占、跨请求可能被释放」这一已知边界，避免后来者按「所有权」直觉改动。若要最小落地，先在函数 doc 注明适用前提（调用方持有名额）。
  - Verification: read `breaker.rs:109-190`（`allow` 置位、`record_success`/`record_failure` 对 flag 的处理、`release_probe` 的 `get_mut` + flag 守卫）；`proxy.rs:1025`、`1216`。

- **S3-final（S2-new 副项沿用）— 计划 §6 仍写 `cargo build` 零警告，与本轮证据（2 条基线既有 dead_code 警告）不一致** — `docs/plans/2026-11-12-long-stream-and-admin-fixes.md:86`。
  - 主项（`有缓冲不提前提交` 假命题）已修正 → 见 Revalidation；仅「零警告」这一判据仍按严格口径书写。基线告警不是本次引入，但按字面判据 PM Acceptance 会得到「不达标」的伪阴性。
  - 建议改为「`cargo build` 无**新增**警告（仅基线既有 2 条 dead_code）」。纯文档一行，可随任意后续提交处理。
  - Verification: read 计划 `:86` vs Shared facts（PM 证据）。

## Revalidation（逐条）

| ID（qc1-rereview） | 判定 | 证据 |
|---|---|---|
| **W1-new** 熔断净记账 | **Closed** | 见下三小节。 |
| **S1-new** 504 配方 | **Closed** | `README.md:139` 改为 `FIRST_BYTE < PRIME`（示例 `=1`）并补「取等时 `PRIME` 到期仍严格更早，会先提交」；与 `config.rs:158-166` 告警条件 `prime_ms >= first_byte_secs*1000`（取等即告警）同一口径；与实现顺序一致（`PrimeDeadline` 在 2xx 后武装、first-byte 计时器懒建、`biased` 先轮询 deadline）。计划 §4 W1.2 与 §6 e2e 判据同步为 `first_byte < prime`。三处（README / config 告警 / 计划）互相自洽。 |
| **S2-new** 计划 §6 判据 | **Closed（主项）** | §6 首条改为「空缓冲/中性帧 SSE）、**非 SSE** 不提前提交」，与 §4 W1.1「缓冲为空或仅中性帧同样提交」一致，假命题已消除；同条另补「透传流 Err→方言错误帧（脱敏、无 Err 泄漏）、熔断按真实序列每次请求只记一个信号」。副项「零警告」未采纳 → S3-final。 |
| **S3-new** §7 residual 措辞 | **Closed** | §7 两行重写：Responses 侧改为「错误码已对齐 `stream_error`，终止事件名不同：透传 `event: error` vs 转换 `response.failed`」；「且错误已记熔断」改为「终止性错误按净记账进熔断」。另新增独立 residual 行「转换链提交后传输失败不计熔断（既有行为，本次只覆盖透明 SSE）」，把 W1-new 指出的范围不对称明确留档。同时删除「禁用用户不即时刷新 `admin_ids`」行——该缺口已由 `update_user` 写入后同步（`api/console.rs:1379`）关闭，删除合理。 |

### W1-new（熔断净记账）细节核验

1. **透明 SSE 提交点不再记 success**：`proxy.rs:1537` 调用 `settle_commit_signal`；仅当 `tail_settles = !anthropic_converted && !converted && content_type.contains("text/event-stream")` 为假时 `record_success`。到达 `wrap_stream`/`sse_error=Some(ctx)`（`1734-1739`）的分支必然满足 `tail_settles == true`（两个转换分支分别在 1557 / 1700 提前 return）→ 透明 SSE 路径提交处**不再产生信号**。转换链路（Anthropic 1557 / Responses 1700）与非 SSE 透传（`is_sse=false` → `record_success`，`!will_stream` 分支另有 1554 的 success 保持不变）语义与改动前一致。
2. **tail 三条路径互斥且各一次**（`proxy.rs:3133-3193`）：
   - `Err`：置 `finished = true` → `record_failure` **一次**；后续 poll 由顶部 `if this.finished` 短路为 `Ready(None)`，**不会**再进 EOF 分支；
   - `None`：置 `finished = true` → 有 `error_ctx` 时 `record_success` **一次**；
   - `Drop`：`finished == true` 直接 return；仅未结算时 `release_probe`，**不计数**。
   三者由单一 `finished` 标志串行化，不存在同一次请求两个信号的路径。`error_ctx == None`（非 SSE）时 EOF/Err 均不计，提交处的 success 为唯一信号 → 仍是一次。
3. **`release_probe` 仅清标志**（`breaker.rs:176-186`）：`states.get_mut`（不建条目）+ `if half_open_probe_in_flight` 守卫，不改任何计数器/`open_until`。满足「不计数、只归还名额」。
4. **三个新测试的自洽性与「失败先于修复」强度**：
   - `drive_committed_sse`（`3981-3995`）复刻真实顺序：先 `settle_commit_signal(..., false,false,"text/event-stream")` 再驱动 tail 到终止——路径与生产一致（提交前不记、终止时记）。
   - `committed_sse_stream_failures_open_breaker`（`4204-4222`）：`test_breaker(4)` 将 `min_requests` 抬到 1000，**只留连续维度**，隔离干净。旧实现（提交处无条件 `record_success`）每轮把 `consec_failures` 归零 → 4 轮后仍放行 → 断言 `!allow` **必然失败**；同时排除错误率维度干扰（旧实现率恒 0.5）。断言强度合格。
   - `committed_sse_normal_eof_resets_consecutive_failures`（`4226-4247`）：2 败 + 1 正常 EOF + 2 败不达阈值。若把 EOF 的 `record_success` 删掉 → 累计 4 连续失败 → 误熔断 → 断言失败；该测试锁定新增的 EOF 分支。
   - `dropped_unsettled_sse_tail_releases_half_open_probe`（`4250-4281`）：`open_secs=0` + `failure_threshold=1` 构造半开；先断言 `allow` 拿到名额、`!allow` 名额占用（证明 setup 真实），再 drop 未结算 tail 并断言名额归还。回归前（无 `Drop` impl）名额永久占用 → 末条断言失败。
   - 观察（非 finding）：测试未断言 `window_requests` 精确为 1/请求（计数器未暴露）；但「一次请求一个信号」由 `finished` 单标志 + 分支结构保证，三个测试分别覆盖了 success/failure/drop 三类终止。若将来想锁死「不多计 requests」，需要一个可观测的计数入口，属可选加强。

## 新抽象 / 重复评估

- `settle_commit_signal`（自由函数，5 参数）职责边界清晰：**只回答「提交时要不要记 success」**，不碰 tail、不碰计时；命名与文档准确描述了「谁结算」。问题不在职责分裂，而在 predicate 来源重复 → S1-final（建议改为单一布尔入参）。
- `admin_ids_gen`（`state.rs:72-75`）+ `commit_admin_snapshot`（`382-397`）：代际写入点在 `apply_admin_id` 的写锁内（`370-372`），写回判定在 `commit_admin_snapshot` 的写锁内比较 `AtomicU64` —— 两个锁内操作互斥，逻辑无 TOCTOU 窗口；`reload_admin_ids` 的 `seen_gen` 在**读库前**取得（`228`），语义是「快照反映读库时刻的状态」。`release_probe` 类似的独立小函数拆分（`apply_admin_id` / `commit_admin_snapshot` 独立成函数以便直接单测）与仓内既有风格一致，未引入新抽象层。
- 3 个新单测分别用「真实调用序列 helper」而不是重复构造 tail，去重合理。

## Residual assessment

- 本轮无可修阻断项；S1-final / S2-final / S3-final 均为非阻塞，可随下次提交处理。
- 已登记 residual（计划 §7）未重开：中性帧无限流、错误构造器统一、**转换链提交后传输失败不计熔断（本轮新增行，证据与代码一致）**、目录管理员钉住、多候选 header 叠加、用户名大小写。§7 与代码现状逐条对得上。

## Evidence check

- 结论基于 diff + read + grep；单测**执行结果**与 L4 链路未由本席位验证（见 Scope `Truncated coverage:`）。W1-new 的「净记账」判定为**静态推演 + 代码结构证明**（`finished` 单标志、提前 return 的分支图、`breaker.rs` 计数器语义），不需要运行时即可成立。
- 与 PM 的「`cargo test` 320/320、`cargo build` 仅 2 条基线 dead_code 警告」自洽：新增测试无死代码残留，`State` 新增字段唯一构造点（`state.rs:161`）已填。

## Completion Report

- Status: **Done（L3 review complete）**
- Verdict: **Approve**（0 Critical / 0 Warning / 3 Suggestion）
- Findings: W1-new、S1-new、S2-new、S3-new 全部闭环；新增 S1-final / S2-final / S3-final 均非阻塞
- Report: `docs/plans/review/qc1-final.md`
- Verification: `git diff 49d3aff..eef9957` + 8 次定向 read/grep；未运行 build/test/lint/e2e
