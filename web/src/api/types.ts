// 控制台 API 类型（与 src/api/console.rs 响应形状一一对应）

export interface UserInfo {
  id: number
  username: string
  display_name: string | null
  email: string | null
  source: 'local' | 'ldap'
  ldap_dn: string | null
  is_admin: boolean
  status: number
  last_login_at: string | null
  created_at: string
}

export interface LoginResp {
  access_token: string
  refresh_token: string
  token_type: string
  expires_in: number
  user: UserInfo
}

export interface MeResp {
  user: UserInfo
  usage: { month: string; tokens: number; cost: number }
  active_keys: number
}

export interface KeyMeta {
  id: number
  name: string
  key_prefix: string
  expires_at: string | null
  last_used_at: string | null
  created_at: string
  /** 是否可再次复制（加密存储上线前的旧 Key 为 false） */
  copyable: boolean
}

export interface KeysResp {
  keys: KeyMeta[]
}

export interface CreateKeyResp {
  key: string
  key_prefix: string
  id: number
  name: string
  expires_at: string | null
}

export interface ModelStat {
  model: string
  call_count: number
  input_tokens: number | null
  output_tokens: number | null
  cost: number | null
}

export interface DailyStat {
  stat_date: string
  call_count: number
  input_tokens: number
  output_tokens: number
  cost: number
}

export interface UsageResp {
  month: string
  by_model: ModelStat[]
  last_7_days: DailyStat[]
}

/** 管理员视角：本月按用户 */
export interface UserUsageRow {
  user_id: number | null
  username: string | null
  display_name: string | null
  call_count: number
  input_tokens: number | null
  output_tokens: number | null
  cost: number | null
}

/** 管理员视角：本月按 API Key */
export interface KeyUsageRow {
  key_id: number
  name: string
  key_prefix: string
  user_id: number | null
  username: string | null
  call_count: number
  input_tokens: number | null
  output_tokens: number | null
  cost: number | null
}

/** 管理员视角：本月按来源 IP */
export interface IpUsageRow {
  client_ip: string
  call_count: number
  input_tokens: number | null
  output_tokens: number | null
  cost: number | null
  last_seen: string
}

/** 管理员用量：全用户汇总 + 按用户 + 按 Key + 按 IP */
export interface AdminUsageResp extends UsageResp {
  by_user: UserUsageRow[]
  by_key: KeyUsageRow[]
  by_ip: IpUsageRow[]
}

/** 实时监控：近 5 分钟全站汇总 */
export interface RealtimeSummary {
  calls: number
  errors: number
  input_tokens: number
  output_tokens: number
  avg_latency_ms: number
  cost: number
}

/** 实时监控：单用户窗口统计（近 60 分钟内有调用的用户） */
export interface RealtimeUserStat {
  user_id: number | null
  username: string | null
  display_name: string | null
  calls_5m: number
  errors_5m: number
  calls_60m: number
  errors_60m: number
  input_tokens: number
  output_tokens: number
  cost: number
  avg_latency_ms: number
  last_call_at: string
}

/** 实时监控：最近请求明细行 */
export interface RealtimeCallRow {
  id: number
  request_id: string
  username: string | null
  model: string
  endpoint: string | null
  streamed: boolean
  input_tokens: number | null
  output_tokens: number | null
  latency_ms: number | null
  status: number | null
  cost: number | null
  created_at: string
}

/** 实时监控响应（管理员） */
export interface RealtimeUsageResp {
  now: string
  summary: RealtimeSummary
  users: RealtimeUserStat[]
  recent: RealtimeCallRow[]
}

/** 按日趋势点（图表） */
export interface TrendPoint {
  stat_date: string
  call_count: number
  input_tokens: number
  output_tokens: number
  cost: number
}

/** 单个用户趋势序列（管理员视图） */
export interface UserTrend {
  user_id: number
  username: string
  display_name: string | null
  daily: TrendPoint[]
}

/** 用量趋势响应 */
export interface TrendResp {
  days: number
  granularity: 'day' | 'half_hour'
  daily: TrendPoint[]
  by_user: UserTrend[]
  /** 本月有调用的模型列表（下拉过滤用） */
  models?: string[]
}

export interface UserWithUsage {
  id: number
  username: string
  email: string | null
  display_name: string | null
  source: string
  is_admin: boolean
  status: number
  last_login_at: string | null
  created_at: string
  month_tokens: number | null
  month_cost: number | null
}

export interface UsersResp {
  users: UserWithUsage[]
}

export interface AuditRow {
  id: number
  actor_id: number | null
  actor_username: string | null
  action: string
  target_type: string | null
  target_id: number | null
  detail: unknown | null
  created_at: string
}

export interface AuditResp {
  audit_logs: AuditRow[]
  total: number
}

