# LLM Gateway

企业内网 LLM API 网关：对外暴露 **OpenAI 兼容接口**（Chat Completions / Responses / Embeddings）与 **Anthropic Messages 接口**（Claude Code / Claude SDK 直连），统一鉴权、限流、配额、路由与用量计费，并提供带管理后台的 Web 控制台和公开服务状态页。

单二进制部署（Rust + axum），唯一外部依赖 PostgreSQL。定位为 <200 用户、低并发的内部部署，架构预留多实例扩展路径。

## 功能特性

**网关（协议转换代理）**

- `POST /v1/chat/completions`、`/v1/completions`、`/v1/embeddings`、`GET /v1/models`
- `POST /v1/responses`（OpenAI Responses API）：上游原生支持时透传（`api_type=openai-responses`），否则自动做 Responses ↔ Chat 转换（非流式 + 流式状态机），新旧 OpenAI SDK 均可直连
- `POST /v1/messages`（Anthropic Messages API，参照 cc-switch 转换层实现）：上游 `api_type=anthropic` 时透传，其余上游自动做 Anthropic ↔ Chat 降级转换——system（含 Claude Code billing header 剥离）/ tool_use ↔ tool_calls（参数 canonical JSON 保前缀缓存）/ tool_result → tool 消息 / thinking ↔ reasoning_content（DeepSeek 等厂商 tool-call 消息空思考自动补占位符）/ usage 三桶恒等式（input + cache_read + cache_creation == prompt_tokens 双向换算）/ finish_reason ↔ stop_reason / 流式 SSE 状态机（message_start → content_block_* → message_delta/message_stop，多 finish_reason 去重、工具块延迟启动、断流兜底）/ 错误整形为 Anthropic 单错误对象形状
- Responses ↔ Chat 转换层按 cc-switch 模式加固：usage 多源缓存字段归一化（prompt_tokens_details.cached_tokens / cache_read_input_tokens / DeepSeek prompt_cache_hit_tokens）、无名工具调用护栏（全部丢弃时报 failed 而非伪 completed）、流内错误帧 → response.failed、断流区分 incomplete/failed
- API Key 鉴权：Key 仅存 SHA-256 哈希 + 12 位前缀定位，明文只在创建时展示一次；可配置为不鉴权（仅本地开发）
- 三层速率限制（API Key → 用户 → 全局，令牌桶，BOOTTIME 回填时钟），命中返回 429 + `Retry-After` + OpenAI 标准错误体；长时间空闲后恢复的首请求空闲豁免，避免 agent 暂停等待用户确认后恢复即 429（见「限流与长时间空闲恢复」）
- 并发上限：限流规则可附在途请求数上限（0 = 不限），与令牌桶同维度（scope × 模型精确匹配）叠加；guard 绑定响应体生命周期，流式请求流尽/断开才释放；并发满返回 429（消息区分 "Concurrency limit exceeded"），不白烧令牌桶令牌，也不参与空闲豁免（见「限流与长时间空闲恢复」）
- 月度配额：按用户限制 Token/成本上限，记账事务内原子扣减，超限 429
- Coding Plan 周期配额：分组绑定套餐（多分组取优先级最高生效），token 上限 + 统计周期（每日 / 每月 / 总量 / 小时级——每 N 小时（1..168）滚动重置，锚点可选 UTC 整点网格或按开通时间偏移），超额策略（拦截 429 / 降级指定模型 / 仅告警）；可选模型作用域（精确 + 前缀通配、白/黑名单黑名单优先，未配置 = 全模型生效，切换模型即时重评估，见「Coding Plan 周期用量限制与小时级重置」）；80/95/100% 阈值告警（站内 / 邮件 / webhook）。重置由周期键在读取路径推导，无清零定时任务
- 模型路由：通配 pattern + priority 匹配，支持 `fallback_ids` 降级链（非流式对 429/5xx/超时自动重试；流式不重试避免重复生成）
- Token 计量：优先解析上游 `usage`（流式自动注入 `include_usage`），usage 双形态（`prompt_tokens` / `input_tokens`）归一化
- 高级请求配置（extra_body 透传）：渠道级（供应商）与模型级（路由规则）各一份 JSON 对象，请求转发前深合并进上游请求体（模型级覆盖渠道级同名叶键，配置覆盖客户端同名叶键、客户端独有字段保留）——解决 vLLM/SGLang 上 Qwen3 思考参数（`chat_template_kwargs.thinking` / `reasoning_effort`）无法透传的问题，兼容 `top_k`、`repetition_penalty` 等后端特有参数；全局开关可临时停用而不丢配置；`model`/`stream`/`stream_options` 由网关管理、配置被拒绝
- 上游 4xx/5xx 透明透传不吞错误（/v1/messages 按客户端方言整形为 Anthropic 错误体）；全链路 `request_id`（uuid v4）+ `X-Request-Id` 透传

