---
report_kind: qc
reviewer: qc-specialist-2
reviewer_index: 2
plan_id: "2026-11-12-long-stream-and-admin-fixes"
verdict: "Approve"
generated_at: "2026-11-12"
---

# QC2 — plan QC tri-review

## Reviewer Metadata

- Reviewer: @qc-specialist-2（`role_id=qc-specialist-2`, `reviewer_index=2`）
- Runtime Agent ID: qc-specialist-2
- Runtime Model: deepseek/deepseek-v4-flash
- Review Perspective: **安全与正确性**（权限模型/会话、流式信息暴露、并发原子性、迁移安全）
- Report Timestamp: 2026-11-12

## Scope

- Review range / Diff basis: `git diff a65384d..3152498`（15 文件，+1189 / −45）
- Working branch（verified）: `fix/long-task-admin`；`git rev-parse --show-toplevel` = `/home/pop/AI/code/llm_gateway.wt/integration`
- plan_id: `2026-11-12-long-stream-and-admin-fixes`（无 `{HARNESS_DIR}` / store row，no-plan mode；计划文档 `docs/plans/2026-11-12-long-stream-and-admin-fixes.md`）
- Commit range: `a65384d`（main）..`3152498`（HEAD），合并点 `7f97190`（T1）/ `3b0e123`（T2）
- Files reviewed: 15 个 diff 文件全部经 diff 逐行读取（`migrations/0033_admin_source.sql`、`src/api/console.rs`、`src/config.rs`、`src/service/{console,session,proxy}.rs`、`src/state.rs`、`src/store/users.rs`、`web/src/{api/types.ts,views/Users.vue}`、`README.md`、plan + 两份 L2 报告、`scripts/mock_upstream.py` 仅看 diff 摘要），另按需读取 `src/api/console.rs:1293-1400,1630-1672`、`src/store/users.rs:115-150`、`src/service/proxy.rs:30-125,1300-1560`，并核实依赖源码 `reqwest-0.12.28/src/{error.rs,async_impl/response.rs}`（用于证明错误文案是否携带 URL）。
- Reused evidence: L2 `docs/plans/review/taskA.md` / `taskB.md`（两者各已修复一轮）+ 实现者 e2e/`cargo test` 记录（作为引用，未复用为结论）。
- Analysis methods: `git diff` / read / grep / 依赖源码比对 / 手工推理；**未运行 build / test / lint / install / e2e**（只读席位纪律）。
- Truncated coverage: `scripts/mock_upstream.py` 的代码只看了 diff 摘要（e2e 工具，不进生产路径）；`src/service/anthropic/stream.rs`、`src/service/responses/*` 的既有错误帧实现（本 diff 未改动，仅作为方言对齐参照）；Chat→Anthropic / Chat→Responses 转换出口的运行期错误帧行为（依赖 L2 taskA 已核结论）。以上不阻断结论：未覆盖部分与本次安全/正确性判定无因果关系，且已在 Residual assessment 标注。

## Verdict: Approve-with-residuals

**Verdict**: Approve（Critical 0 / Warning 0；5 条 Suggestion 均为非阻断加固项 —— 对应 Assignment 语义 Approve-with-residuals）

## Findings

### 🔴 Critical

无。

### 🟡 Warning

无。逐条回答 Assignment 指派的四个重点方向见 `## Evidence check`；未升级为 Warning 的理由在各条列出。

### 🟢 Suggestion

