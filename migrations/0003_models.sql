-- 模型库：test-connection / 手动刷新时从上游拉取并持久化，
-- 供路由规则、模型价格页下拉选择。删除供应商时级联清理。
CREATE TABLE IF NOT EXISTS models (
    id BIGSERIAL PRIMARY KEY,
    provider_id BIGINT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (provider_id, model_id)
);

CREATE INDEX IF NOT EXISTS idx_models_provider ON models(provider_id);