**控制台（Vue 3 + Element Plus）**

- LDAP 登录（AD / OpenLDAP 配置驱动，memberOf 组映射管理员）+ 本地账号；内置 break-glass 本地管理员
- JWT 会话（access 15 分钟 / refresh 30 天，可配置），支持强制下线（token 版本吊销）
- 仪表盘、API Key 自助管理、用量统计（时间段 × 模型维度 + 图表；趋势支持 近 1 小时每分钟 / 今天每 30 分钟 / 近 N 天 三档粒度）、实时监控（5 分钟粒度轮询 + 全站在途请求数 + 每用户实时并发及其模型分布——进程内按 用户×模型 计数，模型为客户端请求的原始 model；guard 绑定响应体生命周期，流式请求流尽/断开才释放）、我的套餐（当前生效 Coding Plan 的余量与用量）
- 管理后台：用户生命周期（重置密码 / 禁用 / 强制下线；管理员可授权其他用户为管理员 / 取消授权）、供应商配置（Key AES-256-GCM 加密落库，永不回显；`api_type` 区分 openai / openai-responses / anthropic）、路由规则、模型库（按 供应商×模型 启停——禁用后不作路由候选、不在 `/v1/models` 列出，刷新模型库保留启停状态）、限流规则、月度配额、模型价格、Coding Plan 套餐管理、用户分组（批量添加 / LDAP 目录联动同步 / CSV 导出）、Plan 用量监控（趋势图 / 按用户按日期回溯 / 告警流）、LDAP 设置（含连通性测试）、SMTP 告警邮件设置、高级请求配置（extra_body JSON 编辑 + 校验 + 快捷预设 + 最终请求体实时预览）；break-glass 内置管理员（`SEED_ADMIN_USERNAME`）受保护——其他管理员的禁用 / 强制下线 / 重置密码 / 授权白名单 / 定向限流·并发规则 / 用户配额改动一律 403，LDAP 登录与目录同步亦不覆写该账号，防止唯一管理入口被锁死
- 管理操作全部写入审计日志，可追溯

**公开状态页（仿 status.openai.com）**

- 无需登录的 `/status` 页面 + `GET /api/status`
- 组件实时状态（网关 / PostgreSQL / 各上游供应商）+ 近 30 天按日可用率色块 + 事故历史（自动检测 + 相邻小时合并 + 严重度分级）
- 上游供应商双证据判定：每 30 秒主动探测 `{base_url}/1/status`（HTTP 状态码 + 响应体 status 字段 / 服务标识）；非结论性（404/401/403/429 等，主流 LLM 上游普遍未部署状态端点）时回退 `GET {base_url}/models`（OpenAI 兼容事实标准探针，免费不计费），融合 `usage_logs` 调用统计；两跳均非结论性才回退纯调用统计
- 页面每 60 秒自动刷新，刷新失败保留旧数据并显示降级提示条

## 架构总览

```mermaid
flowchart LR
    subgraph Client["客户端"]
        A["企业应用 / OpenAI SDK<br/>(Bearer API Key)"]
        B["浏览器（控制台 / 状态页）"]
    end
    subgraph Box["单机 Docker Compose"]
        subgraph GW["gateway 容器 — Rust 单二进制 (axum)"]
            T["TLS 终结 (rustls)<br/>8443 HTTPS + 8080"]
            P["/v1/* 代理管线<br/>鉴权 → 限流 → 配额 → 路由 → 转发(SSE) → 记账"]
            R["协议转换层<br/>Responses ↔ Chat / Anthropic ↔ Chat"]
            C["控制台 API + 静态资源 (Vue dist)"]
            W["后台任务：usage_daily 聚合 (60s)"]
        }
        PG[("postgres — 唯一外部依赖")]
    end
    A -->|HTTPS| T --> P --> R
    B -->|HTTPS| T --> C
    P --> PG
    C --> PG
    W --> PG
    P -. 上游转发 .-> U["DeepSeek / 通义 / GLM / vLLM / OpenAI"]
```
关键设计决策：

