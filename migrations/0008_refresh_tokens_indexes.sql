-- refresh_tokens 索引补全 + 收敛（0001 建表时缺 user_id 索引，0006 只补了 token_hash）
-- 强制下线/登出按 user_id 过滤（revoke_all_for_user），无索引时全表扫描且占用连接池
CREATE INDEX IF NOT EXISTS idx_refresh_tokens_user ON refresh_tokens(user_id) WHERE revoked_at IS NULL;
-- 过期清理（聚合器周期任务）按 expires_at 过滤
CREATE INDEX IF NOT EXISTS idx_refresh_tokens_expires ON refresh_tokens(expires_at);
