-- 模型映射：路由可配置上游模型名（客户端请求模型 → 上游实际模型），NULL = 原样透传
ALTER TABLE model_routes ADD COLUMN upstream_model VARCHAR(128);

-- 来源 IP 统计：记录每次调用的客户端 IP（X-Forwarded-For 优先，缺省取对端地址）
ALTER TABLE usage_logs ADD COLUMN client_ip INET;
CREATE INDEX idx_usage_ip_time ON usage_logs(client_ip, created_at);