1. **模块化单体**：代理热路径、控制台、后台任务同进程；无 Nginx（TLS 由 rustls 终结，静态资源由 tower-http 托管），无 Redis。
2. **记账与热路径零组件解耦**：请求完成后同步单事务直写（日志 + 配额计数原子提交），延迟代价 ~1–2ms，相对 LLM 秒级响应不可见。
3. **内存态仅存在于单实例**：限流令牌桶、配额计数缓存、路由表在进程内，启动 / 30s 周期从 PG 热加载。多实例需换 Redis 实现（接口已封装）。

## 技术栈

| 层 | 选型 |
|---|---|
| 网关核心 | Rust (edition 2024) + axum 0.8 + tokio |
| TLS | tokio-rustls + hyper-util（ALPN: h2 + http/1.1） |
| HTTP 转发 | reqwest（rustls, stream，SSE 逐块透传） |
| 数据库 | sqlx + PostgreSQL 16（编译期 SQL 校验，迁移内嵌于二进制） |
| 加密 | AES-256-GCM（供应商 Key）+ SHA-256（API Key） |
| 认证 | LDAP (ldap3) + JWT (jsonwebtoken) + argon2（本地密码） |
| 前端 | Vue 3 + TypeScript + Element Plus + ECharts + Pinia |
| 部署 | Docker Compose（gateway + postgres，可选 OpenLDAP profile） |

## 快速开始（Docker Compose）

前置：Docker + Docker Compose。

```bash
# 1. 配置环境变量
cp .env.example .env
#    必改：GATEWAY_MASTER_KEY（openssl rand -hex 32 生成）、SEED_ADMIN_PASSWORD
#    （如启用 LDAP 登录再配置 LDAP_*）

# 2. 生成自签证书（公网部署请改用公司 CA / ACME 证书，替换 certs/ 下文件）
bash scripts/gen-cert.sh

# 3. 全自动构建（多阶段 Dockerfile：前端 + Rust + 运行镜像一步到位）
docker compose up -d --build

#    或本机快速构建（利用本地 cargo 缓存，秒级打镜像）：
#    npm run build --prefix web && bash scripts/deploy-local-binary.sh

# 4. 验证
curl -sk https://127.0.0.1:8443/healthz
```

- 控制台：https://127.0.0.1:8443 （首次启动自动创建 `SEED_ADMIN_USERNAME` 管理员）
- 状态页：https://127.0.0.1:8443/status（公开，无需登录）
- 8080 端口行为由 `GATEWAY_HTTP_REDIRECT` 决定：`true` = 仅 301 重定向到 HTTPS（默认）；`false` = 直接明文服务（测试/内网用，compose 默认关闭重定向）

### 首次调用示例

```bash
# 控制台 → API Keys 创建 Key（明文仅显示一次）
curl -sk https://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer sk-xxx" \
  -H "Content-Type: application/json" \
  -d '{"model":"deepseek-chat","messages":[{"role":"user","content":"你好"}]}'
```

未配置供应商时，网关在 `providers` 表为空的首启会自动按 `SEED_PROVIDER_*` 环境变量种入默认上游。

## 配置说明（环境变量）

