# Task review — T2

LDAP 管理员双来源（目录真值 + 控制台手动覆盖）。实现者 `fullstack-dev-2`（`history://LdapAdminFix`）。

## Scope

- Diff range: `a745524..39a59ec`（1 commit `39a59ec fix(admin): track LDAP directory vs manual admin grants`，9 文件，+290/−35）。
- 工作树：`/home/pop/AI/code/llm_gateway.wt/integration`（分支 `fix/long-task-admin`，T1+T2 已合并）。
- 打开的文件：
  - `git diff a745524..39a59ec` 全量（9 文件）
  - `migrations/0033_admin_source.sql`（全文，14 行）
  - `src/store/users.rs:1-190,186-220,297-330`、`src/store/groups.rs:104-163`、`src/store/mod.rs:30-70`
  - `src/api/console.rs:1206-1212,1285-1400,1601-1615`、baseline `a745524:src/api/console.rs:1284-1340`
  - `src/service/console.rs:19-93`、`src/service/plans.rs:135-150`
  - `docs/plans/2026-11-12-long-stream-and-admin-fixes.md:15-26,40-103`
  - `migrations/0001_init.sql:3-16`（users 表定义，用于回填穷尽性判断）
- 未跑 build / test / npm（PM 已提供集成构建 41s、`cargo test --bin llm_gateway` 306/306、`npm run build`）。
- 指派问题全部回答，无截断。

**写路径枚举（不变式审计基线）**——`users.is_admin` 的全部写入点：

| # | 写路径 | 位置 | 实际 SQL |
|---|---|---|---|
| 1 | 本地建号 | `src/store/users.rs:65` | `INSERT (…, is_admin, admin_override) VALUES (…,$4,$4)` |
| 2 | LDAP JIT | `src/store/users.rs:89` | `INSERT … is_admin=$5, admin_ldap=$5` / `ON CONFLICT: admin_ldap=EXCLUDED.admin_ldap, is_admin=COALESCE(users.admin_override, EXCLUDED.admin_ldap)` |
| 3 | 手动授予/撤销 | `src/store/users.rs:163` | `SET admin_override=$1, is_admin=$1` |
| 4 | 恢复目录同步 | `src/store/users.rs:173` | `SET admin_override=NULL, is_admin=admin_ldap` |
| 5 | LDAP 组同步建档 | `src/store/groups.rs:124` | `INSERT … is_admin FALSE`；`ON CONFLICT` 不触碰任何 admin 列 |
| 6 | break-glass 种子 | `src/store/mod.rs:54` | 经 `create_local_user(…, true)`（即路径 1） |

仓库内无其他 `INSERT INTO users` / `UPDATE … is_admin` 写点（`grep -rn is_admin src/` 余下命中均为读：`api/v1.rs:226`（代理旁路读 `admin_ids`）、`api/console.rs:139`（管理员中间件）、`service/notify.rs:220`、`service/session.rs:46`、`api/console.rs` 的 `user_json`/列表）。

## Verdict: **Needs-fix**

不变式本身在六条写路径上成立，无 Critical。但存在 1 条可达的 API 契约漂移（#1）与 1 条未登记的计划偏离（#2），两者都很便宜，建议在进入 QC 前收敛。

## Findings

### Warning

**W1 — 本地账号 reset 后 `admin_source` 报 `directory`，与自身契约矛盾**

