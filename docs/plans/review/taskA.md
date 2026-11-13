# Task review — T1

## Scope

- Diff range: `git diff a745524..4a0e352`（4 文件，+307/−9）：`src/service/proxy.rs`、`src/config.rs`、`scripts/mock_upstream.py`、`README.md`。集成工作树 HEAD 已含 T1 合并（`4932e0b`）。
- 打开的文件：`src/service/proxy.rs`（:24-128、:1225-1285、:1279-1473、:1473-1723、:1713-1803、:2735-2765、:2930-2998、:2999-3113）、`src/service/anthropic/stream.rs:700-760`、上述 4 文件的 diff 全量、`history://LongStreamFix`（实现者报告）。
- 交叉核对（只读 grep）：`GATEWAY_STREAM_*` 全仓引用（仅 README/config/proxy/plan，无 `.env.example` 等第二处漂移）、`saw_timeout`/`is_timeout_error`/`redact_secrets`/`wrap_stream(` 调用点、`STREAM_PING_INTERVAL`、`with_stream_timeouts`。
- 未运行 build/test（只读席位；PM 已提供 41s 集成构建、`cargo test --bin llm_gateway` 306/306、`npm run build` 通过）。

## Verdict: Approve

## Findings

- [Suggestion] `src/service/proxy.rs:1333-1336` — `tokio::select!` 的 `biased;` 把 item 分支放在 deadline 之前，墙钟上限在“同步就绪的中性帧洪水”下会被饥饿 / 证据：`biased` 语义是先 poll 到 ready 即返回，item 每次 polling 都 Ready 时 deadline 分支永不 poll（`Sleep` 也不会注册 waker）；真实网络节奏下两次 chunk 之间必有 Pending，deadline 仍会到期（mock `neutral_frames` 每 ≤5s 一帧，e2e 已在 2.0s 提交），只有上游一次性把大批中性帧压进 hyper 读缓冲时才会连续就绪 / 影响：不是用户可见违约——`buffered_bytes >= STREAM_PRIME_MAX_BYTES`（256KiB，:1327）会在排空时强制提交，界仍在，但“2s 墙钟”退化为“2s 或 256KiB”，与 README 的断言不完全等价 / 建议：把 deadline 分支置于 `biased` 之前（或去掉 `biased`）。安全：deadline 完成的两条出口都是「提交后 `committed=true` 退出」或「`prime_deadline = None` 丢弃」，不存在对已完成 `Sleep` 的二次 poll（见下方已核验项）。

- [Suggestion] `src/service/proxy.rs:162-175`（`is_timeout_node`/`is_timeout_error`）配合 `src/service/proxy.rs:2888` — header 失败分类路径上 `reqwest::Error::is_timeout` 分支不可达 / 证据：`send_upstream` 在 :2888 用 `redact_secrets(format!("upstream request failed: {e}"), …).into()` 把 reqwest 错误压平成字符串 `Box<dyn Error>`，`source()` 链为空、`downcast_ref::<reqwest::Error>()` 永不命中，只剩三条文案可匹配 → :1267 的分类行为与改动前的 `contains("header timeout")` 完全相同（新增文案 “stream first byte/idle timeout” 也不会出现在该路径）。priming 路径（:1410）则受益：`StreamTimeoutStream` 用 `Box::from(e)`（:110-113）保留了 reqwest 类型，downcast 有效 / 影响：无误判风险（这也是本席对“reqwest 嵌套是否会把非超时误判”的结论：在该路径上不可能命中），但函数注释“priming 与 header 失败分类共用”隐含的“header 侧也覆盖 reqwest 超时”并未成立 / 建议：或修正注释，或在 `send_upstream` 保留类型化 source（如自定义 error 包装 reqwest::Error）后再 downcast。

