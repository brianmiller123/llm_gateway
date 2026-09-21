-- 限流规则并发上限：在途请求数（进入代理管线至响应体流尽/断开）上限。
-- 0 = 不限并发（默认，既有行为不变）；>0 时该规则作用域内同时在途请求数
-- 达到上限即 429。规则匹配与令牌桶同维度（scope × 模型精确匹配），多规则
-- 叠加时全部命中规则的并发余量都须满足（全有或全无，不部分占坑）。
ALTER TABLE rate_limit_rules ADD COLUMN IF NOT EXISTS concurrency INTEGER NOT NULL DEFAULT 0;