- 位置：`src/store/users.rs:151-156`（`admin_source(Option<bool>)`）、`src/store/users.rs:172-176`（`reset_admin_source`）、`src/api/console.rs:1350-1353`（reset 分支不校验 `source`）、`src/api/console.rs:1610` / `1209`（输出）、`web/src/api/types.ts:11-12,229-230`（类型文档）。
- 证据：`reset_admin_source` 的 SQL 为 `SET admin_override = NULL, is_admin = admin_ldap`。对 `source='local'` 的行（`admin_ldap` 恒 false）执行后得到 `override=NULL, is_admin=false`；`admin_source()` 只看 `admin_override`，因此返回 `"directory"`。而同文件 `users.rs:154` 的文档注释明确写 `directory` 是「仅 LDAP 用户可处于此态」，`types.ts` 写 `directory = 跟随 LDAP 目录组`。`src/api/console.rs:1302` 的互斥检查与 `1350` 的 reset 分支都没有 `source != 'ldap'` 守卫，所以 `PATCH {"reset_admin_source": true}` 对本地账号返回 200 并落到上述状态；前端只在 `row.source === 'ldap'` 时显示按钮（`web/src/views/Users.vue:380`），故该路径仅 API 可达。
- 影响：`/api/me`、登录/刷新响应（`src/api/console.rs:189,208,245`）与用户列表对本地账号给出「跟随目录」的来源标注，API 消费者与后续代码（如按 `admin_source` 判断是否需要重新授予）会被误导；该状态无目录真值可回归，只能靠再次 `set_is_admin` 恢复。
- 建议（二选一，都很小）：reset 分支对非 `source='ldap'` 目标返回 400（与前端 gating 对齐）；或把 helper 改为 `admin_source(source: &str, admin_override: Option<bool>)`，对非 LDAP 行恒返回 `manual`。若选择后者，请在计划 §7 登记「本地行 reset 后遗留 `admin_override=NULL`」的既有状态（当前生产库无此状态，因 UI 无入口）。

**W2 — 计划 §4.3「二者皆缺 400」未实现，且未在实现者报告中登记为偏离**

- 位置：`src/api/console.rs:1302-1307`（只做互斥）、`1338-1360`（`status`/`is_admin`/`reset` 三者皆缺时无任何校验）。
- 证据：计划 `docs/plans/2026-11-12-long-stream-and-admin-fixes.md:65` 写「`reset_admin_source: bool`（与 `is_admin` 互斥，二者皆缺 400）」。实现为：`{"is_admin":…,"reset_admin_source":…}` → 400（有）；两者皆缺 → 200 no-op（无 400）。baseline `a745524:src/api/console.rs:1284-1296` 也确实没有「空请求体 400」，`PATCH {status}` 是既有且必需的语义（启用/禁用前端在用），字面实现会直接打断它。
- 影响：不实现的判断是正确的（实现字面要求会引入回归），但计划判据与代码不一致，且实现者只在 e2e 记录里写「existing no-op semantics preserved」，没有作为 spec deviation 提交，QC/PM 会按计划字面复检并判失败。
- 建议：PM 修正计划 §4.3 措辞（例如「二者互斥 400；`status` 亦可单独提交，全空为 no-op」），或在实现者报告中登记偏离。本条**不需要改代码**。

**W3 — 迁移回填无法区分「迁移前的手动授予/撤销」，LDAP 行在迁移后首次登录会静默变权**

- 位置：`migrations/0033_admin_source.sql:12-14`。
- 证据：`admin_ldap = (source='ldap' AND is_admin)`、`admin_override = CASE WHEN source='local' THEN is_admin ELSE NULL END`，即所有 LDAP 行一律 `override=NULL`。所以 (a) 迁移前被手动授予、但不在 `LDAP_ADMIN_GROUPS` 的 LDAP 行，回填为 `ldap=true`，下次 JIT 登录把 `admin_ldap` 写成实际组值 false → 静默降权；(b) 迁移前被手动撤销、但在组内的 LDAP 行回填为 `ldap=false`，下次登录被静默重新授予。该行为与计划 §4.1 的回填定义完全一致（不是实现偏离），但 README 的改动只写了运行期语义（`README.md:25`、`:133`），没有任何迁移面提示，实现者把它放在「Issues/Risks」第 4 条而没有落到计划的 residual/issue 载体。
- 影响：升级后第一个登录窗口内可能发生无声的权限变更；对「被降权」方向，管理员只会看到自己突然不是管理员。break-glass 本地管理员路径独立（`src/service/console.rs:34` `skip_ldap`），不会锁死入口，故不阻断。
- 建议：把该情形写成计划 §7 / issue 一行（owner=PM，触发=生产升级），并在 README 的 LDAP 段落或迁移注释里补一句「升级前手动设置过的 LDAP 管理员将被目录真值接管，需在升级后用『恢复目录同步』或重新手动授予确认」。

