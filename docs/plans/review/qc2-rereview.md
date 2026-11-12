---
report_kind: qc
reviewer: qc-specialist-2
reviewer_index: 2
plan_id: "2026-11-12-long-stream-and-admin-fixes"
verdict: "Request Changes"
generated_at: "2026-11-12"
---

# QC2 — targeted re-review（fix wave 2）

## Reviewer Metadata

- Reviewer: @qc-specialist-2（`role_id=qc-specialist-2`, `reviewer_index=2`）
- Runtime Agent ID: QC2Rereview
- Review Perspective: **安全与正确性**（权限模型/会话、流式信息暴露、并发原子性、迁移安全）
- Report Timestamp: 2026-11-12

## Scope

- Review range / Diff basis: `git diff 3152498..49d3aff`（13 文件，+830 / −215），逐 commit 复核：`e1de6aa`（上一轮报告落盘）、`e591e3a`（计划措辞）、`a162a2b`（S4 + S2）、`5ca196a`（0034 CHECK）、`8b173fe`（S1 + S3 + 方言/PRIME）、`40f252a`/`49d3aff`（合并点）。
- Working branch（verified）: `fix/long-task-admin`；toplevel = `/home/pop/AI/code/llm_gateway.wt/integration`。
- plan_id: `2026-11-12-long-stream-and-admin-fixes`（no-plan mode，产物 `docs/plans/`）。
- Reused evidence: 上一轮 `qc2.md`（S1–S5）、L2 `taskA.md`/`taskB.md`、PM 已核的 L1（`cargo build` 零错、`cargo test --bin llm_gateway` 315/315、`web npm run build` 零错）——本轮**未重跑**任何 build/test/lint。
- Analysis methods: `git show`/`git diff` + read + grep + 手工推理（含熔断计数的数值推演）。
- Truncated coverage: 只复核本席 owned findings 与 fix delta 的直接接口；`docs/plans/review/qc1.md`/`qc3.md` 所属条目（QC1 的 S2 CHECK、QC3 的 F-001/F-003、PrimeDeadline 重构的完整正确性）按席位边界未重审；`web/src/**` 前端 delta 未读；`src/service/proxy.rs` 仅精读 `PrimeDeadline`（:141-190）、提交段（:1429-1545）、`redact_secrets`/`send_upstream`（:2801、:2953）、`SseErrorTail`+`sse_error_frame`（:3028-3134）与新增测试。未覆盖范围不影响本报告的 S1–S5 判定。

## Verdict: Needs-fix（`Request Changes`）

S1 / S4 / S5 已闭环；S2 主目标闭环（增量同步、双向正确、status 感知），但残留一处与周期 reload 的竞态（F2）；**S3 未闭环** —— 新增的 post-commit `record_failure` 在默认熔断参数下永远触发不了熔断（F1），且其新增测试构造不含提交时的 `record_success`，无法暴露该问题。

## Findings

### 🟡 Warning

**F1 — `SseErrorTail` 的 post-commit 熔断失败在默认配置下无法推出任何熔断（S3 未闭环）**

- 位置：`src/service/proxy.rs:1526`（提交即 `record_success`）→ `src/service/proxy.rs:3115`（流内 `Err` 时 `record_failure`）；计数语义在 `src/service/breaker.rs:123-147`（`record_success`：`consec_failures = 0`，末尾 `window_requests += 1`）与 `:149-176`（`record_failure`：`window_requests += 1; window_failures += 1`，`rate_open` 需 `window_requests >= min_requests`）。
- 证据（同一请求内两次计数互相抵消）：“200 + 卡死”上游的每个请求都是 ① commit → `record_success`（连续失败清零、`window_requests` +1）② 流内 idle/first-byte 超时 → `record_failure`（`consec_failures` = 1、`window_requests` +1、`window_failures` +1）。默认参数 `failure_threshold=4`、`failure_rate=0.6`、`min_requests=10`（`breaker.rs:39-42`）下，第 k 个坏请求后 `window_requests ≈ 2k-1`、`window_failures = k`，误差率 `k/(2k-1)`；唯一 `>= 10` 样本的候选点 k=6 时为 `6/11 ≈ 0.545 < 0.6`，此前 k≤5 均被 `min_requests` 拦住 → **连续与错误率两个维度都不熔断**，且 `consec_failures` 每个请求都被 commit 的 success 归零，永远到不了 4。这正是 S3 描述的形态（`PRIME=2000ms ≪ FIRST_BYTE=300s`，几乎每次都先提交再超时）。
- 影响：`48b173fe` 提交信息与计划 §7 的“错误已记熔断”在默认配置下不成立 —— “200 + 错误帧”的上游不会被降级链跳过；后续请求仍优先选它，用户持续拿到 200 + 错误帧。半开探测路径（`record_failure` 的 `half_open_probe_in_flight` 分支）有效，但前提是熔断器先被别的原因打开。
- 测试盲区：新增 `sse_error_tail_records_one_breaker_failure_per_stream`（`proxy.rs:4094-4115`）只对 `SseErrorTail` 单独喂 `Err`，**没有**模拟提交时的 `record_success`，因此断言“两条流后熔断”成立，却与生产调用序列不一致（测试通过 ≠ 行为成立）。
- 最小修复方向（任一，均 ≤10 行 + 一条回归测试）：
  1. **计数改写**：给 `Breaker` 增加 `record_committed_failure(provider_id)`：先把本请求在 commit 时记下的那次 success 撤回（`window_requests -= 1` 并复位窗口起点判断），再走既有 failure 计数 —— 净效果每个坏请求 1 次失败样本，纯坏流量错误率趋近 1.0，`min_requests=10` 即熔断；`SseErrorTail` 错误分支改调它（半开分支保持 `record_failure` 语义）。
  2. **延后 success**：commit 不记 success，改为流正常结束（`SseErrorTail` 的 `Poll::Ready(None)`）才 `record_success`；但必须同时给 `SseErrorTail` 加 `Drop` 兜底（`finished == false` 的 drop = 客户端断开）释放半开探测名额，否则 `half_open_probe_in_flight` 会永久占位。
