# Coding Plan 小时级重置：分析与设计

状态：已实现（`migrations/0021_hourly_period.sql` + 后端/前端配套变更）
范围：为 Coding Plan 增加 `hourly` 统计周期，支持每 N 小时（1..168）重置，锚点可选
UTC 整点网格或按用户开通时间偏移。

---

## 1. 现状：重置机制的完整实现

### 1.1 核心结论：重置是「推导」出来的，不是「调度」出来的

现有系统没有任何重制定时任务。周期翻转完全由**纯时间函数**在读取路径上推导：
统计窗口 = `period_type` + 墙钟（UTC）的确定函数，写入时按当时窗口落桶，
读取时只看当前窗口——旧窗口的行自然变成历史，无需任何进程去「清零」。

### 1.2 触发条件（周期翻转的判定点）

| 判定点 | 位置 | 机制 |
|---|---|---|
| 请求期配额检查 | `service/plans.rs::check_plan` | 用 `PlanRuntime::period_key(now)` 查缓存，key 不匹配视为新周期 → 用量 0 |
| 请求计量载荷 | `proxy.rs`（`PlanBill` 构造，约 :851） | 请求时刻解析 `period_start`/`period_key` 随请求携带 |
| 记账落桶 | `store/usage.rs::record_usage` | `plan_usage_counters` 按 `(user_id, plan_id, period_start)` UPSERT，新周期自然写新行（与 usage_logs 同事务） |
| 内存计数缓存 | `service/usage.rs::UsageCache::{get_plan,incr_plan}` | `(user, plan) → (period_key, tokens)`；key 翻转时以本笔增量重置 |
| 缓存兜底重载 | `state.rs::spawn_reload_tasks`（每 `GATEWAY_RELOAD_INTERVAL` 秒）+ 启动时 | `load_usage_snapshots` 只回填**当前窗口**的行，DB 真值覆盖内存 |
| 告警去重重挂 | `service/notify.rs` + `plan_alerts` UNIQUE(plan_id,user_id,period_key,level) | period_key 进入去重键，新周期告警自动重新武装 |

### 1.3 定时任务清单（均不直接参与重置）

- `state.rs::spawn_reload_tasks`：配置热加载 + 用量缓存兜底（周期 = `reload_interval_secs`）
- `worker/aggregator.rs`：usage_daily 聚合（60s）+ refresh token 清理（10 分钟）
- `worker/plan_sync.rs`：LDAP 分组增量对账（600s）

### 1.4 配额与用量数据结构

- `coding_plans`：`token_limit BIGINT`、`period_type IN ('daily','monthly','total')`、
  `overage_action IN ('block','downgrade','log')`
- `plan_usage_counters`：PK `(user_id, plan_id, period_start)`，`tokens` 饱和累加；
  plan_id 无 FK（删 Plan 保留历史）
- `plan_alerts`：阈值告警，`(plan_id,user_id,period_key,level)` 唯一去重
- 内存：`UsageCache.plan_usage`、`AppState.plan_alerts_seen`（进程内去重，可丢失）
- 用户 → 生效 Plan：`load_plan_runtimes` 多分组取 `priority` 最高、同分取 `plan_id` 大

### 1.5 已有语义要点

- **check-then-act 溢出**：流式请求 usage 在流结束才落账，预检查后单请求可使计数
  溢出上限；超出部分计入本周期，下个请求按新余量处置（`service/plans.rs` 模块注释）
- **全 UTC**：daily=当日、monthly=自然月、total=epoch，无视本地时区
- **饱和加法**：上游 usage 为外部输入，`saturating_add` 防 i64 回绕

---

## 2. 小时重置设计

### 2.1 模型

新增 `period_type = 'hourly'`，语义为**滚动窗口桶**（rolling bucket），而非滑动窗口：
时间轴按锚点切成连续的 N 小时桶，请求计入其开始时刻所在的桶，桶翻开后旧桶永久封存。

- `period_hours INT`：窗口长度，1..=168（上限一周）
- `period_anchor_mode`：
  - `fixed`：锚点 = epoch（1970-01-01T00:00:00Z）。桶起点落在 `epoch + k·N·h` 的
    UTC 整点网格上。**注意**：仅当 N 整除 24 时桶界才与每日 00:00 对齐；N=5/7 等
    会在跨日时相位漂移（纯网格，不按自然日重排）——这是有意行为，保证「每 N 小时」
    全局均匀无重叠。
  - `join`：锚点 = 成员加入分组时刻 `user_group_members.added_at`（即「开通时间」）。
    桶起点 = `added_at + k·N·h`。成员所有插入路径均为 `ON CONFLICT DO NOTHING`，
    added_at 稳定；成员被移除后重新加入会得到新 added_at，等价于重新开通（新桶、
    余量重置），符合直觉。