### Suggestion

**S1 — 回填 DML 不可重跑（列变更幂等，数据变更不幂等）**

- 位置：`migrations/0033_admin_source.sql:6-8`（列，`IF NOT EXISTS` ✓）与 `:12-14`（无条件 `UPDATE users SET …`）。
- 证据：再次执行文件会把所有行的 `admin_override` 清成 `CASE …`（LDAP 行 → NULL）并据当前 `is_admin` 重写 `admin_ldap`，从而抹掉 0033 之后写入的所有手动覆盖。`sqlx::migrate!` 的版本表使常规运行不会重跑，故计划 §6「空库与现有库各跑一次」的字面判据是满足的（实现者两库各跑一次，证据见其报告 §Validation 1 与 e2e 的 `llm_gateway_admin_e2e`）。
- 影响：仅对手工重放该 SQL 的运维场景。
- 建议：注释里写明「本文件的数据回填只允许执行一次」。

**S2 — 回填的 `CASE` 未覆盖第三种 `source`**

- 位置：`migrations/0033_admin_source.sql:14`；`migrations/0001_init.sql:9`（`source VARCHAR(16) NOT NULL DEFAULT 'ldap'`，无 CHECK）。
- 证据：只有 `source='local'` 会拿到非空 override；若将来出现第三种 source 且该行为管理员，回填后 `override=NULL, admin_ldap=false` 而 `is_admin=true`，不变式立即被破坏。当前所有写点只产生 `'local'` / `'ldap'`（见 Scope 表），因此不可达。
- 建议：写成 `CASE WHEN source <> 'ldap' THEN is_admin ELSE NULL END`，让不变式对任意 source 都成立。

**S3 — 首次 LDAP 登录会多跑一次 `load_admin_ids`**

- 位置：`src/service/console.rs:53`（`if was_admin != Some(user.is_admin)`）。
- 证据：新用户 `was_admin = None`，而 `user.is_admin` 通常是 `Some(false)`，`None != Some(false)` 为真 → 每次「目录里首次出现且非管理员」的登录都触发一次 `SELECT id FROM users WHERE is_admin AND status = 1`。功能无害（失败也只 warn）。
- 影响：一次多余查询 + 一次写锁；新用户首登集中时（组同步后批量首登）会放大。
- 建议：条件改为 `was_admin != Some(user.is_admin) && (user.is_admin || was_admin == Some(true))`（只有「成为管理员」或「不再是管理员」才需要刷新缓存）。

**S4 — 不变式测试只在末尾断言一次，部分回归会被自愈掩盖**

- 位置：`src/store/users.rs:298-330`（`is_admin_equals_coalesce_of_source_columns`）。
- 证据：断言 `SELECT COUNT(*) … WHERE is_admin <> COALESCE(admin_override, admin_ldap)` 只在所有写操作之后执行一次；且在断言前 `upsert_ldap_user("bob", …, true)` 会把 bob 的 `is_admin` 重算为 `COALESCE(NULL,true)=true`。因此「`set_is_admin` 只写 `is_admin` 不写 `admin_override`」这类回归在该测试中不可见（carol 的 `reset` 能捕获 reset 侧回归；set_is_admin 侧由 `manual_override_survives_ldap_login:248-265` 的 `admin_override == Some(true)` 断言兜住）。三条行为测试本身是 fails-first 的：`ldap_admin_follows_directory_group:229-246` 在旧代码（无条件 `is_admin = EXCLUDED.is_admin`）下必失败，`manual_override_survives_ldap_login:248-265` 在旧代码下第 2 轮即失败 —— 与基线缺陷一一对应，不是 tautology。
- 建议：抽 `async fn assert_invariant(&pool)`，在每条写路径之后各断言一次（尤其 `set_is_admin(…,true)` 之后），让该测试真正覆盖「所有写路径」。

