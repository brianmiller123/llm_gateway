# 长任务流式中断 + LDAP 管理员覆盖 修复计划

> 状态：Prepare 完成（specify/clarify/plan 已锁定）｜日期：2026-11-12｜基线：`main@a65384d`
> 无 `{HARNESS_DIR}`/`mstar` CLI：执行状态按对话追踪（no-plan mode），本文件是路线与验收的可追踪载体。

## 1. Specify（用户可见行为）

**W1 长任务流式不得被网关误判失败**
- 给定一个需要长预填充（首字节 > 60s）或长思考（chunk 间隔 > 120s）的合法上游流，客户端 MUST 持续收到 `200 text/event-stream` 与心跳，直到上游自然结束。
- 网关 MUST 在对上游响应头 2xx 后尽早向客户端返回响应（不得无界等待“首个产出性事件”）。
- 上游真正卡死时，网关 MUST 在有界窗口内终止，且客户端 MUST 收到结构化 SSE 错误帧，而非连接被硬断（`socket connection was closed unexpectedly`）。
- 未向客户端写出任何字节前失败：超时 MUST 映射 504，连接失败 502，候选耗尽 503（不得把超时/4xx 一致改写成 502）。
- 三条链路（Chat 透传 / Anthropic 转换 / Responses 转换）行为对齐。

**W2 LDAP 用户管理员权限不得“偶发消失”**
- 目录授予（memberOf/组）MUST 在登录时自动生效、移出目录组后自动撤销。
- UI 手动授予/撤销 MUST 成为显式覆盖：后续 LDAP 登录不得覆盖，直到管理员显式“恢复目录同步”。
- 授权变更 MUST 立即对 API 鉴权生效，并即时刷新代理旁路 `admin_ids`；已提交的变更不得因 reload 失败返回 500。
- `GET /api/me`、用户列表 MUST 暴露权限来源（目录/手动），UI 可区分。

## 2. Clarify（已确认决策）

| 决策 | 结论 |
|---|---|
| “提示增强”失败面 | ompgui 魔棒 → `omp --print --no-session` → omp provider `localtest`（本网关）；故归入 W1 验收路径，不改 ompgui |
| LDAP 管理员权威 | 双来源：目录自动升降；UI 手动授予为覆盖（可恢复目录同步） |
| 502 取证 | 无生产日志，按代码语义定位并本地复现 |

## 3. RCA（确证，行号对应 `main@a65384d`）

### W1
| # | 触发 | 路径 | 症状 |
|---|---|---|---|
| S1 | 上游 2xx 后首 chunk > 60s（长预填充） | `src/service/proxy.rs:32-115` 首字节计时 → priming 读到 Err `1332-1352` → `last_failure=(502,…)` `1627-1662` | 502 `upstream stream first byte timeout` |
| S2 | 首 chunk 后 priming 内间隔 > 120s | 同上 idle 计时 `88-99` | 502 `upstream stream idle timeout` |
| S3 | 上游持续中性帧（`delta.role`/`message_start`/`: ping`）无产出 | priming `1279-1404` 只在 `Productive|Terminal` 时构造响应；中性帧 `2452-2493` 永不计产出；idle 每 chunk 重置 `103-112` | 客户端无任何响应头/字节 → omp 300s 首事件超时 |
| S4 | commit 后上游静默 > 120s | `wrap_stream` 透传 `chunk?` `2904` 把 Err 交给 hyper | chunked body 硬断 → `socket connection was closed unexpectedly`（转换链路已有错误帧，透传链路没有） |
| S5 | priming 内失败 | 全部走 `last_failure`（优先于 `saw_timeout`） | priming 超时永远是 502，从不 504 |

### W2
| # | 触发 | 路径 | 症状 |
|---|---|---|---|
| A1 | LDAP 用户每次登录 | `src/service/console.rs:37` → `src/store/users.rs:81-88` `is_admin = EXCLUDED.is_admin` | UI 手动授予被目录值覆盖 → “偶发无权限” |
| A2 | 授予后 reload 失败 | `src/api/console.rs:1333-1336` 已提交仍 500 | UI 显示失败、实际生效 |
| A3 | LDAP JIT 改权限 | 不刷新 `admin_ids`（`state.rs:259-263`，仅 30s 周期） | 代理旁路最长 30s 与 DB 分叉 |
| A4 | 本地管理员同名走 LDAP | `upsert_ldap_user` 强制 `source='ldap'` 并重算 | 本地管理员被转源并降权 |

## 4. Design（实现约束）

