<template>
  <div class="usage-page" v-loading="loading">
    <div class="toolbar">
      <span v-if="viewMode === 'summary'" class="month-label">
        统计月份：<b class="month">{{ usage?.month ?? '—' }}</b>
      </span>
      <span class="toolbar-right">
        <el-radio-group v-if="isAdmin" v-model="viewMode" size="small">
          <el-radio-button value="summary">汇总</el-radio-button>
          <el-radio-button value="users">按用户</el-radio-button>
          <el-radio-button value="keys">按 API Key</el-radio-button>
          <el-radio-button value="ips">按来源 IP</el-radio-button>
        </el-radio-group>
        <el-radio-group
          v-if="isAdmin && viewMode !== 'summary'"
          v-model="breakdownRange"
          size="small"
          @change="loadBreakdown"
        >
          <el-radio-button value="30m">30 分钟</el-radio-button>
          <el-radio-button value="1d">1 天</el-radio-button>
          <el-radio-button value="7d">7 天</el-radio-button>
          <el-radio-button value="30d">30 天</el-radio-button>
          <el-radio-button value="90d">90 天</el-radio-button>
        </el-radio-group>
        <el-button :icon="Refresh" :loading="loading" @click="refresh">刷新</el-button>
      </span>
    </div>

    <el-empty v-if="!loading && isEmpty" description="暂无用量数据" />

    <template v-else-if="isAdmin && viewMode === 'users'">
      <el-card shadow="never" class="panel">
        <template #header><span>{{ rangeLabel }}按用户（全站）</span></template>
        <el-table :data="adminUsage?.by_user ?? []" stripe>
          <el-table-column label="用户" min-width="160">
            <template #default="{ row }">
              <span>{{ (row as UserUsageRow).username ?? '—' }}</span>
              <span v-if="(row as UserUsageRow).display_name" class="sub">（{{ (row as UserUsageRow).display_name }}）</span>
            </template>
          </el-table-column>
          <el-table-column prop="call_count" label="调用次数" align="right" min-width="100" :formatter="fmtCell" />
          <el-table-column prop="input_tokens" label="输入 Token" align="right" min-width="110" :formatter="fmtCell" />
          <el-table-column prop="output_tokens" label="输出 Token" align="right" min-width="110" :formatter="fmtCell" />
          <el-table-column prop="cost" label="成本(¥)" align="right" min-width="100" :formatter="fmtCostCell" />
        </el-table>
      </el-card>
    </template>

    <template v-else-if="isAdmin && viewMode === 'keys'">
      <el-card shadow="never" class="panel">
        <template #header><span>{{ rangeLabel }}按 API Key（全站）</span></template>
        <el-table :data="adminUsage?.by_key ?? []" stripe>
          <el-table-column label="Key" min-width="200">
            <template #default="{ row }">
              <span class="key-name">{{ (row as KeyUsageRow).name }}</span>
              <el-tag size="small" type="info" class="key-tag">{{ (row as KeyUsageRow).key_prefix }}…</el-tag>
            </template>
          </el-table-column>
          <el-table-column label="所属用户" min-width="140">
            <template #default="{ row }">{{ (row as KeyUsageRow).username ?? '—' }}</template>
          </el-table-column>
          <el-table-column prop="call_count" label="调用次数" align="right" min-width="100" :formatter="fmtCell" />
          <el-table-column prop="input_tokens" label="输入 Token" align="right" min-width="110" :formatter="fmtCell" />
          <el-table-column prop="output_tokens" label="输出 Token" align="right" min-width="110" :formatter="fmtCell" />
          <el-table-column prop="cost" label="成本(¥)" align="right" min-width="100" :formatter="fmtCostCell" />
        </el-table>
      </el-card>
    </template>

    <template v-else-if="isAdmin && viewMode === 'ips'">
      <el-card shadow="never" class="panel">
        <template #header><span>{{ rangeLabel }}按来源 IP（全站；X-Forwarded-For 优先，缺省取对端地址）</span></template>
        <el-table :data="adminUsage?.by_ip ?? []" stripe>
          <el-table-column label="来源 IP" min-width="150">
            <template #default="{ row }">
              <span class="mono">{{ (row as IpUsageRow).client_ip }}</span>
            </template>
          </el-table-column>
          <el-table-column prop="call_count" label="调用次数" align="right" min-width="100" :formatter="fmtCell" />
          <el-table-column prop="input_tokens" label="输入 Token" align="right" min-width="110" :formatter="fmtCell" />
          <el-table-column prop="output_tokens" label="输出 Token" align="right" min-width="110" :formatter="fmtCell" />
          <el-table-column prop="cost" label="成本(¥)" align="right" min-width="100" :formatter="fmtCostCell" />
          <el-table-column label="最后调用" min-width="140">
            <template #default="{ row }">{{ fmtDateTime((row as IpUsageRow).last_seen) }}</template>
          </el-table-column>
        </el-table>
      </el-card>
    </template>

    <template v-else>
      <el-card shadow="never" class="panel">
        <template #header>
          <div class="card-header">
            <span>调用次数趋势</span>
            <span class="card-tools">
              <el-select
                v-if="isAdmin"
                v-model="trendUser"
                size="small"
                class="user-select"
                @change="renderAllCharts"
              >
                <el-option label="全部用户" :value="0" />
                <el-option
                  v-for="u in trend?.by_user ?? []"
                  :key="u.user_id"
                  :label="u.display_name ? `${u.username}（${u.display_name}）` : u.username"
                  :value="u.user_id"
                />
              </el-select>
              <el-radio-group v-model="trendDays" size="small" @change="loadData">
                <el-radio-button :value="0">今天（30 分钟）</el-radio-button>
                <el-radio-button :value="7">7 天</el-radio-button>
                <el-radio-button :value="30">30 天</el-radio-button>
                <el-radio-button :value="90">90 天</el-radio-button>
              </el-radio-group>
            </span>
          </div>
        </template>
        <div ref="callsChartRef" class="chart"></div>
      </el-card>

      <el-card shadow="never" class="panel">
        <template #header><span>输入 / 输出 Token 趋势</span></template>
        <div ref="tokenChartRef" class="chart"></div>
      </el-card>

      <el-card shadow="never" class="panel">
        <template #header><span>调用成本趋势</span></template>
        <div ref="costChartRef" class="chart"></div>
      </el-card>

      <el-card v-if="isAdmin" shadow="never" class="panel">
        <template #header>
          <div class="card-header">
            <span>用户请求对比</span>
            <span class="card-tools">
              <el-select
                v-model="compareModel"
                size="small"
                clearable
                placeholder="全部模型"
                class="model-select"
                @change="loadCompare"
              >
                <el-option
                  v-for="m in compareTrend?.models ?? []"
                  :key="m"
                  :label="m"
                  :value="m"
                />
              </el-select>
              <el-radio-group v-model="rankMode" size="small" @change="renderCompareChart">
                <el-radio-button value="top">请求最多 Top 10</el-radio-button>
                <el-radio-button value="bottom">请求最少 Bottom 10</el-radio-button>
                <el-radio-button value="all">全部用户</el-radio-button>
              </el-radio-group>
            </span>
          </div>
        </template>
        <div ref="compareChartRef" class="chart"></div>
      </el-card>

      <el-card shadow="never" class="panel">
        <template #header><span>近 7 天明细</span></template>
        <el-table :data="usage?.last_7_days ?? []" stripe>
          <el-table-column prop="stat_date" label="日期" min-width="110" />
          <el-table-column prop="call_count" label="调用次数" align="right" min-width="100" :formatter="fmtCell" />
          <el-table-column prop="input_tokens" label="输入 Token" align="right" min-width="110" :formatter="fmtCell" />
          <el-table-column prop="output_tokens" label="输出 Token" align="right" min-width="110" :formatter="fmtCell" />
          <el-table-column prop="cost" label="成本(¥)" align="right" min-width="100" :formatter="fmtCostCell" />
        </el-table>
      </el-card>

      <el-card shadow="never" class="panel">
        <template #header><span>本月按模型</span></template>
        <el-table :data="usage?.by_model ?? []" stripe>
          <el-table-column prop="model" label="模型" min-width="200">
            <template #default="{ row }">
              <span class="model-name">{{ (row as ModelStat).model }}</span>
            </template>
          </el-table-column>
          <el-table-column prop="call_count" label="调用次数" align="right" min-width="100" :formatter="fmtCell" />
          <el-table-column prop="input_tokens" label="输入 Token" align="right" min-width="110" :formatter="fmtCell" />
          <el-table-column prop="output_tokens" label="输出 Token" align="right" min-width="110" :formatter="fmtCell" />
          <el-table-column prop="cost" label="成本(¥)" align="right" min-width="100" :formatter="fmtCostCell" />
        </el-table>
      </el-card>
    </template>
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { Refresh } from '@element-plus/icons-vue'
import * as echarts from 'echarts'
import { request } from '@/api/client'
import { useAuthStore } from '@/stores/auth'
import type {
  AdminUsageResp,
  IpUsageRow,
  KeyUsageRow,
  ModelStat,
  TrendPoint,
  TrendResp,
  UsageResp,
  UserUsageRow,
} from '@/api/types'