### 2.2 重置时点的确定规则（唯一实现）

```rust
// store/plans.rs::PlanRuntime::period_start —— 所有周期类型统一返回窗口起点时刻
PERIOD_HOURLY => {
    let anchor_ts = match self.period_anchor_mode.as_str() {
        ANCHOR_JOIN => self.member_since.timestamp(),  // 截断到整秒（与 DB 键一致）
        _ => 0,                                        // fixed：epoch 网格
    };
    let step = (self.period_hours.max(1) as i64).saturating_mul(3600);
    let elapsed = (now.timestamp() - anchor_ts).max(0); // 时钟早于锚点钳为 0
    DateTime::from_timestamp(anchor_ts + elapsed / step * step, 0).unwrap()
}
```

- `period_key = "h:" + 桶起点 epoch 秒`，与 `plan_usage_counters.period_start`
  （TIMESTAMPTZ）一一对应；DB 侧用 `'h:' || extract(epoch from period_start)::bigint`
  生成同构键（`load_usage_snapshots`），两侧整秒运算严格一致。
- daily/monthly/total 的键与起点表示不变（daily 起点 = 当日 00:00 UTC 的时刻值）。

### 2.3 与 daily/monthly（及 weekly）的优先级与兼容关系

- **现状澄清**：现有系统实际为 `daily / monthly / total` 三态，**没有 weekly**；
  本次未引入 weekly（加 weekly 与加 hourly 同构，仅需再扩 CHECK + period_start
  分支，见 §7）。
- **单 Plan 生效模型不变**：用户同时命中多个分组时，按 `priority` 最高、同分
  `plan_id` 大者整体生效——hourly 不与 daily/monthly **叠加**计量，是普通的一等
  周期类型。需要「小时 + 月」双重限值的用户，应拆两个分组并用 priority 表达取谁。
- 若产品后续需要叠加式多周期配额（同时受小时桶与月桶约束），需要把
  `PlanRuntime`/`PlanBill` 升级为多配额列表并逐桶检查——超出本次范围，已列为风险
  备选（§7）。
- 周期切换兼容性：管理员把在用 Plan 的 period_type 改为 hourly（或改 N/锚点）后，
  下一请求按新规则解析出新的 period_start/period_key → 内存缓存判翻转变为 0、
  计数器开新行。旧周期行保留可回溯，**不迁移不清洗**。

### 2.4 「调度」：仍无需任何重置任务

小时桶与日/月桶同构：翻桶是读取路径推导结果。`plan_alerts_seen`（进程内去重集）
随流量按桶增长（每用户×级别每小时至多 3 条），量级有界，可接受。

---

## 3. 变更模块清单

| 层 | 文件 | 变更 |
|---|---|---|
| 迁移 | `migrations/0021_hourly_period.sql` | period_type CHECK 扩 'hourly'；新增 `period_hours`（1..168 CHECK）、`period_anchor_mode`（fixed/join CHECK）；`plan_usage_counters.period_start` DATE → TIMESTAMPTZ（显式 `AT TIME ZONE 'UTC'` 换算） |
| 存储层 | `store/plans.rs` | `PERIOD_HOURLY`/锚点常量/`MAX_PERIOD_HOURS`/`validate_period_config`；`CodingPlan`/`PlanSummary`/`PlanRuntime`/`RuntimeRow` 增字段；`period_start`/`period_key` 重写；`load_plan_runtimes` 联取 `m.added_at AS member_since`；create/update SQL 增列（update 走 COALESCE 部分语义）；`list_plan_summaries`/`load_usage_snapshots` 当前桶谓词重写；`PeriodUsage.period_start` → `DateTime<Utc>` |
| 记账 | `store/usage.rs` | `PlanBill.period_start` NaiveDate → `DateTime<Utc>`（UPSERT 绑定处类型随之） |
| 告警 | `service/notify.rs` | `period_label` 增 hourly 案（"本时段"） |
| API | `api/plans.rs` | `PlanCreateReq`/`PlanUpdateReq` 增 `period_hours`/`period_anchor_mode`；create 直接校验；update 按「请求字段 ∪ 现值」合并校验；`validate_plan_fields` 剥离 period 校验；`my_plan` JSON 增两字段 |
| 代理热路径 | `proxy.rs` | 零改动（`PlanBill` 由 `period_start(now)` 派生，类型贯通） |
| 前端 | `web/src/api/types.ts`、`Plans.vue`、`MyPlan.vue`、`PlanMonitor.vue` | `PeriodType` 增 'hourly'、`AnchorMode` 类型；表单增「小时级重置」选项 + 重置间隔（1..168）+ 锚点选择；周期列/我的套餐文案 `每 N 小时重置` |

