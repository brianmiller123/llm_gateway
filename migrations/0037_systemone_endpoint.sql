-- /v1/systemone（TypeSafe System One 决策模型）端点开关：启用/停用与用户页
-- 地址展示（system_settings 单行扩展；形状同 0012 的 responses/messages）。
-- 该端点仅 api_type=systemone 上游原生透传，无流式、无降级转换；管理端 API
-- 测试历史（api_test_results.api CHECK）不扩展，本迁移只开关与地址。
ALTER TABLE system_settings
    ADD COLUMN systemone_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN systemone_visible BOOLEAN NOT NULL DEFAULT TRUE;