| 变量 | 默认 | 说明 |
|---|---|---|
| `GATEWAY_DATABASE_URL` | — | PostgreSQL 连接串（compose 内为 `@postgres:5432`） |
| `GATEWAY_HTTPS_ADDR` / `GATEWAY_HTTP_ADDR` | `0.0.0.0:443` / `0.0.0.0:80` | 监听地址 |
| `GATEWAY_TLS_CERT` / `GATEWAY_TLS_KEY` | `certs/cert.pem` / `key.pem` | TLS 证书（P-256 自签或 CA 签发） |
| `GATEWAY_MASTER_KEY` | — | AES-256-GCM 主密钥（64 hex），**必须更换**；缺失时 JWT 密钥由它派生 |
| `GATEWAY_AUTH_MODE` | `api_key` | `none` = 跳过 API Key 鉴权（仅本地开发） |
| `GATEWAY_RELOAD_INTERVAL` | `30` | 路由/限流/供应商配置热加载周期（秒） |
| `GATEWAY_RATE_IDLE_EXEMPT_SECS` | `60` | 限流恢复豁免阈值：主体静默 ≥ 该秒数后恢复的首请求若被限流则豁免放行（每空闲间隙至多一次）；`0` = 关闭。见「限流与长时间空闲恢复」 |
| `GATEWAY_WEB_DIR` | `web/dist` | 控制台静态资源目录；目录不存在时 `/` 返回服务信息 JSON |
| `GATEWAY_JWT_SECRET` / `GATEWAY_ACCESS_TOKEN_TTL` / `GATEWAY_REFRESH_TOKEN_TTL` | 派生 / `900` / `2592000` | 控制台会话 |
| `LDAP_URL` / `LDAP_STARTTLS` / `LDAP_BIND_DN` / `LDAP_BIND_PASSWORD` / `LDAP_BASE_DN` / `LDAP_USER_FILTER` / `LDAP_ADMIN_GROUPS` | — | LDAP 登录（AD 默认 `sAMAccountName`，OpenLDAP 用 `uid`；不配置则仅本地账号） |
| `SEED_ADMIN_USERNAME` / `SEED_ADMIN_PASSWORD` | `admin` / `change-me-now` | break-glass 本地管理员（无本地管理员时首次启动创建） |
| `SEED_PROVIDER_NAME` / `SEED_PROVIDER_BASE_URL` / `SEED_PROVIDER_API_KEY` / `SEED_MODEL_PATTERN` | 空 | 首次启动种子上游（`providers` 表为空时生效） |

## API 概览

| 路径 | 鉴权 | 说明 |
|---|---|---|
| `GET /healthz` | 无 | 存活探针 |
| `GET /` | 无 | 服务信息 JSON（配置了前端目录时为控制台页面） |
| `POST /v1/chat/completions`、`/v1/completions`、`/v1/embeddings`、`/v1/responses`、`GET /v1/models` | Bearer API Key | OpenAI 兼容调用面 |
| `POST /v1/messages` | Bearer API Key | Anthropic Messages 调用面（Claude Code / Claude SDK；错误体为 Anthropic 形状） |
| `GET /api/status` | 无 | 公开状态：组件状态 + 30 天可用率 + 事故 |
| `POST /api/auth/login`、`/refresh`、`/logout` | — | 控制台会话（LDAP / 本地账号） |
| `GET /api/me`、`/api/me/plan`、`/api/keys`、`/api/usage`、`/api/usage/trend` | JWT | 个人资料、我的套餐、Key 管理、用量查询 |
| `GET/POST /api/admin/users`、`/providers`、`/routes`、`/rate-limits`、`/quotas`、`/prices`、`/models`、`/settings/ldap`、`/settings/extra-body`、`/audit`、`/usage/realtime` 等 | JWT + 管理员 | 管理后台 CRUD 与运维接口 |
| `GET/POST /api/admin/plans`、`/api/admin/groups`、`/api/admin/plan-alerts`、`/api/admin/smtp` 等 | JWT + 管理员 | Coding Plan 套餐 CRUD 与用量回溯、成员管理（直连用户 / 加入分组：列表、候选检索、批量增删）、用户分组（成员管理 / LDAP 同步 / CSV 导出）、阈值告警流、SMTP 设置 |

错误约定：API 错误统一返回 `{"error": {"message", "code", "type"}}` 形状；限流 429 带 `Retry-After`；Coding Plan 配额拦截 429（`insufficient_quota`）同样带 `Retry-After`（距当前统计周期边界的秒数，上限 24h；`total` 周期无边界则不带）——周期内重试必然失败，客户端应等到边界而非空转重试；`/v1/messages` 方言保留 Plan 拦截的明细消息（套餐名/周期/用量），便于区分「限流（短退避可重试）」与「配额耗尽（等到周期边界）」。上游错误透明透传。

