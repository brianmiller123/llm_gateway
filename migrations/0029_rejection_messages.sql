-- 可配置拒绝文案（单行表新增两列；'' = 使用内置默认文案）
-- 1) 并发上限 429（{retry} = Retry-After 秒数占位符）
-- 2) 模型无权限 403（{model} = 客户端请求的模型名占位符）
ALTER TABLE system_settings
    ADD COLUMN msg_concurrency VARCHAR(255) NOT NULL DEFAULT '',
    ADD COLUMN msg_model_denied TEXT NOT NULL DEFAULT '';