- **S1 — 客户端可见的上游错误文本缺少脱敏边界（当前无实际泄漏，边界未强制）**
  - 位置：`src/service/proxy.rs:1683-1701`（构造 `error_dialect`）→ `src/service/proxy.rs:2984-2999`（`SseErrorTail::poll_next` 用 `sse_error_frame(ctx, &e.to_string())` 把内部错误 Display 直接写进客户端帧）；对照 `src/service/proxy.rs:2765-2770` + `:2917`（`redact_secrets` 只作用于 header 失败路径）。
  - 证据：stream `Err` 只有两个来源 —— `with_stream_timeouts(resp.bytes_stream(), …)`（`src/service/proxy.rs:1319-1323`）与 `StreamTimeoutStream` 自造文案（`:104-112`）。`reqwest 0.12.28` 的 `Response::bytes_stream` 用 `crate::error::decode` 构造错误（依赖源码 `async_impl/response.rs:357`、`error.rs:319-321`，**不附加 url**），而 `Display` 只在 `self.inner.url` 存在时追加 ` for url ({url})`（依赖源码 `error.rs` 的 `impl fmt::Display for Error`）。因此当前客户端实际只会看到 `error decoding response body` 或 `upstream stream {first byte|idle} timeout`，**不含 URL / Key**。
  - 影响：今天为零泄漏；但该出口对错误类型无约束 —— 一旦后续把带 url 的错误（`Kind::Request` / `Kind::Status` 的 Display 必带 url）接进这条流，就会把上游 URL（部署可能形如 `https://gw/<KEY>/v1`，正是 `:2764` 注释承认的形态）送进任意已认证租户的响应体。既有脱敏纪律（`redact_secrets`）没有覆盖这条新出口。
  - 建议：客户端只发固定文案（如 `upstream stream interrupted`，detail 进 `tracing::warn!`），或把 `provider_key` 放进 `SseErrorContext` 后 `redact_secrets(e.to_string(), key)`；二者都是 ≤5 行。

- **S2 — 撤销管理员时 `admin_ids` 刷新失败是 fail-open（越权窗口保留到周期 reload）**
  - 位置：`src/api/console.rs:1362-1365`、`src/service/console.rs:60-64`、`src/state.rs:211-216`、`src/store/users.rs:142-149`（`load_admin_ids` 全量替换）、`src/api/v1.rs:226-231`（该缓存决定代理管理员旁路）。
  - 证据：两条刷新路径都是 `warn!` 后继续（计划 §4.4 已决定 warn-only、不把已提交 PATCH 变 500，本席不重开该决定）；`reload_admin_ids` 失败时整份旧集合保留。
  - 影响：撤销方向的窗口内，被撤销者仍命中 `can_use` 旁路（全部模型 / 不限路由）直到下一次周期 reload（`GATEWAY_RELOAD_INTERVAL` 默认 30s）。授予方向 fail-open 无害，撤销方向是越权保留 —— 比单纯"缓存滞后"更值得修。
  - 建议：在"已知旧值为 true、目标为 false"的路径上，reload 失败时从内存集合直接移除该 id（fail-closed）后再 warn；授予方向仍走整份刷新。

- **S3 — commit 之后的上游流错误不进入熔断健康信号（跨请求盲区）**
  - 位置：`src/service/proxy.rs:1497`（提交即 `record_success`）；`SseErrorTail` 的错误分支（`:2984-2999`）只 `tracing::warn!`。全仓 `record_failure` 仅在 commit 之前的路径：`:1189,1283,1299,1391,1412,1434,1491,1807,1900,1917,1927`。
  - 证据：`grep -n 'record_failure|record_success' src/service/proxy.rs` 结果如上，`wrap_stream` / `SseErrorTail` 区间内无任何 `record_*`。
  - 影响：**不重复争论**"deadline 到期后错误帧不切候选"（已登记的设计代价）。新增的跨请求后果是：`prime_timeout=2000ms` + `first_byte=300s` 组合下几乎每次 SSE 都在 2s 提交并记 success；此后真正的首字节/静默超时只产出错误帧 → 熔断器永远观测不到"提交后卡死型"上游，健康度恒绿，后续请求仍优先选它（用户反复拿到 200 + 错误帧，而不是被路由到健康候选）。
  - 建议：把 provider id 放进 `SseErrorContext`，在错误分支 `breaker.record_failure(provider.id)`（timeout 与 transport 分开计数，可复用 `is_timeout_error`）；若有意不做，请在计划 §4 把它写成与"不切候选"并列的登记代价。

- **S4 — `is_admin` 与 `reset_admin_source: false` 组合被判定互斥（误拒）**
  - 位置：`src/api/console.rs:1303-1307`。
  - 证据：互斥检查用 `is_some()`，而 reset 的真实动作只在 `== Some(true)`（`:1358`）。故 `{"is_admin": true, "reset_admin_source": false}` 返回 400，虽然 `false` 是 no-op。
  - 影响：API 契约层面误拒；当前 web 不发送该字段（`web/src/views/Users.vue` 只发 `{reset_admin_source: true}`），无线上影响，但会咬到 SDK / 脚本 / 未来表单。
  - 建议：条件改为 `req.is_admin.is_some() && req.reset_admin_source == Some(true)`。

