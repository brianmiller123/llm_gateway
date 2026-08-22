-- 协议兼容性修复（docs/protocol-conversion-diff-review.md H6/M8/M10）：
-- - providers.auth_scheme：上游认证形态（bearer 默认 / x-api-key，Anthropic 原生路径用）
-- - providers.extra_headers：渠道级静态附加请求头（OpenRouter HTTP-Referer/X-Title 等）
-- - model_prices 缓存桶单价：缓存命中按独立费率计费，缺省回退 input 单价
-- - usage_logs 缓存 token 明细：内部记账/对账（配额扣费同口径）

ALTER TABLE providers ADD COLUMN auth_scheme VARCHAR(16) NOT NULL DEFAULT 'bearer';
ALTER TABLE providers ADD COLUMN extra_headers JSONB NOT NULL DEFAULT '{}'::jsonb;

ALTER TABLE model_prices ADD COLUMN cache_read_price_per_m NUMERIC(10,6);
ALTER TABLE model_prices ADD COLUMN cache_write_price_per_m NUMERIC(10,6);

ALTER TABLE usage_logs ADD COLUMN cache_read_tokens BIGINT;
ALTER TABLE usage_logs ADD COLUMN cache_write_tokens BIGINT;
