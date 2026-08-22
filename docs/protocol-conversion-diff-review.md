# 协议转换层差异审查报告：llm_gateway vs cc-switch

> 审查日期：2026-08-20
> 审查对象：本仓库 `src/service/{responses,anthropic}` + `src/service/proxy.rs`（当前工作区状态）
> 参考实现：`~/AI/code/cc-switch` `src-tauri/src/proxy/`（v3.20.0 工作区状态）
> 方法：逐函数代码比对，所有结论附 文件:行号 证据；cc-switch 侧引用均经源码定点核验（非仅凭 `docs/cc-switch-protocol-conversion-analysis.md` 索引）。
> 范围声明：只列具体差异与缺失，不评价整体代码质量。

## 0. 方向澄清与已对齐部分

网关两条链路均为「客户端方言 ↔ Chat Completions 上游」的双向转换：

- **链路 A（Chat↔Responses）**：`/v1/responses` 客户端 → Chat 上游。请求向 `responses/convert_req.rs`，响应向 `responses/convert_resp.rs` + `responses/stream.rs`。镜像 cc-switch 管线①（`transform_codex_chat.rs` + `streaming_codex_chat.rs`）。
- **链路 B（Chat↔Anthropic）**：`/v1/messages` 客户端 → Chat 上游。请求向 `anthropic/transform.rs`，响应向 `anthropic/convert_resp.rs` + `anthropic/stream.rs`。镜像 cc-switch 管线②（`transform.rs` + `streaming.rs`）。

网关已正确移植、不计入差异的部分（附实现位置）：

| 能力 | 网关位置 |
|---|---|
| usage 三桶恒等式（input + cache_read + cache_creation == prompt_tokens） | `anthropic/convert_resp.rs:104-118` |
| canonical JSON（键序稳定，保上游前缀缓存） | `service/canonical.rs` |
| Claude Code billing header 剥离 | `anthropic/transform.rs:126-144` |
| DeepSeek/MiMo tool-call 消息 reasoning_content 占位符 | `anthropic/transform.rs:20,281-287` |
| 多 finish_reason chunk 去重（kimi-k2.6 类） | `anthropic/stream.rs:147-157,392-425` |
| #4341 无名工具调用护栏（防静默空成功） | `responses/convert_resp.rs:88-113`、两条流式路径 |
| `stream_options.include_usage` 流式注入 | `proxy.rs:284-286,300-302` |
| 断流 incomplete/failed 区分 + 记账兜底（BillingOnDrop） | `responses/stream.rs:198-265`、`anthropic/stream.rs:167-212,472-496` |
| 宽松 chunk 解析（字符串 usage/浮点 created/parts 数组 content） | `responses/dto.rs:213-360` |
| 200 + JSON 错误体不得伪装成空响应（422/502 整形） | `proxy.rs:399-486,566-634`、`responses/mod.rs:34-76` |
| 流式 idle timeout（120s） | `proxy.rs:22-92` |
| 有状态字段显式拒绝（conversation/previous_response_id 等） | `responses/convert_req.rs:113-137`（cc-switch 为静默丢弃，网关更严格） |

---

## 一、高优先级差异（P0）

### H1.〔链路 A·请求·工具〕custom 工具往返断裂：output item 丢失 + wire type 非法

- **网关缺失点**：`input_item_to_chat_messages`（`src/service/responses/convert_req.rs:198-250`）只识别 `function_call` / `custom_tool_call` / `function_call_output` 三种 item；`custom_tool_call_output` 落入默认分支后因无 `content` 字段被**整项静默跳过**（:228-234 注释列举 reasoning/web_search_call/computer_call，但 custom_tool_call_output 同样命中）——工具执行结果永远回不到上游。另外 `custom_tool_call_item_to_chat_tool_call`（:421-443）产出 `type:"custom_tool_call"` + `custom:{...}` 的 tool_call 条目，Chat 协议要求 `type:"function"`，严格上游直接 400。
- **cc-switch 位置**：custom 工具降级为 function 工具并把原始定义嵌进 description（`transform_codex_chat.rs:1252-1259`，调用 :685-687）；`custom_tool_call_output | tool_search_output` → canonical JSON 后作为 tool 消息内容（:724-762）。
- **影响**：Codex 客户端使用 custom/freeform 工具（如 apply_patch 形态）时，第二轮请求即缺失工具结果，上游 400 或对话逻辑损坏。
- **补齐**：为 `custom_tool_call_output` 增加分支（output 序列化为 tool content）；assistant 侧 custom 调用改写为合法 function 形态（type 改 `"function"`，input 包装为 `{"input": <str>}`，参照 cc-switch `CUSTOM_TOOL_INPUT_FIELD` :46）。