- **S5 — 计划 §4.1 的到期提交条件与实现不一致（实现正确，文档需改）**
  - 位置：计划 `docs/plans/2026-11-12-long-stream-and-admin-fixes.md` §4 W1.1（"仅当 `is_sse && buffered.is_empty()` 时到期即提交"）vs `src/service/proxy.rs:143-146`（`prime_deadline_commits = expired && is_sse`）与 `:1337-1350`。
  - 证据：S3 症状本身就是"持续中性帧"（缓冲**非空**）—— 按计划字面加 `buffered.is_empty()`，S3 根本不会被修复。实现的放宽是必要的，代码注释（`:135-142`）已写明"缓冲为空或仅有中性帧同样提交"。
  - 影响：纯文档面；后续 QC / PM 按计划字面复检会把正确实现判成偏离（与 L2 已登记的 §4.3 `PATCH {}` 措辞同类）。
  - 建议：PM 把 §4.1 改为"SSE 响应在 deadline 到期即提交（无论缓冲是否为空）；非 SSE 仍保持『等 JSON 体 / 零产出』判定"。

### ⚪ Unconfirmed

无（证据通道完整：diff、源码、依赖源码、计划与 L2 报告均可读）。

## Summary

| Severity | Count |
|----------|-------|
| 🔴 Critical | 0 |
| 🟡 Warning | 0 |
| 🟢 Suggestion | 5 |
| ⚪ Unconfirmed | 0 |

## Source Trace

| ID | Source Type | Source Reference | Confidence |
|---|---|---|---|
| S1 | git-diff + read + dep-source | `src/service/proxy.rs:1683-1701,2984-2999,2765-2770,2917`；依赖源码 `reqwest-0.12.28/src/error.rs`（`impl fmt::Display for Error`）、`async_impl/response.rs:355-358` | High |
| S2 | git-diff + read + grep | `src/api/console.rs:1362-1365`；`src/service/console.rs:60-64`；`src/state.rs:211-216`；`src/store/users.rs:142-149`；`src/api/v1.rs:226-231` | High |
| S3 | git-diff + grep | `src/service/proxy.rs:1497`；`record_failure` 全量命中清单（见正文）；`SseErrorTail` 错误分支 | High |
| S4 | git-diff + read | `src/api/console.rs:1303-1307,1358` | High |
| S5 | doc-rule + git-diff | 计划 §4 W1.1；`src/service/proxy.rs:135-146,1337-1350` | High |

## Residual assessment

**可在本次范围内修复**（每条 ≤10 行，均不需新依赖/新配置）：

- S1：改 `SseErrorTail` 的文案来源（固定文案或 `redact_secrets`）。
- S2：撤销失败时 fail-closed 移除 id。
- S3：`SseErrorContext` 增加 provider id + 错误分支 `record_failure`；若判定为可接受代价，改为计划登记（一行）。
- S4：互斥条件收窄到 `Some(true)`。
- S5：仅改计划措辞（PM 侧）。

**已由 L2 / PM 处置、本席不重开**（作为 residual 追踪，不重复计为 finding）：

- **目录管理员无法"钉住"为手动覆盖**（L2 taskB S5；PM 已处置/延后）：`src/api/console.rs:1350` 的 `target.is_admin == grant` 短路意味着"目录已是管理员"时再授予不写 override，只能先撤销再授予（有一次瞬时空窗）。本席确认其可达（按钮已翻转为"取消管理员"，但 `{"is_admin": true}` 仍可构造），属 AC 语义偏差而非安全漏洞 —— 处置权在 PM 既有结论内。
- **迁移前手动授予/撤销的静默变权**（L2 taskB W3 / R2）：回填把 LDAP 行一律记为 `directory`，升级后首次登录按目录重算。README 新增行已给出提示；需 PM 落到计划 §7 / issue 载体。
- **回退窗口（0033 之后回滚）** —— 同时回答 Assignment 的"`SELECT *` 新增列对旧二进制有无越权影响"：**读取侧无影响**（sqlx `FromRow` 按列名取值，新增列对旧二进制透明）；越权影响在**写入侧** —— 旧二进制的 `set_is_admin` 只写 `is_admin`，`admin_override` 保持 NULL，回滚窗口内的"撤销"会在重新升级后于下次 LDAP 登录被 `COALESCE(NULL, admin_ldap)` 复活。不构成提权，只是"撤销丢失/复活"。L2 已记为 R3 并建议改运维说明；本席同意，不另开。
- **禁用管理员不刷新 `admin_ids`**（L2 taskB R1，pre-existing）：`set_status(0)` 会 `revoke_all_for_user` 但不刷新缓存，禁用管理员在 ≤30s 内仍可走代理旁路。本次新增的 `reload_admin_ids` 恰好让这条修复变便宜（一行），但属既有缺陷，建议 PM 按 residual 决定是否顺手收口。
- **用户名大小写归一化 / local→ldap 源转换**（计划 §7 deferred，A4）：已在 diff 外，本次未触碰。

