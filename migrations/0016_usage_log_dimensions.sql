-- L3：用量明细扩展（cc-switch usage/logger.rs first_token_ms / session_id 同款）：
-- - session_id：客户端会话关联（x-claude-code-session-id / session_id / x-grok-conv-id）
-- - first_token_ms：流式请求首个上游 chunk 到达耗时（非流式为 NULL）
ALTER TABLE usage_logs ADD COLUMN IF NOT EXISTS session_id TEXT;
ALTER TABLE usage_logs ADD COLUMN IF NOT EXISTS first_token_ms BIGINT;