const auth = useAuthStore()
const isAdmin = computed(() => auth.isAdmin)
const viewMode = ref<'summary' | 'users' | 'keys' | 'ips'>('summary')

const usage = ref<UsageResp | null>(null)
const adminUsage = ref<AdminUsageResp | null>(null)
const loading = ref(false)

/** 分组视图（按用户 / API Key / 来源 IP）时间范围，对应后端 ?range= */
type BreakdownRange = '30m' | '1d' | '7d' | '30d' | '90d'
const breakdownRange = ref<BreakdownRange>('30d')
const rangeLabel = computed(
  () =>
    ({ '30m': '近 30 分钟', '1d': '近 1 天', '7d': '近 7 天', '30d': '近 30 天', '90d': '近 90 天' })[
      breakdownRange.value
    ],
)

/** 趋势：时间范围（天，0 = 今天每 30 分钟）与用户筛选（0 = 全部用户） */
const trendDays = ref(30)
const trendUser = ref(0)

/** 当前是否半小时粒度（今天视图） */
const isHalfHour = computed(() => trendDays.value === 0)
const trend = ref<TrendResp | null>(null)

const callsChartRef = ref<HTMLDivElement | null>(null)
const tokenChartRef = ref<HTMLDivElement | null>(null)
const costChartRef = ref<HTMLDivElement | null>(null)
const compareChartRef = ref<HTMLDivElement | null>(null)
let callsChart: echarts.ECharts | null = null
let tokenChart: echarts.ECharts | null = null
let costChart: echarts.ECharts | null = null
let compareChart: echarts.ECharts | null = null

