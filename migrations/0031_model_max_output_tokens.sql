-- 模型最大输出长度（token）：管理员按 (供应商, 模型) 维护，随 /v1/models 以
-- max_output_tokens / max_completion_tokens 双别名下发。NULL = 未设置（不下发，
-- 客户端回落自身默认）。同 0028/0030：刷新模型库保留，网关不据此截断请求。
ALTER TABLE models
    ADD COLUMN max_output_tokens BIGINT;