## Evidence check

实现者与 L2 证据自洽性（逐条核对代码，不重跑）：

**1. LDAP 权限模型 —— 无绕过门径（结论：成立）**

- 全部写 `admin_override` / `admin_ldap` 的路径只有四条（`grep admin_override|admin_ldap` 全仓 + L2 的写路径枚举交叉验证）：`create_local_user`（`src/store/users.rs:65-66`，`admin_override=$4`）、`upsert_ldap_user`（`:89-98`，`admin_ldap=EXCLUDED.admin_ldap` + `is_admin=COALESCE(users.admin_override, EXCLUDED.admin_ldap)`）、`set_is_admin`（`:163`）、`reset_admin_source`（`:174-178`）。另两处 `INSERT INTO users`（`groups.rs` 组同步建档、`mod.rs` break-glass 种子）前者不触碰任何 admin 列（新行 `admin_ldap` 默认 FALSE、`admin_override` NULL，不变式成立），后者走 `create_local_user`。**没有第二个写 `admin_override` 的 API 路径**，即"仅 UI 显式 manual override 才豁免目录真值"成立。
- 客户端不能伪造目录真值：`UpdateUserReq`（`src/api/console.rs:1281-1291`）只含 `status/is_admin/reset_admin_source`，`admin_ldap` 无对应字段，serde 默认忽略未知键 → 无 mass-assignment。
- 越权修改他人权限仍由 `require_admin` 把关：`update_user` 取 `axum::extract::Extension<users::UserRow>`，该 extension 只在 `require_admin` 成功后写入（`src/api/console.rs:110-142`），且该中间件每请求查库判定 `user.is_admin`（DB 有效值），不读 JWT。
- reset 守卫双层：API 层 `reset_admin_source == Some(true) && target.source != "ldap"` → 400（`:1317-1321`，位于任何字段写入之前）；store 层 `WHERE id = $1 AND source = 'ldap'`（`src/store/users.rs:176-177`）→ 即使 API 校验被绕过也不会写本地行。
- 互斥：`is_admin` 与 `reset_admin_source` 同时出现 → 400（`:1303-1307`；S4 只涉及 `Some(false)` 的误拒，不构成绕过）。
- 受保护账号：`ensure_not_protected`（`:1633-1645`）对保护用户名**无条件 403**，因此 reset 也不能用来降权 break-glass 管理员（特意核对，未发现缺口）。

**2. 令牌 / 会话 —— 无新的可信源（结论：成立）**

- `Claims.is_admin`（`src/service/session.rs:46`）仍**只写不读**：全仓无 `claims.is_admin` 读点；控制台授权读 DB（`src/api/console.rs:139`），代理旁路读 `admin_ids` 缓存（`src/api/v1.rs:226-231`）。故 JWT 里的旧 `is_admin` 不会成为越权来源（它是既有的死字段，可选清理，不在本次范围）。
- `admin_ids` 刷新时序：JIT 首登管理员 `admin_ids_changed(None, true)=true` → 刷新（否则首登管理员会滞后一个周期，正是 RCA A3 的一半）；JIT 改权限、PATCH 授予/撤销均刷；`reload()` 的 admin_ids 分节改为复用 `reload_admin_ids()`（`src/state.rs:265-268`），单实现无分叉。刷新失败只 warn → 撤销方向的 fail-open 窗口见 **S2**。
- 缓存口径正确：`load_admin_ids` 过滤 `is_admin AND status = 1`（`src/store/users.rs:144-146`）→ 用的是**有效权限**而非某个来源列，禁用者不在集合内。
- 代理旁路用的就是该缓存（`src/api/v1.rs:226-231`），没有第二条授权读；控制台中间件每请求查库，PATCH 后天然即时。

