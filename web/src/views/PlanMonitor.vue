<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import * as echarts from 'echarts'
import { request } from '@/api/client'
import type { PlanAlertsResp, PlanUsageResp, PlansResp } from '@/api/types'

const plans = ref<PlansResp['plans']>([])
const selectedPlanId = ref<number | null>(null)
const detail = ref<PlanUsageResp | null>(null)
const detailLoading = ref(false)

// 成员用量视图：默认「当前周期」（自上次重置，与配额/告警口径一致）；
// 30 天累计会远超单周期配额，默认展示只会全是告警，故回溯降级为切换项
const usageMode = ref<'current' | 'range'>('current')

const PERIOD_LABELS: Record<string, string> = {
  hourly: '小时级重置',
  daily: '自然日重置',
  monthly: '自然月重置',
  total: '总量不重置',
}
function periodLabel(pt: string, hours?: number): string {
  return pt === 'hourly' ? `每 ${hours ?? 1} 小时重置` : PERIOD_LABELS[pt] ?? pt
}

function percent(used: number, limit: number): number {
  if (limit <= 0) return 0
  return Math.round((used * 10000) / limit) / 100
}

function usageStatus(p: number): string {
  if (p >= 100) return 'exception'
  if (p >= 80) return 'warning'
  return 'success'
}

async function loadPlans() {
  try {
    const resp = await request<PlansResp>('/api/admin/plans')
    plans.value = resp.plans
    if (resp.plans.length && selectedPlanId.value == null) {
      selectedPlanId.value = resp.plans[0].id
    }
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载 Plan 列表失败')
  }
}

async function loadDetail() {
  if (selectedPlanId.value == null) {
    detail.value = null
    return
  }
  detailLoading.value = true
  try {
    detail.value = await request<PlanUsageResp>(`/api/admin/plans/${selectedPlanId.value}/usage?days=30`)
    await nextTick()
    renderTrend()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载用量失败')
  } finally {
    detailLoading.value = false
  }
}

watch(selectedPlanId, () => {
  currentPage.page = 1
  userPage.page = 1
  loadDetail()
})

// —— 趋势图（echarts，同 Usage.vue 模式）——
const trendChartRef = ref<HTMLDivElement>()
let trendChart: echarts.ECharts | null = null

function renderTrend() {
  if (!detail.value || !trendChartRef.value) return
  if (!trendChart) {
    trendChart = echarts.init(trendChartRef.value)
  }
  const trend = detail.value.trend
  trendChart.setOption({
    grid: { left: 48, right: 16, top: 32, bottom: 28 },
    tooltip: { trigger: 'axis' },
    xAxis: { type: 'category', data: trend.map((d) => d.stat_date) },
    yAxis: { type: 'value' },
    series: [
      {
        name: 'tokens',
        type: 'line',
        smooth: true,
        areaStyle: { opacity: 0.12 },
        data: trend.map((d) => d.tokens),
      },
    ],
  })
}

function onResize() {
  trendChart?.resize()
}

// —— 告警列表 ——
const alerts = ref<PlanAlertsResp['alerts']>([])
const alertsLoading = ref(false)
const alertQuery = reactive({ level: undefined as number | undefined, limit: 100 })

async function loadAlerts() {
  alertsLoading.value = true
  try {
    const params = new URLSearchParams()
    if (alertQuery.level != null) params.set('level', String(alertQuery.level))
    params.set('limit', String(alertQuery.limit))
    const resp = await request<PlanAlertsResp>(`/api/admin/plan-alerts?${params}`)
    alerts.value = resp.alerts
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载告警失败')
  } finally {
    alertsLoading.value = false
  }
}

function fmtAlertTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '-'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

function levelTagType(level: number): string {
  if (level >= 100) return 'danger'
  if (level >= 95) return 'danger'
  if (level >= 80) return 'warning'
  return 'info'
}

// —— 成员当期用量（自上次重置；plan_usage_counters 与配额/告警同源）——
const currentPage = reactive({ page: 1, page_size: 20 })

const currentUsed = computed(
  () => detail.value?.current?.used_tokens ?? detail.value?.periods[0]?.tokens ?? 0,
)

const currentPeriodText = computed(() => {
  if (!detail.value) return ''
  const ps = detail.value.current?.period_start
  if (!ps) {
    return detail.value.plan.period_type === 'total' ? '累计总量（不重置）' : ''
  }
  return `当前周期起点：${fmtAlertTime(ps)}（${periodLabel(detail.value.plan.period_type, detail.value.plan.period_hours)}）`
})

function shareOf(row: { tokens: number }): number {
  if (currentUsed.value <= 0) return 0
  return Math.round(((row.tokens * 100) / currentUsed.value) * 10) / 10
}

