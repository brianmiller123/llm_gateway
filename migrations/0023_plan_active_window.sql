-- Coding Plan 生效时段：仅当天墙钟 [active_start, active_end) 内该 Plan 参与择优生效；
-- 双 NULL = 全天生效（默认，存量行为不变）。start > end 视为跨零点隔夜窗（如 22:00-06:00）。
-- 判定按服务器本地时区（营业时间语义）；配额统计周期仍一律 UTC，两者互不影响。
ALTER TABLE coding_plans
    ADD COLUMN active_start TIME,
    ADD COLUMN active_end   TIME;
