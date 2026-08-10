-- 会话表补索引：refresh token 查找按哈希精确匹配，无索引时全表扫描
CREATE INDEX IF NOT EXISTS idx_refresh_tokens_hash
    ON refresh_tokens(token_hash)
    WHERE revoked_at IS NULL;