- 回归测试要求：测试须包含 `record_success` → `record_failure` 的**真实顺序**（即 `test_breaker(4)` 默认参数下，连续 N 个“提交成功 + 流内失败”的请求最终 `allow() == false`）；仅测 `SseErrorTail` 不足以闭环。

### 🟢 Suggestion

**F2 — 撤销的 fail-closed 仍可被并发周期 reload 回滚（S2 残留竞态）**

- 位置：`src/state.rs:213-216`（`reload_admin_ids`：先 `load_admin_ids(...).await` 读库，再整份 `*self.admin_ids.write() = v`）vs `src/state.rs:225-227`（`sync_admin_id` 写锁内增删单 id）。
- 证据：两处都是无版本守卫的 read-modify-write。时序：T0 周期 reload 读到快照（含 X 为管理员）→ T1 PATCH 撤销提交 → T2 `sync_admin_id` 移除 X → T3 reload 拿到写锁整份覆盖（快照里 X 仍在）→ X 重新进入旁路，直到下一次 reload（`GATEWAY_RELOAD_INTERVAL` 默认 30s）。这使计划 §4.4 的“撤销 fail-closed”措辞过强（应为“撤销不再依赖 reload **成功**，但仍受一次在途 reload 的快照回滚影响”）。
- 影响：窗口 ≤ 一个 reload 周期，且需要 reload 的读快照早于提交、写锁晚于 sync 的交错；比修复前的**失败即整份保留**显著收窄，但未归零。
- 最小修复方向：加一个代数守卫（`admin_ids_gen: AtomicU64`）：`sync_admin_id` 在写锁内 `fetch_add(1)`；`reload_admin_ids` 在 `load_admin_ids` 之前取 gen，写锁内比较，不一致则丢弃快照（或重试一次）。约 8 行，`tests` 可用两个任务交错验证。
- 附带：`update_user`（`src/api/console.rs:1333-1379`）的 `sync_admin_id` 位于 `set_status`/`revoke_all_for_user`/`find_by_id` 之后，任一语句失败导致的 500 会跳过同步（缓存停留到下一次 reload）——与 pre-fix 同量级，无需处理，仅记录。

**F3 — 计划文档与 delta 的两处措辞漂移**

- 位置：`docs/plans/2026-11-12-long-stream-and-admin-fixes.md` §7 表格（约 :102-:105）与 §4.4。
- 证据：① §7 仍登记 “禁用用户不即时刷新 `admin_ids`” 为 deferred，但本 delta 的 `src/api/console.rs:1379` 对 status 变更同样调用 `sync_admin_id(updated.id, updated.is_admin && updated.status == 1)`，该项已闭环（`set_status` 全仓只有这一个调用点，`grep` 已核）→ 该行应删除或改为“已随 W2 收口”。② §7 “仅中性帧的无限流” 行的理由写 “且错误已记熔断”，在 F1 修复前不成立（记录 ≠ 熔断）；修 F1 后措辞才成立。③ §4.4 “撤销 fail-closed” 见 F2。
- 影响：纯文档面，但会让后续 QC/发布说明复检时对代码得出错误结论（与上一轮 S5 同类）。
- 建议：随 F1/F2 一并改三处措辞。

