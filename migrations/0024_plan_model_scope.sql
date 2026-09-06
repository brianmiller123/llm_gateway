-- Coding Plan 模型作用域：控制 Plan 仅对匹配的客户端模型生效。
-- NULL = 对所有模型生效（默认，存量行为不变，完全向后兼容）。
-- JSON 形状（allow/deny 均可选；pattern 支持精确或尾缀 * 前缀匹配，语义与路由规则一致，
-- 大小写不敏感；deny 黑名单优先于 allow 白名单）：
--   {"allow": ["claude-*", "gpt-4o"], "deny": ["*-preview"]}
-- 白名单缺省 = 除黑名单外全部放行；黑白名单同时为空/缺省等价 NULL（加载时归一化为 NULL）。
ALTER TABLE coding_plans
    ADD COLUMN model_scope JSONB;
