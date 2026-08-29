-- 模型级开关：system_head_merge（与 strict_system_head 配套）
-- strict_system_head 开启后，多条 system 消息的处理方式：
-- - TRUE  = 全部 system 文本按序拼接为头部单条（MiniMax 类只接受首条为
--           system 的严格上游；历史行为，默认值保持不变）
-- - FALSE = 仅把全部 system 消息按原序移到消息数组头部，保持多条独立
--           （qwen3 类上游报 "system message must be at the beginning"，
--           只要求 system 在开头，多条/居中非法，无需合并）
-- strict_system_head = FALSE 时本列无效果。
ALTER TABLE model_routes ADD COLUMN system_head_merge BOOLEAN NOT NULL DEFAULT TRUE;
