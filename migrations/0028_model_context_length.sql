-- 模型上下文长度（token 数）：管理员按 (供应商, 模型) 维护，随 /v1/models
-- 下发给客户端展示。NULL = 未设置（不下发该字段，客户端回落自身默认值——
-- 对 384K 等非标上下文模型，客户端默认 1M 会高估可用上下文）。
ALTER TABLE models
    ADD COLUMN context_length BIGINT;
