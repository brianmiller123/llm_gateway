-- 模型库启停：管理员可对具体 (供应商, 模型) 停用服务。
-- 禁用后该模型在该供应商不再作为路由候选（全部候选被禁 → 503），
-- 且不在 /v1/models 目录中列出；路由规则/价格等配置不受影响。
-- 默认 TRUE，存量行为不变；供应商刷新模型库时保留此状态。
ALTER TABLE models
    ADD COLUMN enabled BOOLEAN NOT NULL DEFAULT TRUE;
