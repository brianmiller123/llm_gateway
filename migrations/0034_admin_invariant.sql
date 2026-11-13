-- 双来源管理员不变式：把 0033 的物化列等式从约定升级为数据库强制。
-- 有效权限恒为 is_admin = COALESCE(admin_override, admin_ldap)：
--   admin_ldap     目录真值（LDAP JIT 登录按组写入；非 LDAP 恒 FALSE）
--   admin_override 手动覆盖；非空时优先，NULL = 跟随目录
-- 五条写路径（本地创建 / LDAP JIT / 手动覆盖 / reset / 组同步建档）都在单条
-- SQL 内维持该等式；本约束拦截后续新增写路径的静默漂移。
-- 0033 的回填对任意 source 成立，且约束只校验现有行一次、之后只约束写入，
-- 因此无需再回填。现网存量已核验违反行数为 0。
ALTER TABLE users
    ADD CONSTRAINT users_admin_is_coalesce
    CHECK (is_admin = COALESCE(admin_override, admin_ldap));