// ---------- 配置管理（P4） ----------

export interface ProviderRow {
  id: number
  name: string
  api_type: string
  base_url: string
  api_key_encrypted: string
  timeout_ms: number
  enabled: boolean
  /** extra_body 透传配置（JSON 对象；{} = 未配置） */
  extra_body?: Record<string, unknown>
  created_at: string
}

export interface ProvidersResp {
  providers: ProviderRow[]
}

export interface RouteRow {
  id: number
  model_pattern: string
  provider_id: number
  priority: number
  fallback_ids: number[]
  /** 上游实际模型名（映射）；null = 透传客户端模型名 */
  upstream_model: string | null
  enabled: boolean
  /** 模型级开关：system 消息收拢到消息数组头部（qwen3「system message must
   *  be at the beginning」/ MiniMax 类严格上游） */
  strict_system_head: boolean
  /** 多条 system 收拢时是否合并为单条（true = 合并；false = 仅前移保持多条独立） */
  system_head_merge: boolean
  /** 模型级 extra_body（覆盖渠道级同名叶键；{} = 未配置） */
  extra_body?: Record<string, unknown>
}

export interface RoutesResp {
  routes: RouteRow[]
}

export interface RateRuleRow {
  id: number
  scope: string
  scope_id: number | null
  model: string | null
  rpm: number
  burst: number
  enabled: boolean
  updated_at: string
}

export interface RateRulesResp {
  rules: RateRuleRow[]
}

export interface HeaderSettingsResp {
  upstream_headers: Record<string, string>
  response_headers: Record<string, string>
}

export interface QuotaRow {
  user_id: number
  username: string
  monthly_token_quota: number | null
  monthly_cost_quota: number | null
  billing_day: number
  notify_percent: number
  enabled: boolean
}

export interface QuotasResp {
  quotas: QuotaRow[]
}

export interface PriceRow {
  id: number
  model: string
  input_price_per_m: number | null
  output_price_per_m: number | null
  currency: string
  effective_from: string
}

export interface PricesResp {
  prices: PriceRow[]
}

// ---------- 访问授权（供应商/模型） ----------

export interface AccessRuleRow {
  id: number
  user_id: number
  provider_id: number | null
  provider_name: string | null
  model_pattern: string | null
}

export interface UserAccessResp {
  configured: boolean
  rules: AccessRuleRow[]
}

export interface AccessRuleInput {
  provider_id: number | null
  model_pattern: string | null
}

export interface TestConnectionResp {
  models: string[]
  count: number
}

// ---------- 模型库 ----------

export interface ModelRow {
  id: number
  provider_id: number
  provider_name: string
  model_id: string
  created_at: string
}

export interface ModelsResp {
  models: ModelRow[]
}

export interface ModelsRefreshResp {
  updated: { provider_id: number; provider_name: string; count: number }[]
  failed: { provider_id: number; provider_name: string; error: string }[]
}

// ---------- 模型测试 ----------

export interface ModelTestResult {
  model_id: number
  model: string
  provider_id: number
  provider_name: string
  ok: boolean
  latency_ms: number | null
  error: string | null
}

export interface ModelTestResp {
  results: ModelTestResult[]
  ok: number
  failed: number
}

// ---------- 系统设置（LDAP） ----------

export interface LdapSettingsResp {
  url: string
  starttls: boolean
  bind_dn: string | null
  base_dn: string
  user_filter: string
  admin_groups: string[]
  has_password: boolean
}

/** extra_body 合并全局开关 */
export interface ExtraBodySettingsResp {
  enabled: boolean
}

export interface LdapSettingsInput {
  url: string
  starttls: boolean
  bind_dn: string | null
  /** 传空串 = 不修改已保存密码 */
  bind_password: string
  base_dn: string
  user_filter: string
  admin_groups: string[]
}

// ---------- 服务状态页（公开，仿 status.openai.com） ----------

export type ComponentStatus = 'operational' | 'degraded' | 'down' | 'unknown'

export interface StatusComponent {
  key: string
  name: string
  kind: 'system' | 'provider'
  status: ComponentStatus
  latency_ms: number | null
  detail: string | null
  /** 30 天可用率（0..1；无数据为 null） */
  uptime_30d: number | null
  calls_30d: number
}

export interface StatusDayPoint {
  date: string
  /** 当日成功率（0..1；无数据为 null） */
  success_rate: number | null
  calls: number
  errors: number
}

export interface StatusUptimeSeries {
  key: string
  name: string
  days: StatusDayPoint[]
}

export interface StatusIncident {
  provider_id: number
  provider_name: string
  start: string
  end: string
  ongoing: boolean
  severity: 'major' | 'minor'
  error_rate: number
  calls: number
  errors: number
  title: string
}

