# 会话状态（compact 交接）

> 2026-08-08 生成。用于上下文压缩后恢复。P1–P3 全部完成并验证。

## 项目：企业 LLM API 网关（Rust/axum/PG + Vue3 控制台）

目录 `/home/pop/AI/code/llm_gateway`。单二进制模块化单体：TLS 终结（rustls）、`/v1/*` OpenAI 兼容代理、控制台 API + Vue dist 静态托管、后台聚合。无 Redis、无队列、无 Nginx。本地冒烟端口 `https://127.0.0.1:8443`（443 需 root）+ mock 上游 `127.0.0.1:9001`。

## 里程碑状态

| 阶段 | 状态 | 内容 |
|---|---|---|
| P1 | ✅ | /v1 代理（鉴权/限流/配额/路由/流式/记账）、TLS、Docker、迁移 0001、11 张表 |
| P2 | ✅ | LDAP 登录+JIT+管理员组、JWT 会话（access 15min/refresh 旋转）、自助 Key、管理 API（用户/审计）、种子管理员、v1 鉴权补用户 status 校验 |
| P3 | ✅ | Vue3 控制台（登录/仪表盘 ECharts/Keys/用量/用户管理/审计）、Rust 静态托管+SPA fallback、Dockerfile 多阶段 |
| P4 | ✅ | 配置管理：providers/routes/rate-limits/quotas/prices CRUD API（写后即时 reload+audit）+ 前端 4 配置页（系统配置子菜单）+ 配额超限审计告警（按用户×月去重） |
| P5 | ✅ | 供应商模型探测（test-connection 拉 /models 列表）+ 用户×供应商/模型访问授权白名单（迁移 0002 user_access_rules，即时生效，admin 跳过，/v1/models 按用户过滤） |
| 后续 | ⏭ | 计量二级/三级策略细化、配额告警通知渠道（webhook）、多实例部署（Redis 限流/配额缓存） |

## 运行环境（冒烟中，服务保持运行）

- 网关：hub 名 `gateway`，`https://127.0.0.1:8443`，二进制 `target/debug/llm_gateway`，`GATEWAY_WEB_DIR=web/dist`（默认）
- mock 上游：hub 名 `mock-upstream`（scripts/mock_upstream.py，模型 mock-1）
- PG：docker 容器 `llm_gateway-postgres-1`（gateway/gateway@localhost:5432/llm_gateway）
- LDAP：docker 容器 `ldaptest`（127.0.0.1:3890，cn=admin,dc=example,dc=org / adminpw）；种子导入用 `scripts/seed-ldap.sh`（compose 有 `ldap` profile 服务，勿直接挂载 ldif——osixia 会消费删除）
- 账号：`admin/newadmin456`（本地种子管理员，id=2）、`alice/alicepw`（LDAP 普通，id=1，有 15 次调用数据）、`bob/bobpw`（LDAP 管理员，id=4）

## 关键实现要点

- JWT：HS256，Claims{sub:String, username, is_admin, tv, jti, iat, exp}；**sub 必须是字符串**（jsonwebtoken 9 数字 sub 报 MissingRequiredClaim）；`GATEWAY_JWT_SECRET` 缺省 = sha256(master_key)
- 会话：refresh token 64-hex 只存 SHA-256，单次旋转；强退/禁用/重置密码 = token_version++ + 吊销全部 refresh
- 禁用用户：v1 鉴权 JOIN users 查 status（P2 补的洞）；`AppError::Auth`=401 `invalid_api_key`，`Unauthorized`=401 `unauthorized`，`Forbidden`=403
- LDAP 管理员判定：memberOf 精确 DN / cn 后缀 / groupOfNames 补充搜索（无 overlay 时）
- sqlx 严格类型：NUMERIC 聚合需 `::float8` / `CAST(... AS BIGINT)`
- 静态托管：`/assets` ServeDir + fallback（Accept: text/html 且非 /api /v1 前缀 → index.html，否则 JSON 404）
- **前端坑（已修）**：pinia getter 读 localStorage 非响应式 → computed 缓存 `false` → 守卫把登录后 push 重定向回 /login → vue-router duplicated 静默吞导航。token 必须进 pinia state。**同类模式（非响应式外部状态进 getter）一律禁止**

