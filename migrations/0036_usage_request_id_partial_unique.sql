-- 连接加固/F3 修复：usage_logs.request_id 由全表 UNIQUE（0001 列约束
-- usage_logs_request_id_key）改为部分唯一——唯一性只约束客户端可见行。
-- 失败归因模型对同一请求写多行：每个失败候选一条归因行（failure_class
-- 非空，经 record_failure_row），再加至多一条终局行（failure_class NULL）。
-- 全表约束使先落库的归因行挤掉后落的终局行（duplicate key → 客户端实际
-- 收到的错误行被静默丢弃，failure_class IS NULL 聚合失真）。
-- 终态（客户端可见）行每请求仍唯一；候选失败归因行（failure_class 非空）
-- 允许同请求多行。
ALTER TABLE usage_logs DROP CONSTRAINT usage_logs_request_id_key;
CREATE UNIQUE INDEX usage_logs_request_id_client_visible_key
    ON usage_logs(request_id) WHERE failure_class IS NULL;