export interface StatusResp {
  generated_at: string
  overall: 'operational' | 'degraded' | 'down'
  components: StatusComponent[]
  uptime: StatusUptimeSeries[]
  incidents: StatusIncident[]
}

/** API 端点管理（管理员） */
export interface ApiEndpointInfo {
  path: string
  address: string
  enabled: boolean
  visible: boolean
}

export interface ApiEndpointsResp {
  public_base: string
  public_base_override: boolean
  responses: ApiEndpointInfo
  messages: ApiEndpointInfo
}

export interface ApiEndpointsReq {
  responses_enabled: boolean
  responses_visible: boolean
  messages_enabled: boolean
  messages_visible: boolean
}

/** 管理员 API 测试结果 */
export interface ApiTestResult {
  id: number
  api: 'responses' | 'messages'
  admin_id: number | null
  model: string
  stream: boolean
  status_code: number
  ok: boolean
  latency_ms: number
  error: string
  body_preview: string
  created_at: string
}

/** 单次测试即时响应（无历史字段） */
export interface ApiTestResp {
  ok: boolean
  api: 'responses' | 'messages'
  model: string
  stream: boolean
  status_code: number
  latency_ms: number
  error: string
  content_type: string
  body_preview: string
}

/** 用户页公开端点（hidden 时为 null） */
export interface PublicEndpointInfo {
  path: string
  address: string
}

export interface PublicEndpointsResp {
  responses: PublicEndpointInfo | null
  messages: PublicEndpointInfo | null
}

// ---------- Coding Plan 与用户分组 ----------

/** Plan 过量策略 */
export type OverageAction = 'block' | 'downgrade' | 'log'
/** 统计周期：自然日 / 自然月 / 总量 */
export type PeriodType = 'daily' | 'monthly' | 'total'

export interface CodingPlan {
  id: number
  name: string
  description: string
  priority: number
  token_limit: number
  token_limit_display: string
  period_type: PeriodType
  overage_action: OverageAction
  downgrade_model: string | null
  alert_channels: string[]
  webhook_url: string
  enabled: boolean
  updated_at: string
}

export interface PlanSummary extends CodingPlan {
  used_tokens: number
  active_users: number
  group_count: number
  member_count: number
}

export interface PlansResp {
  plans: PlanSummary[]
}

export interface DailyUsage {
  stat_date: string
  call_count: number
  tokens: number
}

export interface PeriodUsage {
  period_start: string
  tokens: number
  users: number
}

export interface PlanUserUsage {
  user_id: number
  username: string
  display_name: string | null
  call_count: number
  tokens: number
}

export interface PlanUsageResp {
  plan: CodingPlan
  trend: DailyUsage[]
  periods: PeriodUsage[]
  range: { from: string; to: string }
  users: {
    rows: PlanUserUsage[]
    total: number
    limit: number
    offset: number
  }
}

export interface PlanAlert {
  id: number
  plan_id: number | null
  plan_name: string
  user_id: number | null
  username: string
  level: number
  period_key: string
  used: number
  limit_tokens: number
  message: string
  delivered: Record<string, string>
  created_at: string
}

export interface PlanAlertsResp {
  alerts: PlanAlert[]
}

export interface UserGroup {
  id: number
  name: string
  description: string
  plan_id: number | null
  plan_name: string | null
  plan_enabled: boolean | null
  ldap_sync: boolean
  member_count: number
  last_sync_at: string | null
  last_sync_result: string
  created_at: string
  updated_at: string
}

export interface GroupsResp {
  groups: UserGroup[]
}

export interface GroupMember {
  user_id: number
  username: string
  display_name: string | null
  email: string | null
  source: 'manual' | 'ldap' | 'all'
  status: number
  added_at: string
}

export interface MembersResp {
  members: GroupMember[]
  total: number
  page: number
  page_size: number
}

export interface UserPick {
  id: number
  username: string
  display_name: string | null
  email: string | null
  source: string
  status: number
  is_member: boolean
}

export interface UserPickResp {
  users: UserPick[]
  total: number
  page: number
  page_size: number
}

export interface MyPlanResp {
  plan: {
    id: number
    name: string
    group: string
    period_type: PeriodType
    token_limit: number
    token_limit_display: string
    overage_action: OverageAction
    downgrade_model: string | null
  } | null
  period: {
    key: string
    used: number
    remaining: number
    percent: number
  } | null
  daily: DailyUsage[]
}

export interface MyNotificationsResp {
  notifications: PlanAlert[]
}

export interface SmtpSettings {
  host: string
  port: number
  username: string
  from: string
  has_password: boolean
}

export interface SmtpSettingsResp {
  host: string
  port: number
  username: string
  from: string
  has_password: boolean
}