## 前端结构（web/）

Vue 3 + TS + Vite 6 + Element Plus + ECharts + Pinia + vue-router。`npm run build` = vue-tsc 严格检查 + vite build（dist 2.6M）。dev 代理 /api、/v1 → https://127.0.0.1:8443。页面在 `web/src/views/`（Login/Layout/Dashboard/Keys/Usage/Users/Audit），API 契约在 `web/src/api/types.ts`。

## 常用操作

- 构建验证：`cargo build`（零警告）+ `cd web && npm run build`（vue-tsc 零错）
- 起服务：`hub start`（gateway / mock-upstream），ready 用端口探测
- 登录冒烟：`curl -sk -X POST https://127.0.0.1:8443/api/auth/login -H 'Content-Type: application/json' -d '{"username":"admin","password":"newadmin456"}'`
- 浏览器冒烟：自签证书需 `--ignore-certificate-errors`；browser 工具连不上时手动起 `google-chrome --headless=new --no-sandbox --disable-gpu --ignore-certificate-errors --remote-debugging-port=9222` 再用 `cdp_url` 连接
- LDAP 冒烟环境变量：`LDAP_URL=ldap://127.0.0.1:3890`、`LDAP_BIND_DN=cn=admin,dc=example,dc=org`、`LDAP_BIND_PASSWORD=adminpw`、`LDAP_BASE_DN=dc=example,dc=org`、`LDAP_USER_FILTER=(&(objectClass=inetOrgPerson)(uid={0}))`、`LDAP_ADMIN_GROUPS=admins`

## P4 待办（已交付）

- 后端：`src/store/config.rs`（5 类 CRUD）+ `src/api/admin_config.rs`（`/api/admin/providers|routes|rate-limits|quotas|prices`，require_admin + audit + 写后 `st.reload()` 即时生效）
- 前端：`web/src/views/Providers.vue / Routes.vue / Limits.vue / Prices.vue`（系统配置子菜单）
- 配额告警：`check_quota` 超限时按 `(user_id, month)` 去重写 audit `quota.exceeded`（AppState.quota_alerts）
- 已知坑：
  - **Postgres `DISTINCT ON` 与 `::float8` 同层时 sqlx describe 返回 NUMERIC**（空表不触发，有数据才爆）——子查询包一层外层投影解决（rules.rs load_prices）
  - `INSERT ... RETURNING col::float8` 同理——两步写（INSERT RETURNING id → 再 SELECT 完整行）
  - 前端 `enabled` 字段必须是布尔（0/1 会 400）——Providers.vue 已修
  - 测试数据状态：provider-3/mock-3 路由已禁用保留作示例，限流规则已删

## P5 访问授权（已交付）

- 表：`user_access_rules(user_id, provider_id NULL=任意, model_pattern NULL=全部, UNIQUE 三元组)`；迁移 0002
- 语义：**用户无规则 = 默认放行**（兼容既有账号）；有规则 = 白名单命中其一；admin 跳过；规则写入即 reload 生效
- API：`POST /api/admin/providers/test-connection`（表单实时 base_url+key 调上游 /models，10s 超时，SSRF 风险 admin-only 可接受）、`GET/PUT /api/admin/users/{id}/access`（PUT 全量替换，空数组=清空恢复默认放行，audit user.access.update）
- 代理路径：`proxy.rs` resolve 后按候选 provider 过滤（**降级链也过滤**）；`/v1/models` 鉴权后按用户过滤
- 前端：Providers.vue 表单内「测试连接并获取模型」按钮 + 模型 tag 展示；Users.vue 每行「模型授权」dialog（provider 下拉含任意 + model pattern 通配输入，白名单/默认放行状态提示）
- **修复既有 bug**：`resolve_route` 注释称精确优先但实现是线性扫描（mock-* 吞 mock-2 请求）——精确命中优先 + 通配兜底；授权功能暴露此问题
- 授权检查缓存：`AppState.user_access`（HashMap<user_id, rules>）+ `admin_ids`（HashSet，reload 热更新）