### 关键 SQL（当前桶谓词，时区安全）

```sql
-- 日期/月份界显式 UTC（裸 CURRENT_DATE/date_trunc 会随会话时区漂移）
(p2.period_type = 'daily' AND pc.period_start =
    date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC')
-- hourly：开区间窗 (now - N·h, now] 恰好且必然只命中当前桶
(p2.period_type = 'hourly'
    AND pc.period_start > now() - make_interval(hours => p2.period_hours)
    AND pc.period_start <= now())
```

开区间窗正确性：当前桶起点满足 `bucket ≤ now < bucket + N·h`，等价于
`bucket > now − N·h AND bucket ≤ now`；上一桶起点 `= bucket − N·h ≤ now − N·h`
被严格大于排除。无需在 SQL 里重算每计划的锚点偏移，谓词与 N、锚点解耦。

---

## 4. 边界场景

### 4.1 跨时区与夏令时

- 全部桶运算在 **UTC** 上进行（chrono `DateTime<Utc>` + Postgres TIMESTAMPTZ），
  DST 对 UTC 无效应，不存在跳变/重复小时。
- fixed 锚点的桶界是 UTC 整点（或 UTC epoch 网格）；对 UTC+8 用户即本地 08:00 起的
  整点，不提供本地时区锚点（如需要，扩展 `period_anchor_mode` 增加
  `fixed_local` + 每计划 IANA 时区列即可，见 §7）。
- DB 侧日期/月份界与 to_char 键均显式 `AT TIME ZONE 'UTC'`，已实证在
  `America/New_York`、`Pacific/Auckland` 会话时区下结果与 UTC 一致（集成验证 §6）。

### 4.2 重启 / 宕机错过翻桶的补偿

- **无需补偿任务**：`period_start(now)` 是纯时间函数，重启后首个请求按墙钟直接命中
  当前桶；错过的整段桶不存在「待执行动作」——计数器行从未写入即无需清零。
- 记账一致性：计数器与 usage_logs 同事务 UPSERT（`record_usage`），崩溃只丢未提交
  请求的计量（与明细一致地丢失，不产生偏差）；缓存丢失由启动时
  `reload_from_db` 从 DB 回填。
- **时钟回拨/未来锚点**：`elapsed.max(0)` 钳位，桶起点 = 锚点本身，不产生负槽位。

### 4.3 并发写入的幂等性

- 翻桶判定是纯函数，多实例并发计算同一 `period_start`，天然幂等；
- 计数器 UPSERT `ON CONFLICT (user_id, plan_id, period_start) DO UPDATE ... tokens =
  tokens + EXCLUDED.tokens` 在行锁上串行累加，与 usage_logs 同事务原子提交；
- 告警三层去重（进程内 seen / DB UNIQUE(period_key, level) / delivered 回写）在小时
  粒度下语义不变；
- check-then-act 的单请求溢出是**既有已接受语义**，小时桶不放大它（溢出同样封顶在
  本桶内）。

---

## 5. 测试

### 5.1 单元测试（`store/plans.rs::tests`，220 通过）

覆盖：fixed 整点入桶/桶内任意时刻（N=1）；fixed N=5 的跨日网格相位（含
86400 % 18000 ≠ 0 的漂移实例值）；N=7 非整除 24 的纯网格属性；join 锚点 14:37:05
的桶起点序列与整点边界（16:37:04 vs 16:37:05）；未来锚点钳位；`h:` 键 = 桶起点
epoch 且相邻桶键不同（告警重挂）；legacy daily/monthly/total 起点/键回归；
`validate_period_config` 边界（0/169 拒绝、1/168 通过、非法锚点/周期拒绝）；
`(now − N·h, now]` 窗口对当前桶命中、上一桶排除的谓词等价性。

### 5.2 集成验证（已执行，scratch DB 全链）

1. `migrations/0001→0021` 在空库顺序应用全部成功（psql ON_ERROR_STOP）。
2. 固定数据双桶（当前 + 上一桶）× 三种计划（hourly fixed N=2 / hourly join N=24 /
   daily）验证 `load_usage_snapshots` 同款 SQL：仅当前桶返回、键格式
   `h:<epoch>` / `d:YYYY-MM-DD` 与 Rust 端一致；在 `America/New_York` 会话时区下
   结果不变。
3. `list_plan_summaries` 同款 SQL 在 `Pacific/Auckland` 时区下当期聚合正确，
   新列正常返回。
4. `plan_period_history` 在 TIMESTAMPTZ 上逐桶返回历史。
5. 新二进制对 scratch 库完整启动（自动迁移 + 种子 + worker + `/api/status` 200）。

### 5.3 建议的后续用例（可选，CI 化）

