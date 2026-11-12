-- LDAP 管理员双来源：目录真值 + 控制台手动覆盖。
-- 有效权限恒为 is_admin = COALESCE(admin_override, admin_ldap)：
--   admin_ldap     目录授予真值（LDAP JIT 登录按 memberOf 组写入；本地账号恒 false）
--   admin_override 手动覆盖；非空时优先于目录真值，NULL = 跟随目录。
--                  本地账号没有目录来源，其管理员身份只能存于覆盖列。
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS admin_ldap BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS admin_override BOOLEAN;

-- 回填：目录真值取「目录用户且当前管理员」；本地账号的现有权限视为手动覆盖；
-- is_admin 保持现值（迁移后等价于 COALESCE(admin_override, admin_ldap)）。
UPDATE users SET
    admin_ldap = (source = 'ldap' AND is_admin),
    admin_override = CASE WHEN source = 'local' THEN is_admin ELSE NULL END;
