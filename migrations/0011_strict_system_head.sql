-- 模型级开关：strict_system_head
-- 由管理员按模型启用：将 Chat 请求中全部 system 消息收拢到消息数组头部
--（MiniMax 类严格上游只接受首条消息为 system；多条/居中 system 直接 400）。
-- FALSE = 保持原消息序（默认，历史行为不变）。
ALTER TABLE model_routes ADD COLUMN strict_system_head BOOLEAN NOT NULL DEFAULT FALSE;