/** 用户请求对比：按请求数总量 Top/Bottom 10 或全部 */
const rankMode = ref<'top' | 'bottom' | 'all'>('top')

/** 用户请求对比：独立趋势数据与模型过滤（'' = 全部模型） */
const compareTrend = ref<TrendResp | null>(null)
const compareModel = ref('')

const isEmpty = computed(() => {
  const data = isAdmin.value ? adminUsage.value : usage.value
  if (!data) return false
  if (isAdmin.value && viewMode.value !== 'summary') {
    const a = adminUsage.value
    return !a || (a.by_user.length === 0 && a.by_key.length === 0 && a.by_ip.length === 0)
  }
  return data.last_7_days.length === 0 && data.by_model.length === 0
})

function fmt(n: number): string {
  return n.toLocaleString()
}

function fmtCost(n: number): string {
  return '¥' + n.toFixed(2)
}

/** 本地时间 YYYY-MM-DD HH:mm */
function fmtDateTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '-'
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

/** 数字单元格：千分位（null 兜底 0） */
function fmtCell(_row: unknown, _col: unknown, cellValue: unknown): string {
  return fmt(Number(cellValue ?? 0))
}

/** 成本单元格：保留两位小数（null 兜底 0） */
function fmtCostCell(_row: unknown, _col: unknown, cellValue: unknown): string {
  return fmtCost(Number(cellValue ?? 0))
}

function initCharts(): void {
  if (callsChartRef.value && !callsChart) callsChart = echarts.init(callsChartRef.value)
  if (tokenChartRef.value && !tokenChart) tokenChart = echarts.init(tokenChartRef.value)
  if (costChartRef.value && !costChart) costChart = echarts.init(costChartRef.value)
  if (compareChartRef.value && !compareChart) compareChart = echarts.init(compareChartRef.value)
  renderAllCharts()
}

function disposeCharts(): void {
  callsChart?.dispose()
  tokenChart?.dispose()
  costChart?.dispose()
  compareChart?.dispose()
  callsChart = null
  tokenChart = null
  costChart = null
  compareChart = null
}

/** 当前筛选下的趋势序列（0 = 全部用户） */
function currentSeries(): TrendPoint[] {
  if (!trend.value) return []
  if (trendUser.value === 0) return trend.value.daily
  return trend.value.by_user.find((u) => u.user_id === trendUser.value)?.daily ?? []
}