### H2.〔链路 B·请求·参数〕Anthropic `thinking` 配置完全丢弃

- **网关缺失点**：`anthropic_request_to_chat`（`src/service/anthropic/transform.rs:29-93`）处理了 model/stream/temperature/top_p/max_tokens/stop_sequences/system/messages/tools/tool_choice，**无任何 `thinking` 字段分支**。Claude Code 开启扩展思考时发送 `{"thinking":{"type":"enabled","budget_tokens":N}}`，被静默丢弃。
- **cc-switch 位置**：`transform.rs:208-213` + `resolve_reasoning_effort`（:94-124，`output_config.effort` 优先；`thinking.type=adaptive→xhigh`；`enabled+budget` 按 <4000 / 4000–15999 / ≥16000 分档 low/medium/high）+ `supports_reasoning_effort` 模型族门控（:71-81）。
- **影响**：推理力度意图经网关后丢失；对支持 `reasoning_effort` 的 Chat 上游（o 系列 / gpt-5+ / grok-4.5），思考预算退化为上游默认，成本与行为不可控。
- **补齐**：移植 `resolve_reasoning_effort` 分档映射（含模型族门控，避免向非推理上游发未知参数）。

### H3.〔链路 B·请求·多模态〕tool_result 内图片/文档被静默丢弃

- **网关缺失点**：`tool_result_content`（`src/service/anthropic/transform.rs:347-365`）只拼接 `text` 块；tool_result content 数组中的 `image`/`document` 块被忽略。
- **cc-switch 位置**：`plan_chat_tool_output_media` / `queue_chat_tool_output_media`（`transform.rs:426-433`）抽取媒体，`flush_pending_chat_tool_media`（:472，调用 :419-447）合成相邻 user 媒体消息（Chat 的 tool 消息不能带图片）；管线①同款（`transform_codex_chat.rs:702-717`）。
- **影响**：Claude Code 视觉工作流（screenshot 工具回传图片）多轮循环中截图丢失，模型看不到工具执行结果，输出质量静默退化。
- **补齐**：tool_result 块级扫描，非 text 块转 `image_url`/`file` part，按 cc-switch 语义插入紧随其后的 user 消息。

### H4.〔双链路·流式·工具〕无 index 工具帧：并行调用被错误合并

- **网关缺失点**：`append_tool_call_delta`（`src/service/responses/stream.rs:421`）与 `append_tool_delta`（`src/service/anthropic/stream.rs:301`）均 `tool_call.index.unwrap_or(0)`——所有缺 index 的帧一律归入 key 0。两个不同调用（各自带新 id、均无 index）会互相覆盖 id/name，**arguments 跨调用串接成非法 JSON**。
- **cc-switch 位置**：`resolve_tool_key_without_index`（`streaming_codex_chat.rs:345-368`）：仅当帧带"与所有已知调用不同的非空 id"才分配新 key，否则并入最后已知调用（注释明言"宁可坍缩也不炸开"），并防 `usize::MAX+1` 回绕。
- **影响**：省略 index 的第三方上游（OpenAI 协议中 index 必填但实践中存在）+ 并行工具调用 → 参数损坏、工具执行失败。
- **补齐**：两条流式状态机改用 cc-switch 的 key 解析策略（新 id → 新槽位；无 id → 末位合并）。

### H5.〔架构·配对矩阵〕交叉方言配对静默破坏，无显式拦截

- **网关缺失点**：转换决策只有二态——"原生透传 or 转 Chat"（`responses/mod.rs:25-27`、`anthropic/mod.rs:36-38`）。`/v1/messages` + `api_type=openai-responses` 上游：Anthropic 请求被转成 Chat 体发往 `{base}/chat/completions`（`proxy.rs:281-294`），Responses-only 上游无此端点 → 必败；`/v1/responses` + `api_type=anthropic` 上游同理（`proxy.rs:296-307`）。`/v1/chat/completions` + 非 Chat 上游同样只会直通到不存在的端点。无任何显式报错。
- **cc-switch 位置**：管线③ `transform_codex_anthropic.rs`（Responses↔Anthropic）、管线④ `transform_responses.rs`（Anthropic↔Responses）补全了全部客户端×上游组合。
- **影响**：路由/降级链（`fallback_ids`）里混入不同方言上游时，故障转移后静默 404/400，用户侧表现为"偶尔坏"。
- **补齐**：短期在 `should_convert` 决策处对不可行配对返回 400（指明不支持的上游方言组合）；长期按需移植管线③/④。

