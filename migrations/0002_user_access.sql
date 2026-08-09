-- 用户 × 供应商/模型 访问授权（白名单语义）
-- 匹配规则：rule.provider_id IS NULL = 任意供应商；rule.model_pattern IS NULL = 该供应商全部模型
-- 语义：用户无任何规则 = 默认放行（兼容既有账号）；有规则 = 必须命中其一；admin 用户不受限
CREATE TABLE user_access_rules (
    id            BIGSERIAL PRIMARY KEY,
    user_id       BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider_id   BIGINT      REFERENCES providers(id) ON DELETE CASCADE,
    model_pattern VARCHAR(128),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, provider_id, model_pattern)
);
CREATE INDEX idx_access_user ON user_access_rules(user_id);
