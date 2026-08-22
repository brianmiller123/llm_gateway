-- API 端点管理：Response API / Anthropic Messages API 的启用开关与用户页地址
-- 展示开关（system_settings 单行扩展）+ 管理员 API 测试历史。
ALTER TABLE system_settings
    ADD COLUMN responses_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN responses_visible BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN messages_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN messages_visible BOOLEAN NOT NULL DEFAULT TRUE;

-- 管理员 API 测试历史（页面「最近测试结果」数据源；body 仅存预览截断文本）
CREATE TABLE IF NOT EXISTS api_test_results (
    id           BIGSERIAL   PRIMARY KEY,
    api          TEXT        NOT NULL CHECK (api IN ('responses', 'messages')),
    admin_id     BIGINT      REFERENCES users(id) ON DELETE SET NULL,
    model        TEXT        NOT NULL DEFAULT '',
    stream       BOOLEAN     NOT NULL DEFAULT FALSE,
    status_code  INT         NOT NULL,
    ok           BOOLEAN     NOT NULL,
    latency_ms   BIGINT      NOT NULL,
    error        TEXT        NOT NULL DEFAULT '',
    body_preview TEXT        NOT NULL DEFAULT '',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_api_test_results_created ON api_test_results (created_at DESC);
