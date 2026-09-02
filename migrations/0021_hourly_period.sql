-- Coding Plan 小时级重置：
-- - period_type 增加 'hourly'（每 period_hours 小时翻转一个统计窗口）
-- - period_hours：窗口长度（小时，1..=168）；仅 hourly 消费，其余类型恒为默认 1
-- - period_anchor_mode：fixed=UTC 整点网格（锚点=epoch）；join=按用户开通时间
--   （user_group_members.added_at）偏移切桶
-- - plan_usage_counters.period_start DATE → TIMESTAMPTZ：小时桶起点必须携带时刻。
--   语义统一为「当期统计窗口起点时刻」：
--   daily=当日 00:00 UTC / monthly=当月 1 日 00:00 UTC / total=epoch / hourly=桶起点（整秒）。
--   显式按 UTC 换算（裸 ::timestamptz 走会话时区，非 UTC 部署会整体偏移一天界）。

ALTER TABLE coding_plans DROP CONSTRAINT coding_plans_period_type_check;
ALTER TABLE coding_plans ADD CONSTRAINT coding_plans_period_type_check
    CHECK (period_type IN ('daily', 'monthly', 'total', 'hourly'));

ALTER TABLE coding_plans
    ADD COLUMN IF NOT EXISTS period_hours INT NOT NULL DEFAULT 1
        CHECK (period_hours >= 1 AND period_hours <= 168),
    ADD COLUMN IF NOT EXISTS period_anchor_mode VARCHAR(16) NOT NULL DEFAULT 'fixed'
        CHECK (period_anchor_mode IN ('fixed', 'join'));

ALTER TABLE plan_usage_counters
    ALTER COLUMN period_start TYPE TIMESTAMPTZ
    USING (period_start::timestamp AT TIME ZONE 'UTC');

COMMENT ON COLUMN plan_usage_counters.period_start IS
    '当前统计窗口起点时刻(UTC): daily=当日00:00 / monthly=当月1日00:00 / total=epoch / hourly=小时桶起点(整秒)';
