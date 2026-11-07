-- 生成阶段耗时（ms），用于统计输出 token 生成速度（token/s）：
-- 流式 = 请求开始 → 记账（流终态/客户端断开），扣除首 token 等待；
-- 非流式/无首 token 记录 = 总耗时（含预填充，速度按该口径为下界）。
-- 历史行为 NULL（不参与速度聚合），避免用 TTFB 冒充生成长度算出虚高速度。
ALTER TABLE usage_logs ADD COLUMN IF NOT EXISTS gen_ms BIGINT;