### H6.〔链路 B·工程化·认证/版本〕原生 Anthropic 上游缺 `anthropic-version` 头

- **网关缺失点**：`send_upstream`（`src/service/proxy.rs:695-720`）只构造 Authorization: Bearer / Content-Type / X-Request-Id（流式加 Accept）。不透传客户端 `anthropic-version`，也无注入逻辑，且不支持 `x-api-key` 认证形态（`store/upstream.rs:6-16` Provider 无 auth scheme 字段）。Anthropic 官方 API 强制要求 `anthropic-version` → **原生透传路径（api_type=anthropic）对官方 API 全部 400**。
- **cc-switch 位置**：客户端值透传（`forwarder.rs:2098-2102`）+ 缺失补 `2023-06-01`（:2167-2172）；按 provider 推断 `x-api-key` vs `Bearer`（`claude.rs:669-826`，`get_auth_headers` :871-885）。
- **影响**：配置了 Anthropic 官方上游的部署，`/v1/messages` 原生路径 100% 失败。
- **补齐**：Provider 增加 `auth_scheme`（bearer/x-api-key）与 `extra_headers`；anthropic 上游缺 version 时补默认值。

---

## 二、中优先级差异（P1）

### M1.〔链路 A·请求〕Responses `reasoning` input item 丢弃，跨轮思考不回传

- 网关：`convert_req.rs:229-234` 跳过（与 H1 同一分支）。cc-switch：pending_reasoning 前向附挂到其后 assistant 消息（`transform_codex_chat.rs:764-773`）、user 回合边界与 input 尾部回溯附挂（:1077-1098、:654-662，防思考跨回合泄漏）、kimi/DeepSeek 要求 tool-call 消息带非空 reasoning_content 时回填占位（:663、:1030-1063——注意此回填在管线①是**无厂商门控**的）。
- 影响：思考型上游（DeepSeek-reasoner 等）多轮工具循环中历史思考丢失；若上游要求 assistant tool-call 消息必须带 reasoning_content（网关仅 anthropic 链路有厂商 hints，`anthropic/transform.rs:20`，responses 链路无），将 400。
- 补齐：请求期建立 pending_reasoning 附挂逻辑；`REASONING_VENDOR_HINTS` 门控复用到 responses 链路。

### M2.〔链路 A·请求·工具〕tools 为空时未清理 `tool_choice` / `parallel_tool_calls`

- 网关：`request_tool_choice_to_chat`（`convert_req.rs:541-558`）不检查 tools 是否为空 → 可能产出带 tool_choice 无 tools 的 Chat 请求。cc-switch：`transform_codex_chat.rs:330-342`（vLLM/企业网关对此 400）。网关自己的 anthropic 链路已有等价门控（`anthropic/transform.rs:83-90`）。
- 补齐：convert_request 出口处按 tools 空否删两字段。

### M3.〔链路 A·请求·参数〕reasoning 参数形态不可配置；`effort:"none"` 原样透传

- 网关：`ChatRequestOut` 固定输出 `reasoning_effort`（`dto.rs:87-88`）+ `enable_thinking`/`thinking_budget` 原样透传（:97-100）；无 per-provider 形态选择；`convert_req.rs:60-65` 对 effort 仅过滤空串——`"none"` 会以 `reasoning_effort:"none"` 发给上游，多数 Chat 上游枚举不含 none → 400 风险。
- cc-switch：`apply_reasoning_options`（`transform_codex_chat.rs:353-449`）五种形态（`thinking` / `enable_thinking` / `reasoning_split` / `reasoning_effort` / `reasoning.effort`，provider 声明 `CodexChatReasoningConfig`）；显式关闭时对 OpenRouter 形态忠实转发 `{"reasoning":{"effort":"none"}}`（:414-416），`reasoning_requested` 识别 none/off/disabled（:451-460）。
- 影响：要求 `{"thinking":{"type":"enabled"}}` 形态的上游无法开启/关闭思考；effort=none 客户端请求可能被上游拒。
- 补齐：Provider 表增加 reasoning 形态配置；effort=none 语义分支。

### M4.〔双链路·流式〕内联 `<think>...</think>` 检测缺失