async function loadCurrentPage(page = 1) {
  if (selectedPlanId.value == null || !detail.value) return
  currentPage.page = page
  detailLoading.value = true
  try {
    const params = new URLSearchParams({
      limit: String(currentPage.page_size),
      offset: String((page - 1) * currentPage.page_size),
    })
    const resp = await request<PlanUsageResp>(
      `/api/admin/plans/${selectedPlanId.value}/usage?${params}`,
    )
    // 仅替换当期用量段，保留趋势与周期
    if (detail.value) detail.value.current = resp.current
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载当期用量失败')
  } finally {
    detailLoading.value = false
  }
}

// —— 用户维度回溯 ——
const userPage = reactive({ page: 1, page_size: 20 })
const rangeForm = reactive({
  from: '',
  to: '',
})

async function loadUserPage(page = 1) {
  if (selectedPlanId.value == null || !detail.value) return
  userPage.page = page
  const params = new URLSearchParams({
    limit: String(userPage.page_size),
    offset: String((page - 1) * userPage.page_size),
  })
  if (rangeForm.from) params.set('from', rangeForm.from)
  if (rangeForm.to) params.set('to', rangeForm.to)
  detailLoading.value = true
  try {
    const resp = await request<PlanUsageResp>(
      `/api/admin/plans/${selectedPlanId.value}/usage?${params}`,
    )
    // 仅替换 users 段，保留趋势与周期
    if (detail.value) detail.value.users = resp.users
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载用户用量失败')
  } finally {
    detailLoading.value = false
  }
}

function onPageChange(page: number) {
  if (usageMode.value === 'current') loadCurrentPage(page)
  else loadUserPage(page)
}

onMounted(async () => {
  await loadPlans()
  await loadDetail()
  await loadAlerts()
  window.addEventListener('resize', onResize)
})