## 已知权衡（设计文档 §10）

进程内限流/配额仅单实例精确；记账同步写（PG 不可用即失败）；usage_logs 不分区；流式 usage 依赖 include_usage。

## P6 模型库（已交付）

- 表：`models(id, provider_id FK CASCADE, model_id, UNIQUE(provider_id, model_id))`；迁移 0003
- 入口：① test-connection 带 provider_id 成功时自动入库（audit provider.models.sync）；② `POST /api/admin/models/refresh` 并发拉取全部启用供应商的 /models 并整表替换（单个失败不影响其余，返回 updated/failed 汇总）
- API：`GET /api/admin/models`、`DELETE /api/admin/models/{id}`（audit model.delete）
- 前端：Models.vue「模型库」页（列表 + 刷新模型库 + 删除）；路由规则/模型价格页模型字段改为 el-select（filterable + allow-create，可下拉模型库选项也可手输 * 通配）
- test-connection 重构：重试逻辑提取为 `fetch_upstream_models()`（3 次退避/15s/错误链展开），test-connection 与 refresh 共用

## 容器化部署（本机编译 + runtime 镜像）

- `scripts/deploy-local-binary.sh`：本机 `cargo build --release`（约 1 分钟）→ `docker-compose build gateway`（Dockerfile.runtime 基于 ubuntu:24.04，仅 COPY 产物，秒级）→ up -d
- 坑：GLIBC（本机 2.39 vs bookworm 2.36 → 换 ubuntu:24.04）；.dockerignore 需放行 `!target/release/llm_gateway` 等；容器内 127.0.0.1 是容器自身 → extra_hosts host.docker.internal + DB base_url 改 host.docker.internal；compose environment 覆盖 .env 的 dev 值（127.0.0.1/localhost 会污染）
- `Dockerfile`（容器内编译版）保留备用；`GATEWAY_HTTP_REDIRECT=false` + 8080 直出模式冒烟过

## P7 管理员全站用量（已交付）

- 新端点 `GET /api/admin/usage`（admin-only）：全站 by_model/last_7_days + 本月按用户（by_user，LEFT JOIN 防 usage_logs.user_id NULL）+ 本月按 API Key（by_key，JOIN api_keys，key_prefix 展示）
- 前端 Usage.vue：admin 角色显示「汇总 / 按用户 / 按 API Key」radio 切换，数据源按角色分流（admin → /api/admin/usage，普通用户 → /api/usage 保持仅自己）
- 验证：admin 全站 29 次 alice 用量（3 key 拆分）；alice 调 admin 端点 403；alice /api/usage 无 by_user 字段
- 排障记录：LDAP 冒烟容器重建后 **docker exec -f /dev/stdin 的 heredoc 导入静默失败**（ldapadd 无报错但 0 条目）——必须 docker cp + 容器内文件路径（seed-ldap.sh 本就正确）；另 ldaptest 手动重建需 `-p 3890:389`（0.0.0.0），网关容器经 host.docker.internal 访问，绑定 127.0.0.1 会 Connection refused

## P8 用量趋势图表（已交付）