/** 图表 x 轴标签：按天显示 MM-DD，半小时粒度显示本地 MM-DD HH:mm */
function xLabel(d: TrendPoint, gran?: TrendResp['granularity']): string {
  if ((gran ?? trend.value?.granularity) === 'half_hour') {
    const t = new Date(d.stat_date)
    const p = (n: number) => String(n).padStart(2, '0')
    return `${p(t.getMonth() + 1)}-${p(t.getDate())} ${p(t.getHours())}:${p(t.getMinutes())}`
  }
  return d.stat_date.slice(5)
}

function renderCallsChart(): void {
  const c = callsChart
  if (!c) return
  const pts = currentSeries()
  c.setOption(
    {
      tooltip: {
        trigger: 'axis',
        valueFormatter: (v: unknown) => fmt(Number(v)) + ' 次',
      },
      grid: { left: 8, right: 16, top: 32, bottom: 8, containLabel: true },
      xAxis: {
        type: 'category',
        data: pts.map((d) => xLabel(d)),
        boundaryGap: false,
        axisLabel: { hideOverlap: true },
      },
      yAxis: { type: 'value', minInterval: 1 },
      series: [
        {
          name: '调用次数',
          type: 'line',
          smooth: true,
          symbol: 'circle',
          symbolSize: 6,
          data: pts.map((d) => d.call_count),
          itemStyle: { color: '#409eff' },
          areaStyle: { opacity: 0.08 },
        },
      ],
    },
    true,
  )
}

function renderTokenChart(): void {
  const c = tokenChart
  if (!c) return
  const pts = currentSeries()
  c.setOption(
    {
      tooltip: {
        trigger: 'axis',
        valueFormatter: (v: unknown) => fmt(Number(v)),
      },
      legend: { top: 0 },
      grid: { left: 8, right: 16, top: 32, bottom: 8, containLabel: true },
      xAxis: {
        type: 'category',
        data: pts.map((d) => xLabel(d)),
        boundaryGap: false,
        axisLabel: { hideOverlap: true },
      },
      yAxis: { type: 'value' },
      series: [
        {
          name: '输入 Token',
          type: 'line',
          smooth: true,
          symbol: 'circle',
          symbolSize: 6,
          data: pts.map((d) => d.input_tokens),
          itemStyle: { color: '#67c23a' },
        },
        {
          name: '输出 Token',
          type: 'line',
          smooth: true,
          symbol: 'circle',
          symbolSize: 6,
          data: pts.map((d) => d.output_tokens),
          itemStyle: { color: '#e6a23c' },
        },
      ],
    },
    true,
  )
}

function renderCostChart(): void {
  const c = costChart
  if (!c) return
  const pts = currentSeries()
  c.setOption(
    {
      tooltip: {
        trigger: 'axis',
        valueFormatter: (v: unknown) => '¥' + Number(v).toFixed(2),
      },
      grid: { left: 8, right: 16, top: 32, bottom: 8, containLabel: true },
      xAxis: {
        type: 'category',
        data: pts.map((d) => xLabel(d)),
        axisLabel: { hideOverlap: true },
      },
      yAxis: { type: 'value', axisLabel: { formatter: (v: number) => '¥' + v } },
      series: [
        {
          name: '调用成本',
          type: 'bar',
          data: pts.map((d) => d.cost),
          barMaxWidth: 40,
          itemStyle: { color: '#409eff', borderRadius: [4, 4, 0, 0] },
        },
      ],
    },
    true,
  )
}

function renderAllCharts(): void {
  renderCallsChart()
  renderTokenChart()
  renderCostChart()
  renderCompareChart()
}

/** 用户请求对比系列：按请求数总量排序过滤（Top/Bottom 10 或全部） */
function compareSeries(): { name: string; data: number[] }[] {
  if (!compareTrend.value) return []
  const rows = (compareTrend.value.by_user ?? []).map((u) => ({
    name: u.display_name ? `${u.username}（${u.display_name}）` : u.username,
    data: u.daily.map((d) => d.call_count),
    total: u.daily.reduce((s, d) => s + d.call_count, 0),
  }))
  if (rankMode.value === 'top') {
    return [...rows].sort((a, b) => b.total - a.total).slice(0, 10)
  }
  if (rankMode.value === 'bottom') {
    return [...rows].sort((a, b) => a.total - b.total).slice(0, 10)
  }
  return rows
}

