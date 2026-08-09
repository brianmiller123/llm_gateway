-- API Key 明文可再次查看：AES-256-GCM 加密后落库（密钥 = GATEWAY_MASTER_KEY）
-- 存量 Key 此列为 NULL，无法恢复明文，需重新创建
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS key_encrypted TEXT;