- [Suggestion] `src/service/proxy.rs:2956`（`sse_error_frame`）在 `src/service/proxy.rs:1671` 的 `is_sse_body` 上启用，但该透传出口同时服务**原生 Anthropic / 原生 Responses** 客户端（`anthropic_converted==false && converted==false` 的分支）/ 证据：:1666-1690 的 passthrough 分支对 `endpoint.anthropic` + 原生 anthropic 上游同样走 `wrap_stream`，注入帧固定为 OpenAI 形态 `data: {"error":{"message","type","code"}}`，无 `event: error`、无 Anthropic 的 `{"type":"error","error":{…}}` 外层；而 Chat→Anthropic 转换器早已发 `event: error`（`src/service/anthropic/stream.rs:757+`，H1）/ 影响：Anthropic SDK 按 `event:` 名分派，收到无 `event:` 且无 `"type"` 的 data 帧不会归为 typed error；不是回归（改动前是 hyper 硬断，客户端看到传输层错误），但方言不精确 / 建议：按 `endpoint`（anthropic / responses / chat）选择错误帧形状，与转换链路对齐。

- [Suggestion] `src/service/proxy.rs:3828`（`prime_deadline_commits_only_for_sse`）属 duplicated-producer / source-shape 类断言 / 证据：被测 `prime_deadline_commits`（:143）是一行为测试而抽出的 `expired && is_sse` 谓词，测试只是复述其定义；真正有风险的新逻辑——单次 `Sleep` 跨多轮复用、完成后不二次 poll、`buffered.into_iter().chain(stream)` 的前置顺序、到期投提交——无单测覆盖（brief 明确允许“谓词即可”，故不计为缺陷，仅记风险类别）/ 影响：这些性质的证据目前只来自代码阅读 + 实现者 e2e（2.01s 响应头、`Hello` + 错误帧的体顺序）/ 建议：delete，或替换为 product-behaviour 断言（用最小 harness 驱动 priming 循环：无产出 + 短 deadline → 提交；非 SSE → 不提交）；若维持现状，请在实现者报告里把“e2e 覆盖该性质”写明（现报告已隐含，可显式化）。

## Evidence check

已逐条复核实现者报告，结论：可复核、自洽，与 diff 一致。

