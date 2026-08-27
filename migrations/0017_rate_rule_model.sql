-- 限流规则模型维度：model = NULL 表示作用于所有模型（既有行为不变）；
-- 非 NULL 时仅当请求的客户端模型名与之精确相等才命中，桶键独立计量
-- （如 user:1|model:gpt-4o），与不限模型的规则叠加生效（最严者先拒）。
ALTER TABLE rate_limit_rules ADD COLUMN IF NOT EXISTS model VARCHAR(128);
