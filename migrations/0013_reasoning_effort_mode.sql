-- H3：路由级 reasoning_effort 值域钳制模式（cc-switch effortValueMode 子集）
-- 可选值：passthrough（默认，NULL 同义）/ deepseek / low_high / openrouter
ALTER TABLE model_routes ADD COLUMN IF NOT EXISTS reasoning_effort_mode TEXT;