## 状态页监测语义

供应商状态 = **主动健康探测 × 调用统计** 双证据融合（`src/service/health.rs`）：

- **主动探测**：网关对每个启用中的供应商 `GET {base_url}/1/status`（携带该渠道自己的鉴权头，超时 5s，网络错误重试 1 次，结果缓存 30s），按响应综合判定：
  - 2xx + `status` 字段为 ok/healthy/up（及正常/降级/故障中英文词汇表）→ 正常 / 性能下降 / 不可用
  - 2xx + 无 `status` 字段但含服务标识（service/server/version/success=true 等）→ 按连通性判正常
  - 2xx + 非 JSON / 无法识别格式 → 性能下降（可达但无法确认服务身份，防劫持页误报）
  - 连接失败 / 超时 / 5xx / `status` 自报故障 → 不可用
  - 404 / 401 / 403 / 429 → 非结论性，回退第二跳 `GET {base_url}/models`（拼接方式与代理转发一致）：2xx + 模型列表（object="list"/data 数组）→ 正常；401/403 → 性能下降（端点存活但渠道 Key 失效/无权限）；404/429/网络失败 → 仍非结论性，回退调用统计（服务可达，不武断判死）
- **调用统计**（被动证据）：`usage_logs` 真实调用记录（`status >= 400` 视为失败；鉴权/限流 4xx 在进入代理管线前返回，不写入日志，不污染错误率）——窗口错误率判定，优先近 10 分钟，样本不足（<5 次）依次回退 60 分钟、24 小时；≥50% 不可用、≥10% 性能下降、否则正常、24 小时无流量未知
- **融合规则**：探测不可用 → 直接不可用（结论性证据）；探测存活 → 在探测结论与「调用统计（至多性能下降）」中取更差者（探测正常 + 近期调用错误率高 → 性能下降并在详情中说明，常见为 Key 失效/配额等调用侧原因）；探测非结论性 → 完全采用调用统计
- **30 天可用率**：按 UTC 日聚合成功率，绿/黄/红/深红分级
- **事故**：单小时桶调用 ≥5 且错误率 ≥50% 判定事故小时，相邻小时自动合并为一次，峰值 ≥80% 标为重大，进行中（<1 小时）标注「进行中」
- 系统组件（网关/PostgreSQL）只有实时状态，无历史可用率（如实显示）


## 限流与长时间空闲恢复

### 策略与空闲期配额语义

三层规则（API Key → 用户 → 全局）均为**进程内令牌桶**：桶容量 = `burst`，回填速率 = `rpm/60` 每秒，请求消耗 1 个令牌，不足即 429 + `Retry-After`（取全部命中桶中的最大等待秒数，上限 24h，并加 [0,1s) 随机抖动——避免所有被拒客户端按相同报头值整秒对齐重试、互相挤兑每秒唯一回填令牌）。多规则两阶段判定：全部命中桶都足额才统一扣减，拒绝路径**零扣减**（不会白烧其他桶令牌）。空闲期间**没有任何扣减**——桶按真实流逝时间惰性回填直至 `burst`（`rpm=1, burst=5` 时空闲 5 分钟即回满）。回填时钟在 Linux 上取 `CLOCK_BOOTTIME`（含系统挂起/睡眠时间；`std::time::Instant` 的 `CLOCK_MONOTONIC` 在机器睡眠时不前进，会导致睡眠等待后的令牌少回填）。

### 长时间等待后恢复触发 429 的根因

agent 暂停等待用户确认期间，**共享作用域桶（尤其 global）会被其他会话/其他用户的流量耗尽**。恢复执行的首请求撞上已耗尽的桶 → 429，`Retry-After = 60/rpm` 秒——低 rpm 配置下即“额外冷却分钟级才能用”。该主体的 user/key 桶哪怕已回满也无济于事，因为命中即拒作用于任一层。叠加因素：恢复瞬间客户端并发齐发（主循环 + 并行子代理），burst 再大也可能被瞬间打穿。

### 修复：空闲恢复豁免（resume exemption）

网关无法感知客户端“暂停/恢复”，因此不做计时暂停（服务端不可实现），改为**按可观测的空闲间隙豁免**：