**S5 — 目录管理员无法被「钉住」为手动覆盖，只能先撤销**

- 位置：`src/api/console.rs:1341-1343`（`target.is_admin == grant → None`，不写 override）、`web/src/views/Users.vue:376-378`（按钮文案随 `is_admin` 翻转）。
- 证据：对「在目录组内的管理员」点「设为管理员」不可达（按钮已变「取消管理员」）；即便构造 `{"is_admin": true}`，因有效值相同也不会写 `admin_override`。想为某人上双保险（先授予覆盖，再让其留在组内）只能「取消管理员」（瞬时生效）→「设为管理员」。
- 影响：功能可用但有一次瞬时空窗；与「手动授予 = 显式覆盖」的语义略有不一致（有效值相同时的授予不算覆盖）。
- 建议：`grant == true && target.admin_override.is_none()` 时仍执行 `set_is_admin`（写入 override=true 并审计），或在 UI 上给目录来源行提供「固定为手动」入口。

**S6 — 来源标签与 reset 按钮的渲染条件不一致（含本地账号 tooltip 文案）**

- 位置：`web/src/views/Users.vue:323-337`（标签 `v-if="row.is_admin"`）、`:380`（按钮 `source==='ldap' && admin_source==='manual'`，不看 `is_admin`）、`:328-329`（tooltip「权限由控制台手动设置（覆盖目录真值）」）。
- 证据：被手动撤销的目录管理员 `is_admin=false, admin_source='manual'` → 角色列只显示「成员」，但操作列出现「恢复目录同步」，用户看不出这一行处于手动覆盖态；本地管理员（`create_local_user` 写入 `admin_override=Some(true)`）来源为 `manual`，标签 tooltip 却说「覆盖目录真值」，而本地账号没有目录真值。
- 建议：标签改为「管理员时显示来源，非管理员但为手动覆盖时显示『已撤销(手动)』」这类区分；tooltip 文案按 `row.source` 分支。

## Evidence check

可复核且与 diff 自洽的：

- 迁移列类型/默认值/`IF NOT EXISTS` 幂等（`:6-8`）；回填公式与计划 §4.1 逐字一致；实现者贴的 psql 表（6 行，`alice ldap t / admin local t / carol ldap f`）与回填公式推导一致，`invariant` 列全 t。
- 语义表（目录/手动/reset/本地）与代码逐项对得上：`set_is_admin` 写覆盖（`users.rs:163`）、reset 清覆盖回目录（`:173`）、JIT 只在无覆盖时跟随目录（`:97`）、本地权限存覆盖列（`:65`）。
- API：互斥 400（`api/console.rs:1302`）、reset 只认 `Some(true)`（`:1350`，`false` 为 no-op）、审计事件名 `user.is_admin` / `user.admin_source.reset` 与计划 §4.3 一致、detail 含 `is_admin`/`admin_source`/`directory_admin`/`username`（`:1370-1377`）；`reload_admin_ids` 失败只 `tracing::warn!` 且发生在写入之后（`:1362-1365`）——「已提交的 PATCH 不因刷新失败变 500」由代码形状保证，且相对 baseline（`st.reload().await.map_err(AppError::internal)?`）是真实修复。
- JIT 路径（`service/console.rs:39-58`）：预读旧值 → upsert → 值变化才刷新，刷新失败仅 warn，不阻断 `issue_session`；`reload_admin_ids`（`state.rs:211-216`）与 `reload()` 的 `admin_ids` 分节共用同一实现，无重复逻辑。
- 并发正确性：两条可变路径都在**单条 SQL 内**由另一列推导 `is_admin`（`COALESCE(users.admin_override, …)` / `is_admin = admin_ldap`），行锁串行化后不存在「JIT 与 PATCH 交错导致三列不一致」的窗口（这是我重点验的一项，结论：成立）。
- 前端 gating 与后端语义：reset 按钮 `source==='ldap' && admin_source==='manual'`；`types.ts` 两处 `admin_source` 均为非可空联合类型，后端 `user_json`（登录/刷新/me）与列表都恒定输出该字段（`api/console.rs:189,208,245,1209,1610`），类型可空性正确。

