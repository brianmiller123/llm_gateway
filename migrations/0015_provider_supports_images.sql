-- H6：渠道级图像能力声明（cc-switch media_sanitizer image_input_capability_from_settings 同款）。
-- supports_images = FALSE → 发前主动把图片 part 替换为 [Unsupported Image] 文本标记
--（纯文本上游不再每轮对话首请求必失败一次）；默认 TRUE 保持既有行为
ALTER TABLE providers ADD COLUMN IF NOT EXISTS supports_images BOOLEAN NOT NULL DEFAULT TRUE;