**F4 — 提交前的故障切换错误体仍直出未脱敏的 `e`（同类边界，非本 delta 引入）**

- 位置：`src/service/proxy.rs:1452`（`format!("upstream stream failed before productive output: {e}")` → `upstream_error_envelope` → 全候选失败时作为客户端 502 body）。
- 证据：该 `e` 来自 `with_stream_timeouts(resp.bytes_stream(), …)`，今天只可能是 `reqwest` 解码错误（`Display` 不含 url）或 `StreamTimeoutStream` 构造的固定字符串，**当前零泄漏**；但与本轮刚修的帧出口属同一“边界未强制”类，`provider_key` 已在作用域内。
- 建议：一行 `redact_secrets(msg, &provider_key)`（或固定文案 + 日志带详情）。不阻断本轮。

### ⚪ Unconfirmed

无（证据通道完整）。

## Summary

| Severity | Count |
|----------|-------|
| 🔴 Critical | 0 |
| 🟡 Warning | 1（F1，阻断） |
| 🟢 Suggestion | 3（F2、F3、F4） |
| ⚪ Unconfirmed | 0 |

## S1–S5 处置复核

| ID | 原 finding | 处置（commit） | 复核结论 |
|---|---|---|---|
| S1 | 错误文本脱敏边界 | `8b173fe`：`SseErrorContext` 携带 `provider_key`/`request_id`/`provider`/`model`，`proxy.rs:3117` 对客户端帧做 `redact_secrets`，`:3118-3124` 日志保留关联字段；新增测试 `sse_error_tail_redacts_provider_key_from_client_frame`（`:4070`）断言 URL 内密钥变 `***` | **已闭环**。`redact_secrets`（`:2801-2806`）字面替换覆盖 Provider Key/URL 内的 key；日志用同一脱敏串，无第二条泄漏面（帧体经 `serde_json::json!` 转义）。残余同类边界见 F4（pre-commit，非本 delta） |
| S2 | 撤销 fail-open | `a162a2b`：`AppState::sync_admin_id`（`state.rs:225-227` + `apply_admin_id` :347）；PATCH（`console.rs:1379`）、LDAP JIT（`service/console.rs:49`）共路径；`reload_admin_ids` 仅留周期/启动 | **闭环（有残留竞态，F2）**。口径一致：两处调用都传 `is_admin && status == 1`，与 `load_admin_ids` 的 `WHERE is_admin AND status = 1`（`users.rs:141-146`）相同；`set_status` 全仓唯一调用点就在 `update_user`，禁用即移出旁路；授予/撤销两向正确（`apply_admin_id` 幂等、只动目标 id）。新增 `incremental_admin_sync_inserts_and_removes` / JIT 测试（`state.rs:376`）。无新增越权窗口；`/v1/models` 旁路（`api/v1.rs:227`）仍是唯一读者，API-key 认证对禁用账号返回 401（`service/auth.rs:79-81`），禁用用户的旁路因此彻底不可用 |
| S3 | 提交后失败不进熔断 | `8b173fe`：`SseErrorTail` 首个 `Err` 记 `record_failure`（`:3115`，`finished=true` 在 `:3108` 保证一次），客户端断开无 `Err` 天然不计 | **未闭环 → F1**。单次记录语义正确（不会重复计），但提交时的 `record_success`（`:1526`）与失败同请求内相互抵消，默认参数下两个维度都无法熔断 |
| S4 | 互斥误拒 | `a162a2b`：`admin_actions_conflict`（`console.rs:1300-1302`）仅 `is_admin.is_some() && reset_admin_source == Some(true)`；单测 6 组（`:1677`） | **已闭环**。`(true, false)` 放行并按 `is_admin` 处理（无 reset 分支），`(true, true)`/`(false, true)` 400；`reset` 对非 LDAP 的 400 校验仍在任何写入之前（`:1324-1329`） |
| S5 | 计划 §4.1 措辞 | `e591e3a` | **已闭环**。§4.1 改为“到期且 `is_sse` 时立即提交（缓冲为空或仅中性帧同样提交，缓冲原样前置）”，与 `PrimeDeadline`/`is_sse` 判定一致；§4.2 新增超时优先级与 `PRIME ≥ FIRST_BYTE` 告警说明，与 `config.rs:155-172` + README 一致 |

## Evidence check（本轮新查的两处重点）

**A. 0034 CHECK 约束的写入失败面（无客户端可达的 DoS）**

