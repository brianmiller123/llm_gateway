-- Plan 成员直连管理：coding_plans ↔ users / coding_plans ↔ user_groups 多对多关联。
-- - plan_users：直连用户成员（不经过分组，适合个别授权）
-- - plan_groups：分组加入 Plan；一个分组可加入多个 Plan，运行时按 priority 择优
--   （此前 user_groups.plan_id 是「只属于一个 Plan」的特例，本次迁移为多对多）
-- - 主键即唯一约束：同一用户/分组不可重复加入同一 Plan，重复 INSERT 幂等跳过

CREATE TABLE plan_users (
    plan_id  BIGINT NOT NULL REFERENCES coding_plans(id) ON DELETE CASCADE,
    user_id  BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (plan_id, user_id)
);
CREATE INDEX idx_plan_users_user ON plan_users(user_id);

CREATE TABLE plan_groups (
    plan_id  BIGINT NOT NULL REFERENCES coding_plans(id) ON DELETE CASCADE,
    group_id BIGINT NOT NULL REFERENCES user_groups(id) ON DELETE CASCADE,
    added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (plan_id, group_id)
);
CREATE INDEX idx_plan_groups_group ON plan_groups(group_id);

-- 存量绑定迁移：user_groups.plan_id（单绑）→ plan_groups（多对多）
INSERT INTO plan_groups (plan_id, group_id, added_at)
SELECT plan_id, id, now()
FROM user_groups
WHERE plan_id IS NOT NULL;

ALTER TABLE user_groups DROP COLUMN plan_id;