- 每个主体（`u:{user_id}|k:{key_id}`）记录最近一次活动时间，**任意结局的请求（含被拒）都刷新活动时间**；
- 主体静默 ≥ `GATEWAY_RATE_IDLE_EXEMPT_SECS`（默认 60s）后的**首个请求**若被某层规则拒绝，则豁免放行（不扣空桶、不重置任何桶），并记录 `rate limit resume exemption granted` 日志；
- 每个空闲间隙至多豁免一次；主体首次出现（冷启动）视同长空闲后恢复；
- 高频请求永远攒不出空闲间隙，豁免零影响——防压制能力不变。

**防滥用边界**：豁免最坏放大 = 每主体每阈值窗口 1 个请求（仅当拒绝桶已耗尽时发生）；N 个主体交替静默可造成聚合速率超出 global 上限 `N/阈值`，阈值即调节旋钮；登录限流（`login:*` 桶）不参与豁免。

**运维要点**：global 层 rpm 决定 429 后每令牌冷却时间（`60/rpm` 秒），面向并行 agent 客户端建议 ≥ 数百 rpm + 百级 burst；本仓演示数据原为 `rpm=1, burst=5`（每分钟 1 请求），已调整为 `600/100`。豁免只解决“恢复首请求”，恢复后的持续速率仍受规则约束——客户端应遵循 `Retry-After` 重试（OpenAI 兼容客户端均支持）。

### 并发上限（在途请求数）

令牌桶限的是**放行速率**，不是同时在途数——低 rpm 下长流式响应仍会累积大量在途请求（稳态在途 ≈ `放行速率 × 平均响应时长 + burst`）。限流规则另附 `concurrency` 字段（0 = 不限）实现真正的并发控制：

- **计数维度**：与令牌桶同桶键（scope × 模型精确匹配），请求进入代理管线（鉴权、解析、端点开关之后）占坑，**响应体流尽或客户端断开才释放**——与实时监控的并发计数同生命周期；管理员测试调用（`proxy_test`）不占坑；
- **判定顺序**：先占并发名额、后扣令牌桶——并发满的 429 不会白烧令牌；反之被令牌桶拒绝的请求只瞬持名额（微秒级）即释放。多规则叠加时须**全部**命中桶有余量才放行（全有或全无，拒绝路径零占坑）；
- **拒绝语义**：429，错误消息为 "Concurrency limit exceeded, retry after Ns"（`rate_limit_exceeded` 类型不变，客户端按常规 429 退避；`Retry-After` 给 1-2s 短提示——名额何时释放不可知），`/v1/messages` 方言映射为 `rate_limit_error`；
- **不参与空闲豁免**：并发名额是真实资源占用，豁免会突破上限；令牌桶空闲豁免语义不变。

### 已知边界

- 单实例语义：豁免状态、令牌桶与并发计数均在进程内，重启清零（重启后冷启动豁免兜底）；多实例需 Redis 实现。
- 上游 429 与网关自身 429 相互独立：非流式按降级链重试上游 429/5xx；流式不重试（防重复生成），上游 429 原样透传。
- 流式请求在网关自身限流处被拒时同样返回 429 JSON（未建立上游连接，无计费）。

## Coding Plan 周期用量限制与小时级重置

用户经分组或直连两种通道加入套餐（Coding Plan）：token 上限 + 统计周期 + 超额策略（`block` 拦截 429（`insufficient_quota` + `Retry-After`=距周期边界秒数，`total` 周期除外）/ `downgrade` 降级到指定模型并走出站重建路径 / `log` 仅告警）。分组与用户均为多对多关联（`plan_groups` / `plan_users`，主键即唯一约束防重复添加）；用户命中多个入口时按 `priority` 最高者整体生效（同分取 `plan_id` 大），多套餐不叠加计量——需要「小时 + 月」双重限值应拆两个分组用 priority 表达。成员管理入口在 Coding Plan 管理页（直连用户 / 加入分组，选择器分页检索 + 批量增删；重复加入返回 409）。

**统计周期**：`daily`（UTC 当日 00:00 起）/ `monthly`（UTC 自然月）/ `total`（不重置）/ `hourly`（小时级滚动重置）。

