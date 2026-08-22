-- 高级请求配置：extra_body 透传
-- providers（渠道级）与 model_routes（模型级，覆盖渠道级）各一份 JSON 对象，
-- 每次请求深合并进转发上游的请求体（配置值覆盖客户端同名字段）。
-- '{}' = 未配置（不合并）；全局开关 system_settings.extra_body_enabled。
ALTER TABLE providers ADD COLUMN extra_body JSONB NOT NULL DEFAULT '{}';
ALTER TABLE model_routes ADD COLUMN extra_body JSONB NOT NULL DEFAULT '{}';

ALTER TABLE system_settings ADD COLUMN extra_body_enabled BOOLEAN NOT NULL DEFAULT TRUE;