- 新端点 `GET /api/usage/trend?days=N`（本人）与 `GET /api/admin/usage/trend?days=N`（全站 daily + by_user 序列），days clamp 1..=90 默认 30
- SQL：usage_daily LEFT JOIN generate_series 补零到完整日期范围（图表 x 轴连续）；by_user 平铺后 Rust 侧分组 + 补零对齐
- 前端 Usage.vue summary 视图：3 张 echarts 图（调用次数折线+面积 / 输入·输出 Token 双线 / 成本柱状）；管理员额外有用户下拉（0=全部）+ 7/30/90 天范围 radio；切视图模式（users/keys 表格）回 summary 时 dispose 重建图表实例（DOM 销毁陷阱）
- 验证：admin 30 天 30 点补零、by_user alice+bob 各 30 点；alice 端点仅自己、403 保护；days=0→1、999→90；浏览器三图渲染（canvas 像素非空）、用户下拉切换（alice/bob）、7/90/30 天切换触发对应 API 请求
- 坑：el-option :value="null" 不匹配选中（改 0=全部）；PG % 是 trunc 模（造数负值，用 abs 修正）；deploy 脚本需在仓库根执行（cd 后相对路径失效）

## P9 用量趋势半小时粒度（已交付）

- trend 端点新增 `granularity=half_hour`：近 24h 每 30 分钟桶（`to_timestamp(floor(extract(epoch from created_at)/1800)*1800)`，UTC 桶对齐），usage_logs 直查；`fill_half_hour_gaps` 补零到 48 点（当前桶 -47×30min 起）；admin 版 by_user 同粒度分组补零；非法值回落 day 粒度
- 前端范围 radio 加「今天（30 分钟）」(value=0)；`isHalfHour` computed 决定请求 `granularity=half_hour&days=1`；x 轴标签按粒度格式化（day → MM-DD，half_hour → 本地 MM-DD HH:mm），axisLabel hideOverlap 防 48 点重叠
- 验证：admin 48 点（07-07T14:00Z~08-08T13:30Z），9 非零桶计数正确；by_user alice 48 点 9 非零 / bob 48 点 1 非零；alice 端点仅自己；浏览器 radio 切换触发 half_hour 请求、三图重绘、bob 用户切换正常
- 造数：今天 12 时段 usage_logs（alice 4 桶 ×4 条 + bob 1 条）
- 坑：fill_half_hour_gaps 的 `mut rows` 未用（into_iter 消费）→ unused_mut 警告已修

## P10 /v1/models 合并模型库 + API Key 可再次复制（已交付）

- `/v1/models`：模型库（models 表 JOIN providers）+ 已启用路由的具体 pattern（`ends_with('*')` 通配 pattern 不列出，不可作为模型名提交）合并去重；授权过滤：无 key/admin 全见，普通用户按 provider 走 user_can_use；DB 查询失败回落空列表（unwrap_or_default）
- API Key 可再次复制：迁移 0004 加 `api_keys.key_encrypted TEXT`；新 `src/service/keys_crypto.rs`（AES-256-GCM，密钥 = GATEWAY_MASTER_KEY，nonce12 || ct，base64）；create_key 加密落库（key_hash 鉴权不变）；`GET /api/keys/{id}/reveal`（仅本人+仅启用，解密失败=主密钥变更报错；旧 Key 无副本 → 400）；audit 记 api_key.reveal
- 前端 Keys.vue：操作列加「复制 Key」按钮（loading 防连点），reveal 后 clipboard 写入
- 验证：/v1/models 无 key = [mock-1, deepseek-v4-flash, deepseek-v4-pro, mock-2]（mock-* 不再出现）；reveal 往返明文一致；不存在 id / 他人 key → 400；浏览器点击发出 reveal 请求且 200；headless clipboard 被权限模型拒（"Document is not focused"）→ 错误分支正常
- 坑：store/keys.rs find_encrypted 初版 bind 顺序写反（user_id 绑到 $1=id）已修；浏览器 `tab.fill` 会卡（登录改用 evaluate 填表）；clipboard 验证受 headless 权限限制

## P11 模型库入库自动兜底路由（已交付）

