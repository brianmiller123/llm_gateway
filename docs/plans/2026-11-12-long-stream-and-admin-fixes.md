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
1. **Priming 有界 + 尽早提交**：新增配置 `GATEWAY_STREAM_PRIME_TIMEOUT_MS`（默认 2000，0=不限）。priming 循环引入墙钟 deadline；仅当 `is_sse && buffered.is_empty()` 时到期即提交（保留“上游 200 但回 JSON 体”的既有识别路径：已有缓冲则继续原逻辑）。提交后客户端立刻拿到响应头与心跳。
2. **超时默认值**：`GATEWAY_STREAM_FIRST_BYTE_TIMEOUT` 默认 60 → 300，`GATEWAY_STREAM_IDLE_TIMEOUT` 默认 120 → 300（0 仍可禁用）。理由：与 omp 首事件/空闲看门狗 300s 对齐，长预填充/长思考不再被网关先杀。README 配置表同步。
3. **commit 后超时优雅收尾**：透传 Chat SSE 的流包装 MUST 把 timeout/transport `Err` 转成一条 SSE 错误帧（OpenAI 风格 `data: {"error":{message,type,code}}`）后正常结束，禁止把 `Err` 交给 hyper 导致连接硬断。转换链路（Anthropic/Responses）现有错误帧行为保持不变并复用同一判定。
4. **失败分类**：priming 与 pre-response 阶段，流超时（`stream first byte timeout` / `stream idle timeout`）MUST 置 `saw_timeout`；无 `last_failure` 时的聚合保持 504/502/503 语义。priming 内“零产出 EOF / 错误 envelope”仍为 502 + `upstream_error_envelope`。
5. 不改动：整流白名单、路由/熔断、非流路径与 `non_stream_timeout_secs`。
6. 不得以“关闭全部超时/无限重试”实现；每 chunk 路径不新增分配/计时器重建。

### W2（迁移 + `src/store/users.rs`、`src/service/console.rs`、`src/api/console.rs`、`src/state.rs`、web）
1. 迁移 `0033_admin_source.sql`：
   - `ALTER TABLE users ADD COLUMN admin_ldap BOOLEAN NOT NULL DEFAULT FALSE, ADD COLUMN admin_override BOOLEAN;`
   - 回填：`admin_ldap = (source='ldap' AND is_admin)`；`admin_override = CASE WHEN source='local' THEN is_admin ELSE NULL END`；`is_admin` 保持现值（等价于 `COALESCE(admin_override, admin_ldap)`）。
2. 有效权限恒等式：`is_admin = COALESCE(admin_override, admin_ldap)`，两处写路径都必须维护三列一致：
   - LDAP JIT（`upsert_ldap_user`）：`admin_ldap = EXCLUDED.admin_ldap`，`is_admin = COALESCE(users.admin_override, EXCLUDED.admin_ldap)`；不再无条件覆盖 `is_admin`。
   - UI 修改（`set_is_admin`）：写 `admin_override = $grant` 且 `is_admin = $grant`；新增“恢复目录同步”：`admin_override = NULL, is_admin = admin_ldap`。
3. API：`PATCH /api/admin/users/{id}` 增加 `reset_admin_source: bool`（与 `is_admin` 互斥，二者皆缺 400）；响应与列表 `user_json` 增加 `admin_source: "directory"|"manual"`。审计事件沿用 `user.is_admin`，reset 记 `user.admin_source.reset`。
4. 立即性：JIT 与 PATCH 改变 `is_admin` 后即时刷新 `admin_ids`（复用 `state.reload()` 的 admin_ids 分节或等价轻量方法）；`reload()` 失败只 warn，不把已提交的 PATCH 变成 500。
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
- 单测：priming deadline 到期提交（空缓冲 SSE）、有缓冲不提前提交、超时→`saw_timeout`、透传流 Err→SSE 错误帧（无 Err 泄漏）、ping 只在行边界注入。
- e2e（mock 上游新增模式，`scripts/mock_upstream.py`）：①首字节 90s ②首 chunk 后静默 150s 再继续 ③持续中性帧 60s ④正常短流。判据：①②不产生 502、客户端在 ≤3s 内收到响应头、静默期每 ~15s 收到 `: ping`、最终完整结束；③不出现“无响应头”；真卡死（如首字节 > 配置窗口）在窗口内 504。
- `cargo test`（既有 + 新增）、`cargo build` 零警告；`scripts/test_conformance.py` 通过。
- 真实链路：经网关 `localtest/deepseek-flash` 跑一次长上下文请求（≥100K 输入）确认流式无中断；omp 魔棒同 provider 复测成功（<120s）。

**W2**
- 迁移幂等：空库与现有库各跑一次；回填后 `is_admin == COALESCE(admin_override, admin_ldap)`。
- e2e（ldaptest）：`alice` 手动授予 → 连续 3 次登录恒 true；`bob` 目录管理员 → 移出 `admins` 后再登录 false、加回 true；手动撤销恒 false；恢复目录同步后回到目录真值。
- PATCH 后立即 `GET /api/admin/users` 与代理旁路一致；reload 失败不产生 500（模拟）。
- 前端：来源列与“恢复目录同步”生效；`npm run build` 零错。

## 7. Deferred（可追踪，后续计划）

| 项 | 原因 | 触发/owner |
|---|---|---|
| 用户名大小写规范化 + `lower(username)` 唯一索引（`Alice`/`alice` 双行） | 需产品决策（目录规范名 vs 本地历史数据迁移） | 下一计划；先以审计日志观测 |
| 上游非重试 4xx（如 404）在 streaming 中被改写为 502 | 与本次长任务无关，涉及错误语义与客户端兼容 | 下一计划 |
| 前端 refresh 响应中的 `user` 未落 store（降权后幽灵菜单 ≤15min） | 非本次根因；W2 后手动覆盖不再被静默降级 | 观察后决定 |
| 流式 pre-response 多候选 header 超时叠加（4×120s） | 典型部署 1–2 候选，未触发本期症状 | 出现时按预算方案处理 |

## 8. 交付

feature 分支 `fix/long-task-admin` → 两任务 worktree → 合并 → SDD 每任务 review → QC tri-review（qc1/2/3 + consolidated）→ PM acceptance（上文判据）→ Done 汇总（无引擎，Done 记录在本文件尾部与提交信息）。