function renderCompareChart(): void {
  const c = compareChart
  if (!c) return
  const series = compareSeries()
  const dates = (compareTrend.value?.daily ?? []).map((d) => xLabel(d, compareTrend.value?.granularity))
  c.setOption(
    {
      tooltip: { trigger: 'axis' },
      legend: { type: 'scroll', top: 0 },
      grid: { left: 8, right: 16, top: 36, bottom: 8, containLabel: true },
      xAxis: {
        type: 'category',
        data: dates,
        boundaryGap: false,
        axisLabel: { hideOverlap: true },
      },
      yAxis: { type: 'value', minInterval: 1 },
      series: series.map((s) => ({
        name: s.name,
        type: 'line',
        smooth: true,
        symbol: 'circle',
        symbolSize: 5,
        data: s.data,
      })),
    },
    true,
  )
}

function handleResize(): void {
  callsChart?.resize()
  tokenChart?.resize()
  costChart?.resize()
  compareChart?.resize()
}

/** 用户请求对比数据：独立按模型过滤请求（与趋势图共享粒度/天数） */
async function loadCompare(): Promise<void> {
  const base = isAdmin.value ? '/api/admin/usage' : '/api/usage'
  const gran = isHalfHour.value ? 'half_hour' : 'day'
  const days = isHalfHour.value ? 1 : trendDays.value
  const q = new URLSearchParams({ granularity: gran, days: String(days) })
  if (compareModel.value) q.set('model', compareModel.value)
  try {
    compareTrend.value = await request<TrendResp>(`${base}/trend?${q.toString()}`)
    renderCompareChart()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载用户对比失败')
  }
}

async function loadData(): Promise<void> {
  loading.value = true
  try {
    const base = isAdmin.value ? '/api/admin/usage' : '/api/usage'
    const gran = isHalfHour.value ? 'half_hour' : 'day'
    const days = isHalfHour.value ? 1 : trendDays.value
    const [data, t] = await Promise.all([
      request<AdminUsageResp | UsageResp>(base),
      request<TrendResp>(`${base}/trend?granularity=${gran}&days=${days}`),
    ])
    if (isAdmin.value) {
      adminUsage.value = data as AdminUsageResp
      usage.value = data as AdminUsageResp
    } else {
      usage.value = data as UsageResp
    }
    trend.value = t
    await nextTick()
    initCharts()
    void loadCompare()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载用量数据失败')
  } finally {
    loading.value = false
  }
}

/** 分组视图数据：按当前时间范围拉取 by_user/by_key/by_ip（?range=） */
async function loadBreakdown(): Promise<void> {
  if (!isAdmin.value) return
  loading.value = true
  try {
    const data = await request<AdminUsageResp>(`/api/admin/usage?range=${breakdownRange.value}`)
    adminUsage.value = data
    // by_model/last_7_days 后端恒为本月口径，同步回填不改变汇总视图语义
    usage.value = data
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载用量数据失败')
  } finally {
    loading.value = false
  }
}

/** 刷新当前视图：分组视图按范围拉取，其余走完整加载（含趋势） */
function refresh(): void {
  if (isAdmin.value && viewMode.value !== 'summary') void loadBreakdown()
  else void loadData()
}

onMounted(() => {
  window.addEventListener('resize', handleResize)
  void loadData()
})

watch(viewMode, (v) => {
  if (v === 'summary') {
    // 表格分支销毁了图表 DOM，切回时重建实例
    void nextTick(() => {
      disposeCharts()
      initCharts()
    })
  } else if (isAdmin.value) {
    // 切入分组视图：确保数据与当前所选时间范围一致
    void loadBreakdown()
  }
})

onBeforeUnmount(() => {
  window.removeEventListener('resize', handleResize)
  disposeCharts()
})
</script>

<style scoped>
.usage-page {
  padding: 20px;
}

.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 16px;
}

.month-label {
  color: #606266;
  font-size: 14px;
}

.month {
  color: #303133;
  font-size: 16px;
}

.toolbar-right {
  display: flex;
  align-items: center;
  gap: 12px;
}

.sub {
  color: #909399;
  font-size: 12px;
}

.key-name {
  margin-right: 8px;
}

.key-tag {
  font-family: var(--el-font-family-mono);
}

.panel {
  margin-bottom: 16px;
}

.chart {
  width: 100%;
  height: 300px;
}

.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.card-tools {
  display: flex;
  align-items: center;
  gap: 12px;
}

.user-select {
  width: 160px;
}

.model-select {
  width: 160px;
}

.model-name {
  font-family: var(--el-font-family-mono);
}
</style>