### W1（`src/service/proxy.rs`、`src/config.rs`）
1. **Priming 有界 + 尽早提交**：新增配置 `GATEWAY_STREAM_PRIME_TIMEOUT_MS`（默认 2000，0=不限）。priming 循环引入墙钟 deadline；到期且 `is_sse` 时**立即提交**（缓冲为空或仅中性帧同样提交，缓冲原样前置）；非 SSE 保持“上游 200 但回 JSON 体 / 零产出”的既有识别路径。提交后客户端立刻拿到响应头与心跳；deadline 之后到达的上游错误帧随流透传，不再切换候选。
2. **超时默认值**：`GATEWAY_STREAM_FIRST_BYTE_TIMEOUT` 默认 60 → 300，`GATEWAY_STREAM_IDLE_TIMEOUT` 默认 120 → 300（0 仍可禁用）。理由：与 omp 首事件/空闲看门狗 300s 对齐，长预填充/长思考不再被网关先杀。优先级：priming 只覆盖 commit 前，first-byte 只覆盖 commit 前首个 chunk，idle 只覆盖 commit 后；`prime >= first_byte`（均启用）时启动告警，FIRST_BYTE 对 SSE 退为兜底。默认配置下真卡死先表现为 `200 + 错误帧`，观测 504 须 `first_byte < prime`（取等仍由前武装的 deadline 先提交）或 `PRIME=0`。README 配置表同步。
3. **commit 后超时优雅收尾**：透传 SSE 的流包装 MUST 把 timeout/transport `Err` 转成一条**客户端方言对应**的错误帧（Chat：`data: {"error":{…}}`；Anthropic：`event: error` + `stream_error`；Responses：`event: error` + `stream_error`）后正常结束，禁止把 `Err` 交给 hyper 导致连接硬断。帧内 message MUST 经 `redact_secrets`；日志保留 request_id/provider/model。转换链路现有错误帧行为不变。
3a. **熔断净记账**：透明 SSE 不在 commit 处记成功，由错误尾结算（正常 EOF → 一次 success；首次终止 Err → 一次 failure；未结算即 drop → 释放半开探测名额），保证同一请求在窗口内只产生一个信号。
4. **失败分类**：priming 与 pre-response 阶段，流超时（`stream first byte timeout` / `stream idle timeout`）MUST 置 `saw_timeout`；无 `last_failure` 时的聚合保持 504/502/503 语义。priming 内“零产出 EOF / 错误 envelope”仍为 502 + `upstream_error_envelope`。
5. 不改动：整流白名单、路由与候选切换、非流路径与 `non_stream_timeout_secs`；熔断仅新增上述提交后结算语义。
6. 不得以“关闭全部超时/无限重试”实现；每 chunk 路径不新增分配/计时器重建。

### W2（迁移 + `src/store/users.rs`、`src/service/console.rs`、`src/api/console.rs`、`src/state.rs`、web）
1. 迁移 `0033_admin_source.sql`：
   - `ALTER TABLE users ADD COLUMN admin_ldap BOOLEAN NOT NULL DEFAULT FALSE, ADD COLUMN admin_override BOOLEAN;`
   - 回填：`admin_ldap = (source='ldap' AND is_admin)`；`admin_override = CASE WHEN source <> 'ldap' THEN is_admin ELSE NULL END`；`is_admin` 保持现值（等价于 `COALESCE(admin_override, admin_ldap)`）。回填 DML 仅执行一次；迁移前 LDAP 行上的手动授予/撤销无法与目录真值区分，回填为 `directory`，升级后首次登录按目录重算。
2. 有效权限恒等式：`is_admin = COALESCE(admin_override, admin_ldap)`，**所有**写路径（JIT / UI override / reset / `create_local_user` / groups 同步 / seed）都必须维护三列一致；`0034` 迁移增加 DB CHECK 兜底：
   - LDAP JIT（`upsert_ldap_user`）：`admin_ldap = EXCLUDED.admin_ldap`，`is_admin = COALESCE(users.admin_override, EXCLUDED.admin_ldap)`；不再无条件覆盖 `is_admin`。
   - UI 修改（`set_admin_override`）：写 `admin_override = $grant` 且 `is_admin = $grant`；新增“恢复目录同步”：`admin_override = NULL, is_admin = admin_ldap`，仅对 `source='ldap'` 目标开放（否则 400）。
   - `0034_admin_invariant.sql`：`CHECK (is_admin = COALESCE(admin_override, admin_ldap))`。
3. API：`PATCH /api/admin/users/{id}` 增加 `reset_admin_source: bool`（与 `is_admin` 互斥；二者皆缺时保持既有字段级 no-op 语义，仅更新 status 等其它字段；`reset` 对 `source != 'ldap'` 返回 400）；响应与列表 `user_json` 增加 `admin_source: "directory"|"manual"`。审计事件沿用 `user.is_admin`，reset 记 `user.admin_source.reset`。
4. 立即性：JIT 与 PATCH/reset 改变 `is_admin` 后**增量**同步 `admin_ids`（`AppState::sync_admin_id`：授予插入、撤销移除，撤销 fail-closed），不依赖全量 `reload()`；`reload_admin_ids` 用版本号守卫，在途快照不得覆盖并发变更。
5. UI（`web/src/views/Users.vue`、`web/src/api/types.ts`）：权限列显示来源（目录/手动），手动覆盖行提供“恢复目录同步”，删除“仅临时生效”提示；新增 `reset_admin_source` 请求。
6. 不纳入本次：用户名大小写规范化/唯一索引（A4 相关，需产品决策），记录于 §7。

## 5. Tasks（SDD，2 个可并行写任务）