- `migrations/0034_admin_invariant.sql:10-11`：`CHECK (is_admin = COALESCE(admin_override, admin_ldap))`，`ALTER TABLE` 在 sqlx 单文件事务内、启动期执行一次；存量违反行数为 0 时仅做一次全表校验（users 级表，无锁争议）。
- 五条写路径逐条核对等式在**单条 SQL 内**成立：`create_local_user`（`users.rs:65-66`，`is_admin=$4, admin_override=$4`）、`upsert_ldap_user`（`:89-98`，`admin_ldap=EXCLUDED.admin_ldap`、`is_admin=COALESCE(users.admin_override, EXCLUDED.admin_ldap)`；insert 分支两列同为 `$5`）、`set_is_admin`（`:164`，两列同 `$1`）、`reset_admin_source`（`:177-178`，`admin_override=NULL, is_admin=admin_ldap`）、`upsert_sync_user`（`groups.rs:129-135`，显式 `is_admin=FALSE` + 默认 `admin_ldap=FALSE`/`admin_override=NULL`，冲突分支不触碰权限列）。`grep` 全仓无第六处写 `is_admin`/`admin_override` 的语句，也无 `DELETE FROM users`（禁用是唯一停用路径）→ **客户端无法构造违反约束的写入**，无 500/DoS 面。
- 唯一的现实失败面在**回滚**：旧二进制（`<= 3152498` 之前）的 `set_is_admin` 只写 `is_admin`、`upsert_ldap_user` 写 `is_admin=EXCLUDED.is_admin` 而不碰 `admin_ldap`，在 0034 之后回滚会得到 CHECK 违反 → 管理员授予/撤销 PATCH 500，且目录管理员的 LDAP 登录会因 `is_admin ≠ admin_ldap` 500。这是既有回滚 residual（上一轮已登记“撤销丢失/复活”）的**升级形态**：从静默变权变成显式写入失败。回滚不可用是当前 feature 分支的正常代价，但发布说明应写明“0034 之后不可回滚到双来源之前的二进制”，否则运维会在回滚时误判为故障。

**B. 增量缓存同步 × `disabled` / `token_version`**

- `token_version`：`admin_ids` 缓存只按 user id 集合匹配，与令牌版本无关；禁用路径 `revoke_all_for_user`（`console.rs:1339`）与缓存移除在同一 handler 内先后执行，无“令牌有效 + 旁路仍命中”的组合（API-key 认证另查 `user_status`）。
- `disabled`：`sync_admin_id` 本身不信 `is_admin` 列值，两个调用点都传 `is_admin && status == 1`；口径有单测与文档约束，但**函数签名仍是裸 `bool`** —— 未来第三个调用点若传 `user.is_admin` 就会把禁用管理员重新塞进旁路。建议（非阻断）把入参收成 `(is_admin, status)` 或直接收 `&UserRow`，让 `&& status == 1` 无法漏写。

## Residual assessment

- **不重开**（计划 §7 已登记）：仅中性帧无限流、错误构造器统一（Anthropic/Responses 透传与转换链路已对齐 `stream_error`；Responses 透传用 `event: error`、转换链路用 `response.failed`，两形状均合规范且代码注释写明理由）、目录管理员钉住、多候选 header 叠加、用户名大小写。
- **本轮应随 F1 一并处置**：计划 §7 “错误已记熔断” 的括注、§7 “禁用用户不即时刷新 `admin_ids`” 一行（已闭环，应删）、§4.4 “撤销 fail-closed”（受 F2 竞态约束）。
- **交发布说明（不需代码改动）**：0034 之后的回滚限制（见 Evidence A）。
- **交 PM Acceptance 的观测缺口**：本轮仍未看到“熔断降级链真的跳过 200+卡死上游”的运行期观测 —— 在 F1 修复前也不可能有；修复后建议补一次 e2e（mock 上游恒定首字节延迟，连续 ≥10 次请求后该渠道被跳过）。

## Source Trace

| ID | Source Type | Source Reference | Confidence |
|---|---|---|---|
| F1 | git-diff + read + arithmetic | `proxy.rs:1526,3108-3124`；`breaker.rs:123-147,149-176,39-42`；测试 `:4094-4115` | High |
| F2 | read + concurrency reasoning | `state.rs:213-216,225-227`；`console.rs:1333-1379` | High |
| F3 | doc-rule + grep | 计划 §4.4/§7；`console.rs:1379`；`users.rs:118`（`set_status` 唯一调用点） | High |
| F4 | read + dep reasoning（同上一轮 S1） | `proxy.rs:1452,2801-2806,2953` | Medium |
| A | git-diff + grep | `migrations/0034_admin_invariant.sql:10-11`；`users.rs:65-66,89-98,164,177-178`；`groups.rs:129-135` | High |
| B | read + grep | `service/auth.rs:79-81`；`api/console.rs:1339,1379`；`service/console.rs:49` | High |
