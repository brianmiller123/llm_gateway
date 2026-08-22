-- H3：路由级 thinking 形态配置（cc-switch CodexChatReasoningConfig thinking_param 子集）
-- thinking_form 可选值：NULL（剥离 thinking 形态字段，仅 reasoning_effort 渠道）/
-- thinking_param（{"thinking":{"type":"enabled"}}）/ reasoning_split / enable_thinking
-- responses_passthrough_fields：逗号分隔的 Responses 方言字段透传白名单
--（store / safety_identifier / prompt_cache_retention / prompt_cache_key），
-- 默认 NULL = 全部剥离（严格上游对未知字段 400）
ALTER TABLE model_routes ADD COLUMN IF NOT EXISTS thinking_form TEXT;
ALTER TABLE model_routes ADD COLUMN IF NOT EXISTS responses_passthrough_fields TEXT;
