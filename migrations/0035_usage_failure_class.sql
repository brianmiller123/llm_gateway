-- 用量失败归因（连接加固）：候选失败的 usage_logs 行记录失败类别 / 上游状态 /
-- 候选序号，供按渠道/原因统计上游故障（连接错误、header 等待超时、读体超时等）。
-- 可空无默认：成功行与既有行保持 NULL，不改变成功记账行为。
--   failure_class   connect_error / header_timeout / body_timeout /
--                   body_read_error / upstream_status / all_candidates_failed
--   upstream_status 上游 HTTP 状态（仅 failure_class = upstream_status 时有值）
--   attempt         候选序号（0 = 主渠道，1.. = 降级链）
ALTER TABLE usage_logs ADD COLUMN IF NOT EXISTS failure_class TEXT;
ALTER TABLE usage_logs ADD COLUMN IF NOT EXISTS upstream_status INTEGER;
ALTER TABLE usage_logs ADD COLUMN IF NOT EXISTS attempt SMALLINT;
