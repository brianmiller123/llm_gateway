-- LDAP 管理员双来源：目录真值 + 控制台手动覆盖。
-- 有效权限恒为 is_admin = COALESCE(admin_override, admin_ldap)：
--   admin_ldap     目录授予真值（LDAP JIT 登录按 memberOf 组写入；非 LDAP 账号恒 false）
--   admin_override 手动覆盖；非空时优先于目录真值，NULL = 跟随目录。
--                  非 LDAP 账号没有目录来源，其管理员身份只能存于覆盖列。
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS admin_ldap BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS admin_override BOOLEAN;

-- 回填（只允许执行一次）：列变更幂等，但本 UPDATE 不幂等——重放会按当前
-- is_admin 重写来源列，并清掉 0033 之后写入的所有手动覆盖。
-- 规则：LDAP 行的现有权限视为目录真值（admin_override = NULL）；其他来源的
-- 现有权限全部视为手动覆盖（source <> 'ldap'，使不变式对任意 source 成立）。
-- is_admin 保持现值（回填后等价于 COALESCE(admin_override, admin_ldap)）。
-- 迁移前对 LDAP 用户的手动授予/撤销无法与目录真值区分，一律回填为 directory：
-- 该用户下一次 LDAP 登录会按目录组重算权限；需要保留手动结果时请升级后重新授予。
UPDATE users SET
    admin_ldap = (source = 'ldap' AND is_admin),
    admin_override = CASE WHEN source <> 'ldap' THEN is_admin ELSE NULL END;