onBeforeUnmount(() => {
  window.removeEventListener('resize', onResize)
  trendChart?.dispose()
})
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <div class="title">Plan 用量监控看板</div>
      <el-select
        v-model="selectedPlanId"
        placeholder="选择 Plan"
        style="width: 260px"
      >
        <el-option
          v-for="p in plans"
          :key="p.id"
          :value="p.id"
          :label="`${p.name}${p.enabled ? '' : '（已停用）'}`"
        />
      </el-select>
    </div>

    <el-row v-if="detail" :gutter="16">
      <el-col :span="8">
        <el-card shadow="never">
          <div class="stat-label">总配额</div>
          <div class="stat-value">{{ detail.plan.token_limit_display }}</div>
          <div class="stat-sub">{{ periodLabel(detail.plan.period_type, detail.plan.period_hours) }}</div>
        </el-card>
      </el-col>
      <el-col :span="8">
        <el-card shadow="never">
          <div class="stat-label">已用 / 剩余（自上次重置）</div>
          <div class="stat-value">
            {{ (detail.plan.token_limit - currentUsed).toLocaleString() }}
          </div>
          <div class="stat-sub">
            当期成员合计 {{ currentUsed.toLocaleString() }} tokens
          </div>
        </el-card>
      </el-col>
      <el-col :span="8">
        <el-card shadow="never">
          <div class="stat-label">绑定分组 / 成员</div>
          <div class="stat-value">
            {{ plans.find((p) => p.id === detail!.plan.id)?.group_count ?? '-' }} /
            {{ plans.find((p) => p.id === detail!.plan.id)?.member_count ?? '-' }}
          </div>
          <div class="stat-sub">
            活跃用户 {{ detail.periods[0]?.users ?? 0 }}
          </div>
        </el-card>
      </el-col>
    </el-row>

    <el-card v-if="detail" shadow="never">
      <div class="stat-label">近 30 日使用趋势（成员合计 tokens）</div>
      <div ref="trendChartRef" class="chart" />
    </el-card>

    <el-card v-if="detail" shadow="never">
      <div class="section-head">
        <div>
          <div class="stat-label">
            {{ usageMode === 'current' ? '成员当期用量（自上次重置，与配额/告警同口径）' : '成员用量回溯（按日期区间）' }}
          </div>
          <div v-if="usageMode === 'current' && currentPeriodText" class="stat-sub">
            {{ currentPeriodText }}
          </div>
        </div>
        <div class="range-row">
          <el-radio-group v-model="usageMode" size="small">
            <el-radio-button value="current">当前周期</el-radio-button>
            <el-radio-button value="range">历史回溯</el-radio-button>
          </el-radio-group>
          <template v-if="usageMode === 'range'">
            <el-date-picker
              v-model="rangeForm.from"
              type="date"
              value-format="YYYY-MM-DD"
              placeholder="开始日期"
              style="width: 140px"
            />
            <el-date-picker
              v-model="rangeForm.to"
              type="date"
              value-format="YYYY-MM-DD"
              placeholder="结束日期"
              style="width: 140px"
            />
            <el-button type="primary" @click="loadUserPage(1)">查询</el-button>
          </template>
        </div>
      </div>

      <el-table
        v-if="usageMode === 'current'"
        v-loading="detailLoading"
        :data="detail.current?.users ?? []"
        stripe
        size="small"
      >
        <el-table-column label="用户" min-width="140">
          <template #default="{ row }">
            {{ row.username }}
            <span v-if="row.display_name" class="muted">（{{ row.display_name }}）</span>
          </template>
        </el-table-column>
        <el-table-column label="tokens" min-width="120">
          <template #default="{ row }">{{ row.tokens.toLocaleString() }}</template>
        </el-table-column>
        <el-table-column label="占当期合计" min-width="200">
          <template #default="{ row }">
            <div class="share-row">
              <el-progress
                :percentage="shareOf(row)"
                :stroke-width="6"
                :show-text="false"
                class="share-bar"
              />
              <span class="share-text">{{ shareOf(row).toFixed(1) }}%</span>
            </div>
          </template>
        </el-table-column>
      </el-table>

      <el-table
        v-else
        v-loading="detailLoading"
        :data="detail.users.rows"
        stripe
        size="small"
      >
        <el-table-column label="用户" min-width="140">
          <template #default="{ row }">
            {{ row.username }}
            <span v-if="row.display_name" class="muted">（{{ row.display_name }}）</span>
          </template>
        </el-table-column>
        <el-table-column label="调用次数" prop="call_count" width="110" />
        <el-table-column label="tokens" prop="tokens" min-width="140" />
      </el-table>
      <el-pagination
        class="pager"
        layout="total, prev, pager, next"
        :total="usageMode === 'current' ? (detail.current?.total ?? 0) : detail.users.total"
        :page-size="userPage.page_size"
        :current-page="usageMode === 'current' ? currentPage.page : userPage.page"
        @current-change="onPageChange"
      />
    </el-card>

    <el-card shadow="never">
      <div class="section-head">
        <div class="stat-label">阈值告警（80% / 95% / 100% 与系统事件）</div>
        <div class="range-row">
          <el-select v-model="alertQuery.level" placeholder="全部级别" clearable style="width: 140px">
            <el-option label="80%" :value="80" />
            <el-option label="95%" :value="95" />
            <el-option label="100%" :value="100" />
            <el-option label="系统事件" :value="0" />
          </el-select>
          <el-button @click="loadAlerts">刷新</el-button>
        </div>
      </div>
      <el-table v-loading="alertsLoading" :data="alerts" stripe size="small">
        <el-table-column label="级别" width="100">
          <template #default="{ row }">
            <el-tag size="small" :type="levelTagType(row.level)" effect="plain">
              {{ row.level === 0 ? '系统' : `${row.level}%` }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="Plan" prop="plan_name" min-width="130" />
        <el-table-column label="用户" prop="username" min-width="110" />
        <el-table-column label="用量" min-width="170">
          <template #default="{ row }">
            {{ row.used.toLocaleString() }} / {{ row.limit_tokens.toLocaleString() }}
          </template>
        </el-table-column>
        <el-table-column label="消息" prop="message" min-width="260" show-overflow-tooltip />
        <el-table-column label="渠道回执" min-width="170">
          <template #default="{ row }">
            <span v-if="!Object.keys(row.delivered || {}).length">-</span>
            <el-tag
              v-for="(v, k) in row.delivered"
              :key="k"
              size="small"
              :type="String(v).startsWith('ok') ? 'success' : 'danger'"
              effect="plain"
              class="channel-tag"
            >
              {{ k }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="时间" width="140">
          <template #default="{ row }">{{ fmtAlertTime(row.created_at) }}</template>
        </el-table-column>
      </el-table>
    </el-card>
  </div>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.title {
  font-size: 16px;
  font-weight: 600;
  color: #303133;
}
.stat-label {
  font-size: 12px;
  color: #909399;
  margin-bottom: 6px;
}
.stat-value {
  font-size: 20px;
  font-weight: 600;
  color: #303133;
}
.stat-sub {
  font-size: 12px;
  color: #909399;
  margin-top: 4px;
}
.chart {
  height: 260px;
}
.section-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 10px;
}
.range-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.pager {
  margin-top: 12px;
  justify-content: flex-end;
}
.muted {
  color: #909399;
  font-size: 12px;
}
.share-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.share-bar {
  flex: 1;
}
.share-text {
  font-size: 12px;
  color: #606266;
  min-width: 48px;
  text-align: right;
}
.channel-tag {
  margin-right: 4px;
}
</style>