无法由本席位复核（未运行，依赖 PM/实现者证据）：306/306 与 4 个 `#[sqlx::test]` 是否真被执行、`npm run build`、e2e 输出本身（端口 8453 服务已停、`llm_gateway_admin_e2e` 已 drop，无残留工件可验）。

不自洽 / 未满足的计划判据：

- **计划 §6「reload 失败不产生 500（模拟）」未模拟**：实现者在 Issues 明确写「guaranteed by code shape; not force-simulated」，计划判据要求的是模拟。代码形状确实保证 warn-only（已核），但这是判定证据的缺口，需 PM 在 acceptance 记录里显式接受或补一次 DB 中断演练。
- **计划 §6「PATCH 后立即 `GET /api/admin/users` 与代理旁路一致」只验到 `/api/me`**：e2e 表里有 `PATCH … → 200 → /api/me`，没有一次「代理请求走管理员旁路」的观测。`admin_ids` 刷新逻辑本身正确，缺的是端到端观测（见 W2/S3 区域，非代码缺陷）。
- **风险描述不准确（旧二进制共存）**：实现者写「the old binary … can leave `admin_ldap` stale（effective permission stays correct because no overrides exist for those rows）」。前半句对，后半句只在「旧二进制运行期间」成立：回滚窗口内用旧控制台做的授予/撤销只写 `is_admin`，`admin_ldap` 不更新也不产生 `admin_override`；重新升回新二进制后的下一次 LDAP 登录会用目录真值覆盖 `is_admin`，即 W2 要修的静默丢失在回滚窗口内被完整复现。建议把风险改写为「0033 之后的回滚窗口内的管理员变更会在重新升级后于下次登录丢弃；若必须回退，先记录当时的 `is_admin` 以便回填 `admin_override`」。
- 迁移与 e2e 分属两个库（现有库只跑迁移、登录场景跑独立库 `llm_gateway_admin_e2e`）：偏离已披露且理由成立（共享库的 `system_settings` 覆盖 env，改配置会污染在线网关），不影响结论，但意味着「现有库上的迁移」只验证了 DDL/回填，没有验证该库在 LDAP 登录下的三态行为。

## Residuals

- **R1 `R-lag-admin-ids-on-status`**（pre-existing, low）：`set_status`/禁用不刷新 `admin_ids`，被禁用的管理员在 ≤`GATEWAY_RELOAD_INTERVAL` 内仍可通过代理旁路（`api/v1.rs:226` 读缓存）。实现者已披露；不在本任务范围，需进计划 residual/issue 载体。
- **R2 迁移后首次登录的静默变权**（= W3）：与计划一致，需在计划 §7 或 issue 登记 owner/触发条件。
- **R3 旧二进制/回滚窗口**（见 Evidence check 第三条）：不构成当前代码缺陷；需在 README/运维说明或计划 residual 里写清「0033 之后不建议回退」。
- **R4 `admin_source` 的 `directory` 语义对本地账号无意义**（= W1）：即使修正 helper，仍需决定是否为已存在的 `local + admin_override IS NULL` 行做数据修正（当前生产库无此状态）。
- **R5 目录管理员无法钉住覆盖**（= S5）、**S1/S2/S4/S6** 属小改进，可交由计划 QC 的 minor 清单统一处置。
- 明确排除：A4（用户名大小写规范化）与 local→LDAP source 转换，计划 §4.6/§7 已 deferred，本次未触碰（diff 内无相关改动，已核）。