### 小时级重置（hourly）语义

滚动桶而非滑动窗口：时间轴按锚点切成连续的 N 小时桶，请求计入其开始时刻所在的桶；翻桶后旧桶封存、余量归零。

- `period_hours`：窗口长度 1..=168（上限一周）
- `period_anchor_mode`：
  - `fixed`：UTC 整点网格，桶界 = `epoch + k·N·h`。仅当 N 整除 24 时桶界才与每日 00:00 对齐，N=5/7 等跨日相位漂移是有意行为（纯网格，全局均匀无重叠）
  - `join`：按成员开通时间（分组成员取 `user_group_members.added_at`，直连用户取 `plan_users.added_at`）偏移切桶；移除后重新加入视同重新开通（新桶、余量重置）
- **重置是推导出来的，不是调度出来的**：无任何清零定时任务。预检查内存缓存按 `period_key`（hourly 为 `h:<桶起点 epoch 秒>`）翻转归零，`plan_usage_counters` 按新 `period_start` 自然落新行，阈值告警去重键含 period_key 故新周期自动重新武装；旧周期数据保留可回溯

### 计量与一致性

- `plan_usage_counters (user_id, plan_id, period_start TIMESTAMPTZ)` 与 `usage_logs` 同事务 UPSERT（`tokens` 饱和累加防 i64 回绕）；内存当期计数缓存承担热路径预检查，启动 / 30s 周期从 DB 快照重载兜底
- 周期界全部按 UTC 计算（SQL 显式 `AT TIME ZONE 'UTC'`；hourly 当前桶判定用 `(now − N·h, now]` 开区间窗，与 N、锚点解耦），数据库会话时区不影响结果
- 重启 / 宕机错过翻桶无需补偿：`period_start` 为纯时间函数，重启后首请求直接命中当前桶；时钟回拨或未来锚点钳位，不产生负槽位
- 阈值告警 80/95/100%：站内通知 + 邮件（SMTP）+ webhook，进程内 seen 与 `plan_alerts UNIQUE(plan_id, user_id, period_key, level)` 双层去重

### 模型作用域（model_scope）

Plan 可选限定生效模型：`{"allow": ["claude-*", "gpt-4o"], "deny": ["*-preview"]}`（NULL = 全模型，存量行为不变）。

- **pattern**：精确（`gpt-4o`）或尾缀 `*` 前缀通配（`claude-*` 覆盖全家族含日期后缀变体），与路由规则同语义；大小写不敏感；合法字符 `字母数字 . _ + : - /` 与尾缀 `*`。中间 `*` / 裸 `*` / 未知字段在写入期 400 报错
- **黑白名单**：deny 命中即排除、优先于 allow；allow 缺省 = 除 deny 外全放行；黑白名单同条目写入期报错
- **择优**：作用域更具体者优先（仅精确 = 2 > 含通配 = 1 > 未配置 = 0，压过 priority），同具体度按存量 `(priority, plan_id)`；多候选命中时 warn 留痕（选中者 + 落选者及各自具体度），排查指南见设计文档
- **拼写防线**：写入期精确条目须命中任一路由 `model_pattern`，否则 400 `unrecognized model ...`（只拦「未建路由的家族」；前缀路由无法识别家族内拼写差异）；绕过 API 手改 DB 的损坏 scope 在加载期剔除该 Plan 并 error 留痕（fail-closed），修复后随 reload 恢复
- **动态生效**：作用域按每请求 `model` 字段求值——会话中切换模型的下一个请求即按新模型重评估；改配置走存量 reload 即时生效。检查与记账同源（检查期解析的 Plan 直接作为计量载荷），降级用量记入触发降级的 Plan

示例：`{"allow": ["claude-*"], "deny": ["claude-2*"]}`（Claude 家族专用、排除 2.x）；`{"deny": ["gpt-4o", "claude-opus-*"]}`（除旗舰外全放行）。