**3. 流式安全 / 正确性**

- 错误体信息暴露：见 **S1**（当前零泄漏，边界未强制）。
- 请求重放 / 双计费：**未发现**。提交后不回到 `'candidates` 循环（`committed = true; continue` 直接退出 `while`，`:1337-1350`），故不会二次发送上游请求；记账仍在 `wrap_stream` 的 `ctx.recorded.swap(true)` + EOF 兜底单次路径（错误帧作为普通 chunk 经过 `then`，不产生第二次 `usage::record`），实现者 e2e 的"`usage recorded … status=200` 一次 + `emitting SSE error frame` 一次"与该结构一致。
- 注入 / 格式污染：帧体全部经 `serde_json::json!` / `ResponsesStreamEventOut` 序列化，`msg` 中的换行与引号被 JSON 转义，无法构造第二条 SSE 帧或伪造 `event:` 行；Chat 帧不带 `event:`、Anthropic 帧带 `event: error`、Responses 帧带 `event: response.failed` 且 `output` 为数组 —— 三处方言与各自转换链路对齐（这同时关闭了 L2 taskA 的"方言不精确"建议）。
- `SELECT *` / `RETURNING *` 与新增列：`UserRow` / `UserWithUsage` 为 sqlx `FromRow` 按列名绑定，新增 `admin_ldap/admin_override` 对旧二进制读取透明；越权影响只在旧二进制写入侧（见 Residual assessment 回退窗口条）。

**4. 并发 / 原子性（结论：不存在不变式破裂窗口）**

- 三条会改权限的语句都在**单条 SQL 内**由另一列推导 `is_admin`：JIT 的 `is_admin = COALESCE(users.admin_override, EXCLUDED.admin_ldap)`（Postgres 在 `ON CONFLICT DO UPDATE` 下对并发已提交版本用 EvalPlanQual 重估，读到的必是最新 `admin_override`）、`set_is_admin` 的 `admin_override=$1, is_admin=$1`、reset 的 `admin_override=NULL, is_admin=admin_ldap`。任两条并发都在行锁上串行，最后一条提交后三列一致 → 无瞬时半态、无永久破裂。
- `reload_admin_ids` 失败路径：`load_admin_ids` 在写锁之前 await（`src/state.rs:211-216`），不跨 await 持锁；失败即整份旧集合保留（S2）。
- 迁移在单事务内（`sqlx::migrate!`，`src/store/mod.rs:29`，逐文件事务），DDL 与回填不暴露中间态。

**5. 迁移安全（结论：成立）**

- 回填公式对任意 source 成立：`admin_ldap = (source='ldap' AND is_admin)`、`admin_override = CASE WHEN source <> 'ldap' THEN is_admin ELSE NULL END`（`migrations/0033_admin_source.sql:17-19`）→ 任意第三类 source 的行也不会破坏 `is_admin = COALESCE(admin_override, admin_ldap)`（这正是 L2 S2 的修复形态）。
- 列默认值对旧行：`admin_ldap NOT NULL DEFAULT FALSE` + `admin_override` 可空，随后无条件 `UPDATE users` 覆盖全部行，不存在"新列默认值使老管理员看起来是普通成员"的残留（`is_admin` 保持现值）。
- 不可重跑：文件注释第 10-11 行明写"本 UPDATE 不幂等"；且迁移由 `sqlx::migrate!("./migrations")` 版本表驱动，正常运行不会重放 → 与计划 §6 "空库与现有库各跑一次"的字面判据相容（本席核对机制与注释，不重跑迁移）。

**证据缺口（交 PM Acceptance，本席不自执行）**

- 计划 §6 两条判据只被"代码形状"覆盖、未被实际演练：①"reload 失败不产生 500（模拟）"—— 代码形状确实保证 warn-only（已核），但无模拟演练证据；②"PATCH 后立即 `GET /api/admin/users` **与代理旁路一致**"—— e2e 只观察到 `/api/me`，没有一次"走管理员旁路的代理请求"观测。两者都不影响本席对代码的判定，但需在 acceptance 记录里显式接受或补一次观测。
- 实现者 e2e 的 `cargo test 309/309`、`npm run build`、长流/504 场景数字本席未复算（只读席位，未运行）；已接受为 L1/L2 证据。
