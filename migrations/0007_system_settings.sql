-- 系统设置：单行表（LDAP 等运行时可配置项）
-- 优先级：DB 设置 > 环境变量（env 作为未配置时的默认值/兜底）
CREATE TABLE IF NOT EXISTS system_settings (
    id                     SMALLINT     PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    ldap_url               VARCHAR(255) NOT NULL DEFAULT '',
    ldap_starttls          BOOLEAN      NOT NULL DEFAULT FALSE,
    ldap_bind_dn           VARCHAR(255) NOT NULL DEFAULT '',
    ldap_bind_password_enc TEXT         NOT NULL DEFAULT '',  -- AES-256-GCM(master_key)，'' = 未设置
    ldap_base_dn           VARCHAR(255) NOT NULL DEFAULT '',
    ldap_user_filter       VARCHAR(255) NOT NULL DEFAULT '',
    ldap_admin_groups      TEXT         NOT NULL DEFAULT '',  -- 逗号分隔 DN 列表
    updated_at             TIMESTAMPTZ  NOT NULL DEFAULT now()
);

INSERT INTO system_settings (id) VALUES (1)
ON CONFLICT (id) DO NOTHING;