- **priming deadline 机制（核心风险项）已核验为正确**：`Sleep` 每次候选新建（:1324-1325，位于 `'candidates` 循环内）；两种完成出口分别是 `committed=true; continue;`（while 条件即退出）与 `prime_deadline=None; continue;`（丢弃已完成的 `Sleep`）→ **不存在 fuse/重复 poll panic**；线程内无忙循环（每轮必 await 一次流；非 SSE 到期后直接回到无 deadline 的旧 `next().await`）；`is_sse=false` 时 deadline 撤销后仍保留 `with_stream_timeouts` 的 first-byte 窗口（300s）。
- **提交语义**：`break`（256KiB 上限）与 deadline 提交共用同一提交段（:1508-1513）：`record_success` + `buffered.into_iter().map(Ok).chain(stream)` —— 顺序保持、每块仅一次，无丢字节/重复；`is_sse` 为真时即使缓冲为空也提交（:1339-1350）。
- **`SseErrorTail`**：`Err` → 置 `finished=true` → 单条帧；后续 poll 直接 `None`，**不再 poll 上游**（:2984-2995）；非 SSE `enabled=false` 原样返回 `Err` 后同样 `None`。与 `StreamTimeoutStream` 的 `timed_out` 终止语义配合，正常路径只会产生一条错误帧。
- **usage 记账 / 告警“一次”成立**：`SseErrorTail` 位于 `wrap_stream` 的记账层**上游**（:3023-3029），错误帧作为普通 chunk 经过 `then`（`feed_sse`）与 `once` 兜底，`ctx.recorded` 的 `swap(true)` 与 `BillingOnDrop` 共享 → 仍只记一次；适配器自带 `tracing::warn!`。实现者 e2e 的 `usage recorded … status=200` + `WARN … emitting SSE error frame` 各一次与该结构一致。`sse_error_frame` 用 `json!` 构造，消息内换行被 JSON 转义，不会产生 SSE 帧注入。
- **超时分类**：`is_timeout_error` 沿 `source()` 逐节点检查并有负例（`connection refused`）单测；priming 超时 502→504 只改状态码，错误体仍是 `upstream_error_envelope`（:1411-1416），最终聚合出口按方言 reshape（:1720-1745），`saw_timeout→504 / connect→502 / 耗尽→503` 未变；零产出 EOF 仍 502（:1478-1491）。真卡死场景的 504 体文案 `upstream stream failed before productive output: upstream stream first byte timeout` 与 :1405-1406 的拼接逐字一致。
- **配置 / README**：默认 300/300/2000 在 `src/config.rs:140-151` 与 README 环境变量表逐项一致；`0` 语义（first-byte/idle = 禁用、prime = 不限）与 `> 0` 守卫（:126-128、:135-138）一致；`STREAM_PING_INTERVAL = 15s`（:29）与 README 的“15s `: ping`”一致；全仓无第二处 `GATEWAY_STREAM_*` 文档漂移。
- **mock**：沿用 `/tmp/mock_mode.txt` 的 mode 约定，新增仅追加分支（`slow_first_byte`/`slow_mid`/`neutral_frames`，均要求 `body["stream"]`），既有模式与 `json_body_for_stream` 分支未被改动；延迟参数走 `/tmp/mock_delay_*_secs` 且带缺省（90/150/60）；`PORT` 默认 9001 保持兼容。`slow_first_byte` 先发 SSE 头再睡，正确构造“上游 2xx 后无产出”。
- **e2e 数字自洽性**（仅算术复核，未重跑）：90s 场景 ping 17.01→77.01 共 5 条 = 提交(2.01s)+15s 步进、内容 90.25s（mock 睡 90s）、首块 17.01s 即首条 ping；150s 场景 ping 15.0→150.02 共 10 条、头 0.0s（首块 `Hello` 即 productive）；60s 场景头 2.0s = 默认 2000ms；504 场景 3.04s = `GATEWAY_STREAM_FIRST_BYTE_TIMEOUT=3`。该组数据互相印证，无自相矛盾。
- **测试计数**：实现者记录 302 passed（其分支，未并 T2），PM 记录 306/306（集成树含 T2 的 4 个新测例）——差异可由合并解释，非矛盾。
- **不可独立复核项**（不阻断，PM 侧已知）：`cargo build` 仅 2 条既有 warning（`ResponseGuards`、`ConcurrencyLimiter::active_in`）经 `git stash` 前后比对得出；本席未运行构建，接受为实现者证据。3 个新单测的可失败性：`is_timeout_error_*` 与 `sse_error_tail_*` 断言真实行为（能失败）；`prime_deadline_commits_only_for_sse` 见 Findings 第 4 条。

补充（本席核验、报告未点明的正向项）：deadline 位于**共享** priming 循环，因此对三个流出口（透传、Chat→Anthropic、Chat→Responses）同时生效，转换链路也会在 2s 内拿到响应头；`wrap_stream` 新形参仅 1 个调用点（:1672），无遗漏；非流循环与转换器内部逻辑未被本 diff 触碰。

## Residuals

1. **已接受的设计代价**（brief 允许、代码注释 + README 显式记录）：deadline 提交后到达的 200 + 错误帧随流透传，不再触发候选切换（`src/service/proxy.rs:1319-1323`；README `GATEWAY_STREAM_PRIME_TIMEOUT_MS` 行）。
2. **多候选 pre-response header 超时叠加未处理**（计划 §7 已登记，默认部署 1–2 候选）——既有行为，本 diff 未改。
3. **既有 2 条 `dead_code` warning**（`ResponseGuards` 字段、`ConcurrencyLimiter::active_in`）超出本任务文件范围，未处置。
4. **共享下游错误收尾覆盖面**：本次仅透传出口获得优雅收尾（brief 明确限定 `wrap_stream`）；转换出口已由 H1 自行发 `event: error`，无缺口，但两条路径的错误帧形状/文案来源不同（见 Findings 第 3 条），属后续统一项而非本任务缺陷。
