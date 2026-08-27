-- 全局自定义 Header（设置页）：
--   upstream_headers  网关 → 上游提供商的每个代理请求追加的请求头
--   response_headers  网关 → 客户端 /v1/* 所有响应附加的响应头
-- '{}' = 未配置。JSONB 对象 {name: value}；保存前经 HeaderName/HeaderValue
-- 合法性与网关管理头黑名单校验（见 admin_config::validate_header_map）。
-- 优先级：渠道级 provider.extra_headers > 全局 upstream_headers > 客户端透传白名单。
ALTER TABLE system_settings
  ADD COLUMN IF NOT EXISTS upstream_headers JSONB NOT NULL DEFAULT '{}',
  ADD COLUMN IF NOT EXISTS response_headers JSONB NOT NULL DEFAULT '{}';