| ID | Owner | Scope | 依赖 |
|---|---|---|---|
| T1 | fullstack-dev | W1：`src/service/proxy.rs`、`src/config.rs`、`scripts/mock_upstream.py`（新增长任务模式）、`src/service/proxy.rs` 单测、README 配置表 | 无 |
| T2 | fullstack-dev-2 | W2：`migrations/0033_admin_source.sql`、`src/store/users.rs`、`src/service/console.rs`、`src/api/console.rs`、`src/state.rs`、`web/src/views/Users.vue`、`web/src/api/types.ts`、README LDAP 语义、DB 测试 | 无 |

写所有权隔离：`fix/long-stream` 与 `fix/admin-source` 两个 worktree 从 `fix/long-task-admin` 分出；PM 负责合并到 `fix/long-task-admin` 后统一 QC。

## 6. Verification（可执行判据）

**W1**
- 单测：priming deadline 到期提交（空缓冲/中性帧 SSE）、非 SSE 不提前提交、超时→`saw_timeout`、透传流 Err→方言错误帧（脱敏、无 Err 泄漏）、熔断按真实序列每次请求只记一个信号（成功/失败/半开 drop 释放）、ping 只在行边界注入。
- e2e（mock 上游新增模式，`scripts/mock_upstream.py`）：①首字节 90s ②首 chunk 后静默 150s 再继续 ③持续中性帧 60s ④正常短流。判据：①②不产生 502、客户端在 ≤3s 内收到响应头、静默期每 ~15s 收到 `: ping`、最终完整结束；③不出现“无响应头”；真卡死（首字节 > 配置窗口，且 `first_byte < prime` 或 `PRIME=0`）在窗口内 504；默认配置下则为 `200 + 方言错误帧`。
- `cargo test`（既有 + 新增）、`cargo build` 无新增警告（基线既有 2 条 dead_code）；`scripts/test_conformance.py` 通过。
- 真实链路：经网关 `localtest/deepseek-flash` 跑一次长上下文请求（≥100K 输入）确认流式无中断；omp 魔棒同 provider 复测成功（<120s）。

**W2**
- 迁移幂等：空库与现有库各跑一次；回填后 `is_admin == COALESCE(admin_override, admin_ldap)`。
- e2e（ldaptest）：`alice` 手动授予 → 连续 3 次登录恒 true；`bob` 目录管理员 → 移出 `admins` 后再登录 false、加回 true；手动撤销恒 false；恢复目录同步后回到目录真值。
- PATCH 后立即 `GET /api/admin/users` 与代理旁路一致；reload 失败不产生 500（模拟）；在途 reload 的陈旧快照不得恢复已撤销的 admin id。
- 前端：来源列与“恢复目录同步”生效；`npm run build` 零错。

## 7. Deferred（可追踪，后续计划）

| 项 | 原因 | 触发/owner |
|---|---|---|
| 用户名大小写规范化 + `lower(username)` 唯一索引（`Alice`/`alice` 双行） | 需产品决策（目录规范名 vs 本地历史数据迁移） | 下一计划；先以审计日志观测 |
| 上游非重试 4xx（如 404）在 streaming 中被改写为 502 | 与本次长任务无关，涉及错误语义与客户端兼容 | 下一计划 |
| 前端 refresh 响应中的 `user` 未落 store（降权后幽灵菜单 ≤15min） | 非本次根因；W2 后手动覆盖不再被静默降级 | 观察后决定 |
| 流式 pre-response 多候选 header 超时叠加（4×120s） | 典型部署 1–2 候选，未触发本期症状 | 出现时按预算方案处理 |
| 仅中性帧的无限流（commit 后无产出） | 网关无“产出性空闲”计时；客户端 watchdog/断开兜底，终止性错误按净记账进熔断 | 下一计划（需评估逐帧解析成本） |
| 错误构造器统一（Anthropic 已逐字段一致；Responses 错误码已对齐 `stream_error`，终止事件名不同：透传 `event: error` vs 转换 `response.failed`） | 构造器仍按链路各自维护 | 下一计划 |
| 转换链（Anthropic/Responses）提交后传输失败不计熔断 | 既有行为，本次只覆盖透明 SSE | 下一计划（与构造器统一同批） |
| 半开探测名额占用整条透明 SSE 流且不绑定请求（并行 drop 可提前释放；pre-commit 断开不释放） | 只影响半开探测并发度，不绕过熔断；`release_probe` 仅清标志 | 下一计划（token/guard 归属） |
| 上游自报错误 message 进入 pre-commit 失败体时不做二次脱敏 | 需上游回显我方凭据才可泄露；当前零泄漏 | 观察 |
| `reload_admin_ids` 的 `seen_gen` 读序无测试锁定 | 静态审核确认正确；时序回归会被两向 helper 测试部分覆盖 | 下一计划（补 seam 测试） |
| 目录管理员“钉住为覆盖”交互 | UX 改进，非缺陷 | 有反馈时 |

## 8. 交付

feature 分支 `fix/long-task-admin` → 两任务 worktree → 合并 → SDD 每任务 review → QC tri-review（qc1/2/3 + consolidated）→ PM acceptance（上文判据）→ Done 汇总（无引擎，Done 记录在本文件尾部与提交信息）。