- 网关：正文 delta 直接进 output_text / text 块（`responses/stream.rs:168-176`、`anthropic/stream.rs:135-143`）。cc-switch：`InlineThinkState` 三态机（`streaming_codex_chat.rs:40-52`、边界冲刷 :229-268），把混在 content 里的思考分离为 reasoning 事件。
- 影响：不单独发 `reasoning_content` 而把思考混入正文的第三方上游 → 思考文本污染最终答案、计费口径（text vs reasoning token）失真。
- 补齐：在两条流式状态机的 content 通道前置 `<think>` 探测缓冲。

### M5.〔链路 B·非流式〕无名工具调用丢弃后无护栏，与自身流式路径不一致

- 网关：`chat_tool_call_to_tool_use`（`anthropic/convert_resp.rs:158-174`）丢弃无名 tool_call；`chat_response_to_anthropic` 无 #4341 护栏 → 客户端拿到 `content:[]` + `end_turn` 的空成功。流式路径有护栏（`anthropic/stream.rs:195-207`）、responses 非流式也有（`responses/convert_resp.rs:88-113`）——仅此一处缺口。
- cc-switch：非流式保留 tool_use（name 空亦保留，`transform.rs:617-635`）且 `has_tool_use` 强制 stop_reason=tool_use（:683）；流式合成 `unknown_tool` late-start（`streaming.rs:542-602`）。
- 影响：非流式请求遇到畸形 tool_calls 时 Claude Code 拿到空消息静默"成功"，agent loop 无声卡死。
- 补齐：对齐自身流式路径的护栏（丢弃全部且无其他输出 → 422/error）。

### M6.〔链路 B·流式〕工具参数无限空白防护缺失

- 网关：`ToolBlock`（`anthropic/stream.rs:39-48`）无空白计数。cc-switch：`INFINITE_WHITESPACE_THRESHOLD=500`（`streaming.rs:101-102`）+ 连续空白达到阈值即中止该工具调用（:432-446，Copilot 上游 bug）。
- 影响：连接该类缺陷上游时流式挂死直至 120s idle timeout，且已下发大量垃圾 delta。
- 补齐：ToolBlock 加 consecutive_whitespace 计数与中止逻辑。

### M7.〔工程化·重试/高可用〕401/403 不走降级链；流式完全无故障转移；无熔断器

- 网关：非流式仅 429/5xx/传输错误/超时换下一候选（`proxy.rs:548-553`），401/403/404 立即透传；流式"仅主上游，不重试"（`proxy.rs:350-352`）；无熔断器。
- cc-switch：`categorize_proxy_error`（`forwarder.rs:2721-2724`）仅 `400|405|406|413|414|415|422|501` 不可重试，401/403/408/429/5xx 均可换家；**流式首块失败可重试**（`forwarder.rs:4047-4048` 测试 `streaming_first_chunk_error_is_retryable_before_success_record`）；三态熔断（`circuit_breaker.rs`，单 provider 链路旁路 `forwarder.rs:436-438`）。
- 影响：主上游 key 失效/限流时，网关流式请求直接失败，尽管降级链里有健康候选。
- 补齐：流式在"尚未向客户端写出任何字节"窗口内允许重试下一候选（无重复生成风险）；非流式把 401/403 纳入可切换集合。

### M8.〔计费〕内部记账丢失缓存桶，缓存命中按全额 input 价扣费

- 网关：`billing_usage()` 只上报 input/output（`responses/stream.rs:283-290`、`anthropic/stream.rs:215-222`）；`store::usage::Usage` 无 cache 字段（`store/usage.rs:21-29`）；`ModelPrice` 仅 input/output 单价（`store/rules.rs:22-26`）；`compute_cost`（`service/usage.rs:115-120`）按全额计。客户端侧 usage 桶本身正确（`anthropic_usage` 三桶恒等式、`input_details_with_cache` 归一化）。
- cc-switch：`usage/parser.rs:12-22` cache_read 多源解析 + `calculator.rs:14-25` cache_read/cache_creation 独立每百万定价、按 app 类型区分 inclusive 语义（:62-68）。
- 影响：配额（monthly_token_quota/cost_quota）与成本统计对缓存重度用户显著偏高。
- 补齐：store::Usage 与 ModelPrice 增加 cache_read/cache_creation 字段，record 时透传。

### M9.〔错误映射〕错误体缺结构化上下文、无 message 截断、code 非稳定枚举

