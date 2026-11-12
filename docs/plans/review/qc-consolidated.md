# QC Consolidated — 长任务流式 + LDAP 管理员双来源

> 计划：`docs/plans/2026-11-12-long-stream-and-admin-fixes.md`｜分支：`fix/long-task-admin`｜base `a65384d`｜QC 范围 head `0ee7b03`（含文档 `6a38814`）
> 执行模式：SDD；QC 模式：tri（`qc1` / `qc2` / `qc3` + targeted re-review + final closure + micro）

## 席位与轮次

| 轮次 | qc1（架构/可维护性） | qc2（安全/正确性） | qc3（性能/可靠性） |
|---|---|---|---|
| R1 全量 tri | Needs-fix（3 W / 4 S） | Approve with residuals（5 S） | Needs-fix（1 W / 3 S） |
| R2 targeted（fix wave 2） | Needs-fix（1 W：F1） | Needs-fix（1 W：F1 + F2） | Approve（误判 F1 closed） |
| R3 final closure（fix wave 3） | Approve（3 S） | Approve（3 S） | Approve（3 S，自认上轮漏判） |
| micro（S1/S2 单源化） | — | — | Approve（findings: []） |

## 阻断项闭环

| Finding | 来源 | 处置 | 证据 |
|---|---|---|---|
| 提交后失败与提交成功双记账 → 2xx 后卡死永不被熔断 | qc1/qc2 R2；qc3 R3 复核 | 透明 SSE 提交处不再记 success；`SseErrorTail` 结算：EOF→success、Err→failure、未结算 drop→`release_probe`；判定由 `tail_settles` 单源 | 3 条回归测试按生产序列；旧实现下 `committed_sse_stream_failures_open_breaker` 实测失败；`cargo test` 320/320 |
| reset 对本地行产生 `directory` 状态 | qc1 R1 W1 | API 400 + store `AND source='ldap'` | e2e 实测 400；DB 测试 |
| `reload_admin_ids` 无守卫快照回写（撤销复活 ≤30s） | qc2 R2 F2 | `admin_ids_gen` 版本守卫（锁内比较） | 两向单测 `stale_admin_snapshot_is_rejected…` / `fresh_admin_snapshot_is_committed` |
| 错误帧文本可能泄漏上游 URL/Key | qc2 R1 S1 | 帧与日志同源 `redact_secrets` | 单测断言 URL 内密钥变 `***` |
| 方言错误帧不一致 / 伪造 response 对象 | qc1 R1 W1/S3 | Anthropic → `stream_error`；Responses → `event: error`（不伪造）；Chat 不变 | 3 方言单测 |
| 配置边界静默失效 | qc1 R1 W3 | 启动告警 + README 优先级；`FIRST_BYTE < PRIME` 配方 | config 单测/代码审阅 |
| 中止连接硬断（socket closed） | qc3 R1 F-001 | 透传 SSE Err → 方言错误帧后正常 EOF | e2e：idle=3 + 静默 6s → 200 + 错误帧，curl exit 0 |

## 已登记 residual（计划 §7）

- 仅中性帧的无限流（网关无“产出性空闲”计时；客户端 watchdog/断开兜底，错误进熔断）。
- 转换链（Anthropic/Responses）提交后传输失败不计熔断（既有）。
- 错误构造器未统一（Anthropic 已一致；Responses 终止事件名不同）。
- 半开探测名额占用整条流、不绑定请求；pre-commit 断开不释放（不绕过熔断）。
- 上游自报错误 message 进 pre-commit 失败体不做二次脱敏（需上游回显我方凭据）。
- `reload_admin_ids` 读序无 seam 测试；用户名字大小写规范化；404 流式改 502；多候选 header 叠加；目录管理员钉住交互。

## 证据基线（PM 复核）

- `cargo build`：通过，仅 2 条基线既有 `dead_code` 警告（`ResponseGuards`、`active_in`；`a65384d` 已存在）。
- `cargo test --bin llm_gateway`：**320 passed / 0 failed**（`DATABASE_URL` 指向本地 PG；`#[sqlx::test]` 临时库）。
- `web npm run build`：零错。
- PM acceptance（独立于实现者）：
  - 首字节 65s（旧 60s 窗口）：200、响应头 2.0s、4 次 ping、65.2s 完整结束，无 502；
  - 首 chunk 后静默 130s（旧 120s 窗口）：200、响应头 0.002s、8 次 ping、130.1s 完整结束；
  - 默认配置真卡死：`200 + 方言错误帧`（idle=3 场景 3.0s 正常 EOF）；
  - LDAP：手动授予跨 3 次登录保持 `manual/true`；reset 回 `directory/false`；目录撤组重登降权、加回恢复；本地行 reset 400；不变式 0 违例 + CHECK 已验证；
  - 共享库迁移：33 重放 + 34 应用成功（0033 checksum 偏差已在本机库修正）。