- 问题：/v1/models 列出模型库模型（deepseek-v4-flash/pro）但 model_routes 无对应路由 → 调用 400 "not routed to any provider"
- 修复：`replace_provider_models`（test-connection / models/refresh 共用）事务内，对每个入库模型检查 enabled 路由能否命中（pattern_hits：尾缀 * 前缀匹配或精确，与 routing::matches_pattern 同语义，内联避免 store→service 依赖）；无命中才自动 INSERT 精确路由（priority 默认 100、无 fallback）；已有通配/精确路由则不动（尊重手动配置，如 mock-1 被 mock-* 命中不重复建）
- test_connection / refresh_models 成功后立即 `st.reload().await`（缓存即时生效，不等 30s）
- 验证：refresh 后路由新增 deepseek-v4-flash/pro（provider 4），mock-1/mock-2 未重复建；alice key 调 deepseek-v4-flash/pro 真实上游返回正常对话、mock-1 仍走 mock-* 路由

## P12 用户请求对比复合图（已交付）

- 需求：所有用户请求画在同一张图上，支持「请求最多 Top 10 / 请求最少 Bottom 10 / 全部用户」过滤，便于找出用量最多/最少用户
- 实现（纯前端，web/src/views/Usage.vue，后端 by_user 已含全部用户每日序列）：
  - summary 视图新增第 4 张图「用户请求对比」（v-if isAdmin）：多系列折线，每用户一条线，legend type scroll
  - rankMode ref（'top'|'bottom'|'all'，默认 top）+ el-radio-button 切换，@change=renderCompareChart
  - compareSeries()：按 call_count 总量排序取前/后 10（total = daily.reduce 求和），名称 display_name ? `${username}（${display_name}）` : username
  - compareChart 接入 init/dispose/resize/renderAllCharts 全链路；x 轴复用 trend.daily 的 xLabel（与 by_user 同粒度序列）
- 冒烟：npm run build ✓；部署后造数（alice 176/bob 16/admin 1 次）；Chrome CDP 验证「用户请求对比」卡片 + 4 canvas 渲染、Bottom10/Top10 切换 active 正常、无 pageerror
- 坑位：browser 工具 app.spawn 起 Chrome 失败 → bash 手动起 `google-chrome-stable --headless=new --remote-debugging-port=9222 --ignore-certificate-errors --no-sandbox` 再 cdp_url attach；登录页 placeholder 是「用户名（LDAP / 本地账号）」

## P13 限流字段中文化（已交付）

- Limits.vue：表格列 label「RPM（每分钟请求数）」→「每分钟请求数」、「Burst（桶容量）」→「突发上限」；表单 label「RPM」→「每分钟请求数」、「Burst」→「突发上限」；校验消息同步中文（'每分钟请求数必须为大于 0 的数字' / '突发上限必须为大于 0 的数字'）
- 仅前端文案改动；rpm/burst 字段名与 API 不变
- 冒烟：Chrome CDP 登录 → /limits 页表格列头与新建弹窗表单 label 均为中文，无 pageerror

## P14 配额列表缺用户 + 旧 Key 复制报错修复（已交付）

- 问题 1：/api/admin/quotas 只返回已配置配额的 1 个用户（user_quotas JOIN users，未配置的用户不出现）
  - 修复：store/config.rs list_quotas 改 `users LEFT JOIN user_quotas`，billing_day/notify_percent/enabled 用 COALESCE 默认值（1/80/TRUE），配额列 NULL = 不限制；列出全部 3 个用户
- 问题 2：P10 前创建的旧 Key（key_encrypted IS NULL）复制时报笼统 400 "key not found or not owned by you"
  - 修复：store/keys.rs find_encrypted 返回 `Option<Option<String>>`（None=行不存在/非本人；Some(None)=旧 Key 无密文；Some(Some)=密文）；console.rs reveal_key 对旧 Key 返回明确提示 "key was created before encrypted storage; delete and recreate the key to copy it again"，越权/不存在仍统一 400 文案
- 冒烟：配额列表返回 alice/admin/bob 3 用户（未配置者默认值+null 配额）；alice 复制旧 Key id=1 → 重建提示；复制新 Key id=7 → 明文；复制 admin 的 id=6 → 统一 400
