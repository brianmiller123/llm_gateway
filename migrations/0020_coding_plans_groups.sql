-- Coding Plan 用量限制与用户分组管理：
-- - coding_plans：配额包（token 上限精确整数、统计周期、超额策略、告警渠道）
-- - user_groups / user_group_members：分组与成员（manual/ldap/all）
-- - plan_usage_counters：(user, plan, period_start) 周期计数器，记账事务内原子 UPSERT
-- - plan_alerts：80/95/100% 阈值告警 + 站内通知（user_id 非空）+ 系统兜底通知（level=0）
-- - system_settings：SMTP（告警邮件；密码密文存储）

CREATE TABLE coding_plans (
    id              BIGSERIAL PRIMARY KEY,
    name            VARCHAR(128) NOT NULL UNIQUE,
    description     TEXT NOT NULL DEFAULT '',
    -- 用户同属多个分组时取 priority 最高者生效；同分取 plan_id 大者
    priority        INT NOT NULL DEFAULT 0,
    -- 精确 token 数（前端 1.5G 等单位串由 API 层解析；500M=500000000）
    token_limit     BIGINT NOT NULL CHECK (token_limit > 0),
    -- daily=自然日重置 / monthly=自然月重置 / total=总量不重置（一律 UTC）
    period_type     VARCHAR(16) NOT NULL DEFAULT 'monthly'
                    CHECK (period_type IN ('daily', 'monthly', 'total')),
    -- block=拦截(429) / downgrade=降级到 downgrade_model / log=仅记录并告警
    overage_action  VARCHAR(16) NOT NULL DEFAULT 'block'
                    CHECK (overage_action IN ('block', 'downgrade', 'log')),
    downgrade_model VARCHAR(128),
    -- JSON 数组：in_site / email / webhook 的子集
    alert_channels  JSONB NOT NULL DEFAULT '["in_site"]',
    webhook_url     TEXT NOT NULL DEFAULT '',
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE user_groups (
    id               BIGSERIAL PRIMARY KEY,
    name             VARCHAR(128) NOT NULL UNIQUE,
    description      TEXT NOT NULL DEFAULT '',
    -- Plan 删除时解绑（SET NULL）：成员回退次优先级分组的 Plan 或不限额
    plan_id          BIGINT REFERENCES coding_plans(id) ON DELETE SET NULL,
    -- TRUE = 与 LDAP 目录联动：周期同步 + 手动触发，目录增删用户自动进/出分组
    ldap_sync        BOOLEAN NOT NULL DEFAULT FALSE,
    last_sync_at     TIMESTAMPTZ,
    last_sync_result TEXT NOT NULL DEFAULT '',
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE user_group_members (
    group_id BIGINT NOT NULL REFERENCES user_groups(id) ON DELETE CASCADE,
    user_id  BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- manual=手动添加；ldap=LDAP 同步管辖（同步只增删该来源，手动调整不受影响）；
    -- all=一键添加全部用户的快照
    source   VARCHAR(16) NOT NULL DEFAULT 'manual',
    added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX idx_group_members_user ON user_group_members(user_id);

-- Plan 计量计数器。plan_id 不带 FK：Plan 删除后历史用量保留，支持按周期回溯。
-- 与 usage_logs 明细写入同事务 UPSERT（见 store/usage.rs::record_usage），原子一致；
-- 单行热更新（高并发下同用户同周期串行化在行锁上，无读放大）。
CREATE TABLE plan_usage_counters (
    user_id      BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    plan_id      BIGINT NOT NULL,
    -- daily=当日(UTC) / monthly=当月 1 日 / total='1970-01-01'（常量）
    period_start DATE NOT NULL,
    tokens       BIGINT NOT NULL DEFAULT 0,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, plan_id, period_start)
);
CREATE INDEX idx_plan_counters_plan ON plan_usage_counters(plan_id, period_start);

-- 阈值告警 / 站内通知 / 系统事件（Plan 删除、停用等兜底提示）。
-- plan_id 不带 FK：删除后记录保留。UNIQUE 防多实例重复派发同一告警。
CREATE TABLE plan_alerts (
    id           BIGSERIAL PRIMARY KEY,
    plan_id      BIGINT,
    plan_name    TEXT NOT NULL DEFAULT '',
    user_id      BIGINT,
    username     TEXT NOT NULL DEFAULT '',
    -- 80 / 95 / 100；0 = 系统事件（无配额语义）
    level        SMALLINT NOT NULL,
    period_key   TEXT NOT NULL DEFAULT '',
    used         BIGINT NOT NULL DEFAULT 0,
    limit_tokens BIGINT NOT NULL DEFAULT 0,
    message      TEXT NOT NULL DEFAULT '',
    -- 各渠道派发结果 {"email":"ok","webhook":"error:..."}
    delivered    JSONB NOT NULL DEFAULT '{}',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX uq_plan_alerts_dedup
    ON plan_alerts(plan_id, user_id, period_key, level);
CREATE INDEX idx_plan_alerts_user ON plan_alerts(user_id, created_at DESC);
CREATE INDEX idx_plan_alerts_created ON plan_alerts(created_at DESC);

-- SMTP（告警邮件）；密码 AES-256-GCM 密文（crypto::encrypt，master_key 派生）
ALTER TABLE system_settings
    ADD COLUMN IF NOT EXISTS smtp_host VARCHAR(255) NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS smtp_port INT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS smtp_username VARCHAR(255) NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS smtp_password_enc TEXT NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS smtp_from VARCHAR(255) NOT NULL DEFAULT '';
