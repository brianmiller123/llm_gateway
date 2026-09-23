-- 限流简化为仅并发上限：移除令牌桶（rpm/burst）与用户月度配额子系统。
-- 并发上限（concurrency）成为限流规则的唯一限制维度；user_quotas 表及其
-- 月度 Token/成本配额整体废弃（月度用量展示仍由 user_monthly_usage 支撑，
-- 该表保留，仅不再用于配额拦截）。
ALTER TABLE rate_limit_rules
    DROP COLUMN IF EXISTS rpm,
    DROP COLUMN IF EXISTS burst;

DROP TABLE IF EXISTS user_quotas;