- 网关：`reshape_upstream_error_for_responses`（`responses/mod.rs:38-76`）/ `reshape_upstream_error`（`anthropic/mod.rs:113-149`）只拼 `{message,type,code}`，code 缺失时回退 HTTP 状态码数值（数值与字符串混合类型，客户端难分支）；message 无长度上限（上游 413 返回整页 nginx HTML 会原样透传）；无 provider/model 关联。
- cc-switch：`handlers.rs:1897-2030`——message 截断 1800 字符（:1964）、稳定 `cc_switch_*` code 枚举（:2008-2029）、`provider/model/endpoint/upstream_status` 结构化字段（:1996-2002）、413 特化为可操作指引（:1928-1931）、非 JSON 上游体截 1KB（:1812-1826）。
- 影响：排障时无法从客户端错误定位到渠道/模型；超大错误体浪费带宽；code 类型不稳定破坏程序化处理。
- 补齐：错误整形统一加 provider/model/request_id（网关已有 X-Request-Id 响应头关联，补进 body）、message 截断、字符串 code 枚举。

### M10.〔认证/透传〕客户端请求头零透传，Provider 无自定义头配置

- 网关：`send_upstream` 仅 4 个自建头（`proxy.rs:701-707`）；`headers` 参数只用于鉴权与取 IP（`proxy.rs:210`、`v1.rs:16-23`）。user-agent、OpenRouter 所需 `HTTP-Referer`/`X-Title`、`anthropic-beta` 等全部丢弃；Provider 无 headers 字段，`extra_body` 只能改 body。
- cc-switch：客户端头白名单透传（`forwarder.rs:2098-2173`，保序；`anthropic-version`/`anthropic-beta`/`user-agent`/`accept`/`x-app`，:4358-4365）。
- 影响：依赖 referer/title 计费归因或 beta 特性的上游行为异常。
- 补齐：Provider 增加静态 headers 配置；定义客户端头透传白名单。

---

## 三、低优先级差异（P2）

| # | 差异 | 网关位置 | cc-switch 位置 | 说明 |
|---|---|---|---|---|
| L1 | system 消息收拢到头部 | `convert_req.rs:235-246` input 中间 system 原地保留 | `transform_codex_chat.rs:554-580`（MiniMax 只许首条 system）；管线② `transform.rs:308-357` | 影响 MiniMax 类严格上游与前缀缓存稳定 |
| L2 | `response.in_progress` 事件 | 只发 created（`responses/stream.rs:154-157`） | `streaming_codex_chat.rs:270-282` created+in_progress 双事件 | 多数 SDK 容忍，个别严格客户端依赖 |
| L3 | 未知参数白名单透传 | 固定结构体丢弃（`dto.rs:53-101`）：`frequency_penalty/logit_bias/logprobs/n/presence_penalty/seed/stop` | `EXTRA_CHAT_PASSTHROUGH_FIELDS` 15 字段（`transform_codex_chat.rs:28-43`） | 标准 Responses 请求不含这些字段，仅影响非标客户端 |
| L4 | legacy `function_call` 响应字段 | 非流式不处理（`anthropic/convert_resp.rs:43`）；流式反而兼容（`dto.rs:314-338`） | `transform.rs:636-665` | 网关自身两路径不一致 |
| L5 | 工具 schema 清洗深度 | 根非 object 时**整个替换**为空 object（`anthropic/transform.rs:410-415`，破坏性）；responses 方向无 normalize（`convert_req.rs:519-522`） | `clean_schema` 递归清 `format:uri`、缺 type 补 object+properties（`transform.rs:518-549`）；`normalize_function_parameters`（`transform_codex_chat.rs:1272-1286`） | 网关替换丢 properties |
| L6 | 流式 function_call item 附 reasoning | 无 | `streaming_codex_chat.rs:469-477` | 思考归属工具调用的保真度 |
| L7 | namespace 工具名拍平（64 截断+hash） | 无 namespace 概念 | `transform_codex_chat.rs:1223-1240` | Codex namespace 工具场景 |
| L8 | 缓存字段解析优先级 | details 优先于直传字段（`dto.rs:406-420`） | 直传字段优先（`transform.rs:693-711`） | 仅双字段并存的非标上游有差异 |
| L9 | 首字节超时独立配置 | 仅固定 120s idle（`proxy.rs:25`） | first_byte/idle 分开可配（`response_processor.rs:700-733`） | 首字节也吃满 120s 才断 |
| L10 | `metadata.user_id` 键名错误 | 读 `/metadata/user`（`anthropic/transform.rs:58`），Anthropic 规范为 `metadata.user_id` → 永远取不到 | 管线②同样不处理 | 网关属实现 bug，修正为 user_id 即可 |
| L11 | 非流式请求收到 SSE 体 | 直接把 SSE 字节当 JSON body 返回（`proxy.rs:555-637` 无 content-type 嗅探） | `body_looks_like_sse` 嗅探兜底（`handlers.rs:397-411`） | 依赖违反 stream:false 的上游 |
| L12 | tool_choice 未知值策略 | 未知 mode 强制 `auto`（`anthropic/transform.rs:388-393`） | 原样透传（`transform.rs:285-306`） | 均不致命 |
| L13 | 测试/示例覆盖缺口 | 单测 + conformance 脚本（8 种怪癖模式：string_usage/crlf/content_parts/unnamed_tool/cache_usage 等，`scripts/mock_upstream.py:59-236`） | 转换文件内联海量测试（`transform.rs` 2000 行中约 1250 行测试），覆盖断流、缺 index、UTF-8、多 finish_reason | conformance 未覆盖：缺 index 工具帧、内联 `<think>`、多 finish_reason、工具媒体、custom 工具往返、交叉配对拒绝 |

