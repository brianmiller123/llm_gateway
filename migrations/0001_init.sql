-- LLM 网关初始 Schema（11 张表 + 聚合水位）

CREATE TABLE users (
    id            BIGSERIAL PRIMARY KEY,
    username      VARCHAR(64)  NOT NULL UNIQUE,
    email         VARCHAR(255),
    display_name  VARCHAR(128),
    ldap_dn       VARCHAR(512),
    source        VARCHAR(16)  NOT NULL DEFAULT 'ldap',
    password_hash VARCHAR(255),
    is_admin      BOOLEAN      NOT NULL DEFAULT FALSE,
    status        SMALLINT     NOT NULL DEFAULT 1,
    token_version INT          NOT NULL DEFAULT 0,
    last_login_at TIMESTAMPTZ,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE api_keys (
    id           BIGSERIAL PRIMARY KEY,
    user_id      BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         VARCHAR(64) NOT NULL,
    key_prefix   VARCHAR(16) NOT NULL,
    key_hash     CHAR(64)    NOT NULL,
    status       SMALLINT    NOT NULL DEFAULT 1,
    expires_at   TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_keys_prefix ON api_keys(key_prefix);
CREATE INDEX idx_keys_user   ON api_keys(user_id);

CREATE TABLE providers (
    id                BIGSERIAL PRIMARY KEY,
    name              VARCHAR(64) NOT NULL UNIQUE,
    api_type          VARCHAR(16) NOT NULL DEFAULT 'openai',
    base_url          TEXT        NOT NULL,
    api_key_encrypted TEXT        NOT NULL,
    timeout_ms        INT         NOT NULL DEFAULT 120000,
    enabled           BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE model_routes (
    id            BIGSERIAL PRIMARY KEY,
    model_pattern VARCHAR(128) NOT NULL,
    provider_id   BIGINT       NOT NULL REFERENCES providers(id),
    priority      INT          NOT NULL DEFAULT 100,
    fallback_ids  BIGINT[]     NOT NULL DEFAULT '{}',
    enabled       BOOLEAN      NOT NULL DEFAULT TRUE
);
CREATE INDEX idx_routes_pattern ON model_routes(model_pattern);

CREATE TABLE usage_logs (
    id            BIGSERIAL PRIMARY KEY,
    request_id    UUID         NOT NULL UNIQUE,
    user_id       BIGINT,
    api_key_id    BIGINT,
    model         VARCHAR(128) NOT NULL,
    provider_id   BIGINT,
    endpoint      VARCHAR(64),
    streamed      BOOLEAN      NOT NULL DEFAULT FALSE,
    input_tokens  BIGINT,
    output_tokens BIGINT,
    latency_ms    INT,
    status        SMALLINT,
    cost          NUMERIC(12,6),
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);
CREATE INDEX idx_usage_user_time  ON usage_logs(user_id, created_at);
CREATE INDEX idx_usage_model_time ON usage_logs(model, created_at);
CREATE INDEX idx_usage_time       ON usage_logs(created_at);

CREATE TABLE user_monthly_usage (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    month   CHAR(7) NOT NULL,
    tokens  BIGINT NOT NULL DEFAULT 0,
    cost    NUMERIC(14,6) NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, month)
);

CREATE TABLE usage_daily (
    user_id       BIGINT        NOT NULL,
    model         VARCHAR(128)  NOT NULL,
    stat_date     DATE          NOT NULL,
    call_count    BIGINT        NOT NULL DEFAULT 0,
    input_tokens  BIGINT        NOT NULL DEFAULT 0,
    output_tokens BIGINT        NOT NULL DEFAULT 0,
    cost          NUMERIC(14,6) NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, model, stat_date)
);

CREATE TABLE model_prices (
    id                  BIGSERIAL PRIMARY KEY,
    model               VARCHAR(128) NOT NULL,
    input_price_per_m   NUMERIC(10,6),
    output_price_per_m  NUMERIC(10,6),
    currency            VARCHAR(8) DEFAULT 'CNY',
    effective_from      DATE NOT NULL DEFAULT CURRENT_DATE
);

CREATE TABLE rate_limit_rules (
    id         BIGSERIAL PRIMARY KEY,
    scope      VARCHAR(16) NOT NULL,
    scope_id   BIGINT,
    rpm        INT NOT NULL DEFAULT 60,
    burst      INT NOT NULL DEFAULT 10,
    enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE user_quotas (
    user_id             BIGINT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    monthly_token_quota BIGINT,
    monthly_cost_quota  NUMERIC(14,6),
    billing_day         INT NOT NULL DEFAULT 1,
    notify_percent      INT NOT NULL DEFAULT 80,
    enabled             BOOLEAN NOT NULL DEFAULT TRUE,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE refresh_tokens (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash CHAR(64) NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE audit_logs (
    id          BIGSERIAL PRIMARY KEY,
    actor_id    BIGINT,
    action      VARCHAR(64) NOT NULL,
    target_type VARCHAR(32),
    target_id   BIGINT,
    detail      JSONB,
    ip          INET,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 聚合水位（幂等聚合）
CREATE TABLE aggregation_state (
    id           INT PRIMARY KEY CHECK (id = 1),
    watermark_id BIGINT NOT NULL DEFAULT 0
);
INSERT INTO aggregation_state (id) VALUES (1);