管理端点：`/api/admin/plans`（CRUD + 用量回溯 + 成员管理：`{id}/users`、`{id}/groups` 列表 / `*-candidates` 选择器 / `*/add`、`*/remove` 批量增删，重复加入 409、参数错误 400；`model_scope` 三态：absent=保留 / null=清空 / 对象=设置）、`/api/admin/plan-alerts`、`/api/me/plan`（我的套餐，可选 `?model=` 按模型做作用域感知解析）；设计细节见 `docs/plans/hourly-reset-design.md` 与 `docs/plans/model-scope-design.md`。

## 数据模型

核心表（迁移见 `migrations/`，随二进制内嵌自动执行）：

| 表 | 用途 |
|---|---|
| `users` / `api_keys` | 本地 + LDAP 镜像账号；API Key（哈希 + 前缀） |
| `providers` / `model_routes` | 上游供应商（Key 加密存储、`api_type` 区分 Responses 原生/转换）；模型路由 + 降级链 |
| `usage_logs` | 用量明细（`request_id` 唯一防重复记账，按用户/时间索引） |
| `user_monthly_usage` / `usage_daily` | 配额计数（记账事务内 UPSERT）；仪表盘聚合（60s 后台任务） |
| `coding_plans` / `user_groups` / `user_group_members` | 周期配额套餐（token 上限 / 统计周期 / 超额策略 / `model_scope` 模型作用域 JSONB）与用户分组（LDAP 联动同步） |
| `plan_users` / `plan_groups` | Plan ↔ 用户 / 分组多对多关联（主键即唯一约束防重复添加；随 Plan 删除级联清理） |
| `plan_usage_counters` / `plan_alerts` | Plan 当期计数（记账事务内 UPSERT，`period_start` TIMESTAMPTZ）与阈值告警（`period_key` 去重） |
| `refresh_tokens` / `audit_logs` / `aggregation_state` | 会话、审计、聚合水位 |

## 本地开发

```bash
# 后端：需要本地 PostgreSQL（或 docker compose up -d postgres）
cargo run                      # 读取 .env，监听 8443/8080
cargo test                     # 单元测试 235 项（健康探测判定/融合 + Responses/Anthropic 转换 + SSE 状态机 + 限流回归）

# 前端（Vite dev server，/api 与 /v1 代理到 8443）
cd web && npm install && npm run dev
npm run build                  # vue-tsc 类型检查 + 产物构建（提交前必跑）

# 无真实上游时本地模拟
python3 scripts/mock_upstream.py   # http://127.0.0.1:9001/v1，支持 chat（含 tools/reasoning/错误场景）/responses
                                   # 另暴露 GET /1/status：MOCK_STATUS_MODE 或 /tmp/mock_status_mode 可切 ok/degraded/down/html/shapeless/404/500/timeout

其他脚本：`scripts/gen-cert.sh`（自签证书）、`scripts/deploy-local-binary.sh`（本机编译 release 后打运行时镜像）、`scripts/seed-ldap.sh`（演示用 OpenLDAP 种子数据，配合 `--profile ldap`）。

## 已知限制

1. **单实例语义**：限流令牌桶、配额计数缓存、路由表均在进程内，多实例部署需换 Redis 实现（已封装，接口不变）。
2. **记账同步写入**：PostgreSQL 不可用时请求失败（内部系统可接受），无丢失窗口。
3. **`usage_logs` 不分表**：规模增长后需按月 RANGE 分区（`request_id` 唯一约束需配合调整）。
4. **流式依赖上游 `include_usage`**：不支持的供应商暂走零值兜底（启发式估算未实现）。
5. **流式断连记账**：客户端中途断开时流式记账可能丢失（预存架构问题，非 Responses 转换引入）。
6. **Responses 升级路径未做**：Chat 客户端 → Responses 原生上游的转换未实现（目标上游均兼容 Chat，无收益）。

## 相关文档

- `docs/plans/2026-08-08-llm-gateway-design.md` — 定稿设计（数据模型、安全设计、权衡清单）
- `docs/plans/2026-08-10-responses-api-port.md` — Responses API 兼容层移植设计（转换规则、SSE 状态机、回归步骤）
- `docs/plans/hourly-reset-design.md` — Coding Plan 小时级重置设计（滚动桶模型、边界场景、灰度发布与回滚）
- `docs/plans/model-scope-design.md` — Coding Plan 模型作用域设计（pattern 语法、黑白名单、择优规则、冲突排查指南）
