-- 模型能力标注：管理员按 (供应商, 模型) 维护，随 /v1/models 下发。
-- text = 仅文本 / vision = 视觉（图像输入）/ multimodal = 多模态。
-- NULL = 未标注（不下发该字段）。同 0028 context_length：刷新模型库保留
--（replace_provider_models 只删上游已下架的行，不动已存在行）。
ALTER TABLE models
    ADD COLUMN capability TEXT;

ALTER TABLE models
    ADD CONSTRAINT models_capability_check
    CHECK (capability IS NULL OR capability IN ('text', 'vision', 'multimodal'));