---

## 四、双方均缺失（该配对方向）

1. **`tool_choice.disable_parallel_tool_use` → `parallel_tool_calls:false`**（链路 B 请求方向）：cc-switch 仅在管线③做反向映射（`transform_codex_anthropic.rs:427-435`），管线②与网关均丢弃该字段。
2. **Chat→X 流式方向的 SSE `ping` 心跳**：cc-switch 仅在管线④（Responses 上游→Anthropic 客户端）发 ping（`streaming_responses.rs:67`）；两条 Chat 上游链路双方均不发。长思考静默期只能靠客户端自身超时容忍。
3. **流式 `delta.refusal` 处理**：双方流式状态机均无 refusal 通道（cc-switch `streaming_codex_chat.rs` 无 refusal 处理；网关 `ChatStreamDelta`（`dto.rs:173-189`）无该字段）；仅非流式有 refusal→text 兜底。
4. **hosted 工具（web_search 等）在 Chat 上游方向的桥接**：双方在该配对均剥离/丢弃内置工具（网关 `normalize_tools` `proxy.rs:108-120`，显式声明"客户端可感知"降级；cc-switch 管线①同样无 web_search 处理），仅 cc-switch 管线④有桥接。
5. **思考签名（thinking signature）跨协议封装**：cc-switch 的 `reasoning_bridge.rs`（`ccswitch-anthropic-thinking-v1:` opaque 封装）服务于管线③④；在「Chat 客户端/上游」两侧配对中双方均无法保留签名（Chat 协议无载体），属协议本质限制。

---

## 五、高风险协议兼容性问题汇总

1. **custom_tool_call wire type 非法 + custom_tool_call_output 丢失**（H1，`convert_req.rs:421-443` / `:228-234`）——严格上游 400 或工具循环断裂。
2. **无 index 并行工具帧合并损坏**（H4，`responses/stream.rs:421`、`anthropic/stream.rs:301`）——参数串接成非法 JSON。
3. **Anthropic `thinking` 丢弃**（H2，`anthropic/transform.rs:29-93`）——扩展思考请求行为不可控。
4. **tool_result 媒体丢弃**（H3，`anthropic/transform.rs:347-365`）——视觉工具循环静默降质。
5. **交叉方言配对静默 404**（H5，`responses/mod.rs:25-27` + `anthropic/mod.rs:36-38` + `proxy.rs:281-307`）——降级链含异构上游时随机失败。
6. **原生 Anthropic 上游缺 `anthropic-version`**（H6，`proxy.rs:695-720`）——官方 API 100% 400。
7. **空 tools + tool_choice 发往严格上游**（M2，`convert_req.rs:541-558`）——vLLM 类 400。
8. **`reasoning_effort:"none"` 原样下发**（M3，`convert_req.rs:60-65`）——枚举不含 none 的上游 400。
9. **Anthropic 非流式空成功**（M5，`anthropic/convert_resp.rs:158-174`）——agent loop 无声卡死，且与自身流式/responses 路径行为不一致。

---

## 六、修复优先级建议

H6（一行头注入即可修复完全不可用路径）→ H4/H1（数据损坏类）→ H2/H3（功能静默丢失）→ M5/M2/M3-none（400/空成功类）→ H5（先加显式拒绝，再决定是否移植管线③/④）。