- 端到端：mock 上游造 100% 阈值 → 拦截 429 → 用 `period_hours=1` 的 Plan 等待
  （或注入时钟）翻桶 → 请求放行；断言 `plan_alerts` 在新 period_key 重新出现。
- 并发：同用户 16 并发记账 → 断言计数器 tokens 等于请求 tokens 总和（无丢失）。

---

## 6. 灰度发布与回滚

### 6.1 发布顺序

1. **先迁后启**：`sqlx::migrate!` 在启动时自动执行 0021；Docker 单实例直接替换镜像
   即完成（迁移在服务就绪前跑完）。多实例共享库：先滚动升级任意一台（其迁移对旧
   实例透明，旧代码读 TIMESTAMPTZ 的 daily/monthly 行在 UTC 会话下等值兼容），
   再收敛全量；建议低峰执行以缩短混跑窗口。
2. **灰度开关**：功能本身由「管理员是否创建 hourly Plan」门控——不发 hourly 计划
   即零行为差异；可先在测试分组/低优先级分组（priority 低于现网生效分组）挂
   hourly 计划观察 `plan_usage_counters` 桶分布与告警频率，再放开真实用户。
3. **观察指标**：`plan_usage_counters` 按 `period_type='hourly'` 的行数/桶跨度；
   429（insufficient_quota）率变化；`plan_alerts` 派发量（小时粒度下 80/95/100
   告警频次显著高于月度属预期，需提前知会运营）。

### 6.2 回滚

- **代码回滚（不回库）**：旧二进制可读写迁移后的库（daily/monthly 等值判定在 UTC
  会话下成立；period_hours/anchor 列被旧代码忽略）。**回滚前必须先停用/解绑全部
  hourly 计划**——旧二进制不识别 'hourly'：`load_plan_runtimes` 会把它当 `_ =>`
  分支按 total 语义处理（period_key='t'），可能误读当期用量。
- **数据回滚（如需彻底降级）**：
  ```sql
  DELETE FROM plan_usage_counters pc USING coding_plans p
    WHERE p.id = pc.plan_id AND p.period_type = 'hourly';  -- 小时行按日粒度有损
  ALTER TABLE plan_usage_counters
    ALTER COLUMN period_start TYPE DATE
    USING (period_start AT TIME ZONE 'UTC')::date;
  ALTER TABLE coding_plans
    DROP COLUMN period_hours, DROP COLUMN period_anchor_mode;
  ALTER TABLE coding_plans DROP CONSTRAINT coding_plans_period_type_check;
  ALTER TABLE coding_plans ADD CONSTRAINT coding_plans_period_type_check
    CHECK (period_type IN ('daily','monthly','total'));
  ```
  注意 DATE 化对 hourly 行有损（桶起点时刻丢失）；先 DELETE 可避免同用户同日多行
  违反 PK。0017+ 迁移链为前向式（sqlx 无 down），以上为手工反向脚本。

---

## 7. 后续扩展位（本次未做）

- `weekly` 周期：扩 CHECK + `period_start` 分支即可，模型与 hourly 同构。
- 本地时区锚点（`fixed_local` + 每计划 IANA tz）：让「每日 00:00 本地」类需求成立。
- 多周期叠加配额（同一用户同时受 hourly + monthly 约束）：PlanRuntime → 配额列表，
  check_plan 逐项判定，DB 计数器结构已天然支持（按 plan 多行）。
- `plan_alerts_seen` 进程内集合按桶淘汰（当前量级无需）。

## 8. 风险评估

| 风险 | 等级 | 缓解 |
|---|---|---|
| 管理员误配 N=1 的小时计划 → 告警/429 频发，用户困惑 | 中 | UI 明示「每 N 小时清零」；灰度 §6.1-2；告警邮件注明「本时段」 |
| 旧二进制与迁移后库混跑期间读 hourly 计划按 total 语义 | 中（仅回滚窗口） | 回滚 SOP：先停用 hourly 计划（§6.2） |
| `plan_usage_counters` 行数放大（24×/日 vs 月度 1 行） | 低 | 行宽极小、PK 索引局部性好；`idx_plan_counters_plan` 已存在；如需可按 `updated_at` 归档 |
| fixed 锚点 N 不整除 24 的跨日相位漂移与运营直觉不符 | 低 | 设计文档 + UI hint 明示「纯 UTC 网格」；join 锚点不受影响 |
| UPDATE 屏蔽 period 切换瞬间的在途请求按旧桶落账 | 低 | 与既有周期切换语义一致：旧桶封存、新桶从 0 起算，无资金/配额安全风险 |
| 键格式漂移（Rust `h:{epoch}` vs SQL `extract(epoch)::bigint`） | 低 | 两侧均整秒；单测 `hourly_key_is_bucket_epoch` + 集成键比对双保险 |
