<template>
  <div class="realtime-page">
    <div class="toolbar">
      <div class="toolbar-left">
        <span class="live-dot" :class="{ paused: !autoRefresh }"></span>
        <span class="live-text">{{ autoRefresh ? '实时监控中' : '已暂停' }}</span>
        <span v-if="lastUpdated" class="updated">更新于 {{ lastUpdatedText }}</span>
      </div>
      <div class="toolbar-right">
        <span class="label">刷新间隔</span>
        <el-select v-model="intervalSec" size="small" style="width: 100px" :disabled="!autoRefresh">
          <el-option :value="2" label="2 秒" />
          <el-option :value="5" label="5 秒" />
          <el-option :value="10" label="10 秒" />
        </el-select>
        <el-switch v-model="autoRefresh" active-text="自动刷新" inline-prompt />
        <el-button size="small" :icon="Refresh" :loading="loading" @click="load()">刷新</el-button>
      </div>
    </div>

    <el-row :gutter="16" class="cards" v-loading="loading && !data">
      <el-col :span="3" v-for="card in cards" :key="card.label">
        <el-card shadow="never" class="stat-card">
          <div class="stat-title" :title="card.hint">{{ card.label }}</div>
          <div class="stat-value" :class="{ danger: card.danger }">{{ card.value }}</div>
        </el-card>
      </el-col>
    </el-row>

    <el-card shadow="never" class="panel">
      <template #header>
        <div class="card-header">
          <span>用户调用情况（近 60 分钟）</span>
          <span class="sub">按 5 分钟调用量排序，红色为错误数；并发列悬停查看模型分布</span>
        </div>
      </template>
      <el-table v-if="data && users.length" :data="users" size="small" :row-class-name="userRowClass">
        <el-table-column label="用户" min-width="170">
          <template #default="{ row }">
            <span class="username">{{ row.username ?? '匿名' }}</span>
            <span v-if="row.display_name" class="display-name">（{{ row.display_name }}）</span>
          </template>
        </el-table-column>
        <el-table-column label="并发" align="right" width="70">
          <template #default="{ row }">
            <el-tooltip v-if="activeOf(row) > 0" placement="top">
              <template #content>
                <div v-for="m in activeModelsOf(row)" :key="m.model" class="active-tip-row">
                  <span class="mono">{{ m.model || '（缺 model）' }}</span>
                  <span class="active-tip-n">{{ m.active }}</span>
                </div>
              </template>
              <span class="active-cell" :class="activeOf(row) > 1 ? 'err' : 'calls'">{{ activeOf(row) }}</span>
            </el-tooltip>
            <span v-else class="calls">0</span>
          </template>
        </el-table-column>
        <el-table-column label="5 分钟调用" align="right" width="150">
          <template #default="{ row }">
            <span class="calls">{{ row.calls_5m }}</span>
            <span v-if="row.errors_5m > 0" class="err">（{{ row.errors_5m }} 错误）</span>
          </template>
        </el-table-column>
        <el-table-column label="60 分钟调用" align="right" width="150">
          <template #default="{ row }">
            <span class="calls">{{ row.calls_60m }}</span>
            <span v-if="row.errors_60m > 0" class="err">（{{ row.errors_60m }} 错误）</span>
          </template>
        </el-table-column>
        <el-table-column label="Token 入/出（60 分钟）" align="right" width="180">
          <template #default="{ row }">
            <span class="tok-in">{{ fmt(row.input_tokens) }}</span>
            <span class="tok-sep">/</span>
            <span class="tok-out">{{ fmt(row.output_tokens) }}</span>
          </template>
        </el-table-column>
        <el-table-column label="成本" align="right" width="100">
          <template #default="{ row }">{{ fmtCost(row.cost) }}</template>
        </el-table-column>
        <el-table-column label="平均延迟" align="right" width="100">
          <template #default="{ row }">{{ row.avg_latency_ms }} ms</template>
        </el-table-column>
        <el-table-column label="输出速度" align="right" width="120">
          <template #default="{ row }">
            <span
              :title="row.stream_tps > 0
                ? '流式生成速度：Σ输出 token ÷ Σ生成阶段耗时（已扣首 token 等待）'
                : (row.throughput_tps > 0 ? '无流式样本，显示≈全量吞吐下界（含非流式预填充/排队）' : '')"
            >
              {{ fmtSpeed(row.stream_tps, row.throughput_tps) }}
            </span>
          </template>
        </el-table-column>
        <el-table-column label="最近调用" width="130">
          <template #default="{ row }">
            <span :title="fmtDateTime(row.last_call_at)">{{ fmtAgo(row.last_call_at) }}</span>
          </template>
        </el-table-column>
      </el-table>
      <div v-else-if="data" class="empty">近 60 分钟暂无调用</div>
    </el-card>

    <el-card shadow="never" class="panel">
      <template #header>
        <div class="card-header">
          <span>最近请求</span>
          <span class="sub">最新 50 条，自动刷新时高亮新记录</span>
        </div>
      </template>
      <el-table v-if="data && recent.length" :data="recent" size="small" :row-class-name="recentRowClass">
        <el-table-column label="时间" width="150">
          <template #default="{ row }">
            <span class="mono" :title="fmtDateTime(row.created_at)">{{ fmtAgo(row.created_at) }}</span>
          </template>
        </el-table-column>
        <el-table-column label="用户" min-width="130">
          <template #default="{ row }">{{ row.username ?? '匿名' }}</template>
        </el-table-column>
        <el-table-column label="模型" min-width="150">
          <template #default="{ row }"><span class="mono">{{ row.model }}</span></template>
        </el-table-column>
        <el-table-column label="端点" min-width="110">
          <template #default="{ row }"><span class="mono">{{ row.endpoint ?? '-' }}</span></template>
        </el-table-column>
        <el-table-column label="流式" width="70" align="center">
          <template #default="{ row }">
            <el-tag v-if="row.streamed" size="small" type="primary" effect="plain">流式</el-tag>
            <span v-else class="sub">-</span>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="80" align="center">
          <template #default="{ row }">
            <el-tag v-if="row.status != null" size="small" :type="statusType(row.status)">{{ row.status }}</el-tag>
            <span v-else class="sub">-</span>
          </template>
        </el-table-column>
        <el-table-column label="延迟" width="100" align="right">
          <template #default="{ row }">
            <span :title="row.first_token_ms != null ? `首 token ${row.first_token_ms} ms` : ''">
              {{ row.latency_ms != null ? row.latency_ms + ' ms' : '-' }}
            </span>
          </template>
        </el-table-column>
        <el-table-column label="输出速度" width="110" align="right">
          <template #default="{ row }">
            <span :title="row.streamed ? '' : '非流式：分母为总耗时，此为下界（≈）'">
              {{ fmtTps(row.tokens_per_sec, !row.streamed) }}
            </span>
          </template>
        </el-table-column>
        <el-table-column label="Token 入/出" width="140" align="right">
          <template #default="{ row }">
            <span class="tok-in">{{ fmt(row.input_tokens) }}</span>
            <span class="tok-sep">/</span>
            <span class="tok-out">{{ fmt(row.output_tokens) }}</span>
          </template>
        </el-table-column>
        <el-table-column label="成本" width="100" align="right">
          <template #default="{ row }">{{ row.cost != null ? fmtCost(row.cost) : '-' }}</template>
        </el-table-column>
      </el-table>
      <div v-else-if="data" class="empty">暂无请求记录</div>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { Refresh } from '@element-plus/icons-vue'
import { request } from '@/api/client'
import type { RealtimeActiveModel, RealtimeCallRow, RealtimeUsageResp, RealtimeUserStat } from '@/api/types'

const data = ref<RealtimeUsageResp | null>(null)
const loading = ref(false)
const autoRefresh = ref(localStorage.getItem('rt_auto') !== '0')
const intervalSec = ref(Number(localStorage.getItem('rt_interval')) || 5)
const lastUpdated = ref<Date | null>(null)
const users = computed(() => data.value?.users ?? [])
const recent = computed(() => data.value?.recent ?? [])
/** user_id → 在途请求数；无记录（近 60 分钟无调用但在途）视为 0 */
const activeMap = computed(
  () => new Map((data.value?.active_by_user ?? []).map((u) => [u.user_id, u.active])),
)
/** user_id → 在途请求的模型分布（客户端请求的原始 model） */
const activeModelsMap = computed(
  () => new Map((data.value?.active_by_user ?? []).map((u) => [u.user_id, u.by_model ?? []])),
)
function activeOf(row: RealtimeUserStat): number {
  return row.user_id != null ? (activeMap.value.get(row.user_id) ?? 0) : 0
}
function activeModelsOf(row: RealtimeUserStat): RealtimeActiveModel[] {
  return row.user_id != null ? (activeModelsMap.value.get(row.user_id) ?? []) : []
}
/** 每秒跳动，驱动相对时间重新渲染 */
const tick = ref(0)

const lastUpdatedText = computed(() => {
  if (!lastUpdated.value) return ''
  const p = (n: number) => String(n).padStart(2, '0')
  const d = lastUpdated.value
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`
})

const cards = computed(() => {
  const s = data.value?.summary
  const rate = s && s.calls > 0 ? Math.round(((s.calls - s.errors) / s.calls) * 100) + '%' : '-'
  return [
    {
      label: '5 分钟调用',
      value: s ? fmt(s.calls) : '-',
      danger: false,
      hint: '最近 5 分钟的调用次数',
    },
    {
      label: '5 分钟错误',
      value: s ? fmt(s.errors) : '-',
      danger: !!s && s.errors > 0,
      hint: '最近 5 分钟状态码 ≥ 400 的调用次数',
    },
    { label: '成功率', value: rate, danger: false, hint: '最近 5 分钟成功率' },
    {
      label: '在途请求',
      value: data.value ? fmt(data.value.active_total) : '-',
      danger: !!data.value && data.value.active_total > 50,
      hint: '当前进行中的请求数（含流式未结束）',
    },
    {
      label: '5 分钟 Token 入/出',
      value: s ? `${fmt(s.input_tokens)} / ${fmt(s.output_tokens)}` : '-',
      danger: false,
      hint: '最近 5 分钟全站输入 / 输出 token（不含在途未结束请求）',
    },
    {
      label: '输出速度',
      value: s ? fmtSpeed(s.stream_tps, s.throughput_tps) : '-',
      danger: false,
      hint: '流式生成速度 = 近 5 分钟 Σ输出 token ÷ Σ生成阶段耗时（已扣首 token 等待）；无流式样本时以 ≈ 显示含非流式的下界吞吐',
    },
    {
      label: '平均延迟',
      value: s ? `${s.avg_latency_ms} ms` : '-',
      danger: false,
      hint: '最近 5 分钟平均请求耗时',
    },
    { label: '成本', value: s ? fmtCost(s.cost) : '-', danger: false, hint: '最近 5 分钟成本' },
  ]
})

/** 新请求行高亮：记录上次最大 id，本次超过的 id 闪烁一次 */
const flashIds = ref<Set<number>>(new Set())
let maxSeenId = 0
let hasLoadedOnce = false
let flashTimer: number | null = null

function markNewRows(rows: RealtimeCallRow[]): void {
  if (!rows.length) return
  if (hasLoadedOnce) {
    const fresh = rows.filter((r) => r.id > maxSeenId).map((r) => r.id)
    if (fresh.length) {
      flashIds.value = new Set(fresh)
      if (flashTimer != null) window.clearTimeout(flashTimer)
      flashTimer = window.setTimeout(() => {
        flashIds.value = new Set()
        flashTimer = null
      }, 2200)
    }
  }
  maxSeenId = rows[0].id
  hasLoadedOnce = true
}

function userRowClass({ row }: { row: RealtimeUserStat }): string {
  return row.errors_5m > 0 ? 'user-error' : ''
}

function recentRowClass({ row }: { row: RealtimeCallRow }): string {
  return flashIds.value.has(row.id) ? 'flash-row' : ''
}

function statusType(status: number): 'success' | 'info' | 'warning' | 'danger' {
  if (status < 300) return 'success'
  if (status < 400) return 'info'
  if (status < 500) return 'warning'
  return 'danger'
}

function fmt(n: number | null | undefined): string {
  return (n ?? 0).toLocaleString()
}

function fmtCost(n: number | null | undefined): string {
  return '¥' + (n ?? 0).toFixed(2)
}

/** 单请求速度：无有效样本显示 '-'；非流式为下界，加 ≈ 前缀 */
function fmtTps(n: number | null | undefined, approx = false): string {
  return n != null && n > 0 ? `${approx ? '≈' : ''}${n.toFixed(1)} tok/s` : '-'
}

/** 聚合速度：优先流式生成速度；无流式样本时以 ≈ 显示全量下界吞吐 */
function fmtSpeed(stream: number | null | undefined, all: number | null | undefined): string {
  if (stream != null && stream > 0) return `${stream.toFixed(1)} tok/s`
  if (all != null && all > 0) return `≈${all.toFixed(1)} tok/s`
  return '-'
}

/** 本地时间 YYYY-MM-DD HH:mm:ss */
function fmtDateTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '-'
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`
}

/** 相对时间：x 秒/分钟/小时前（由 tick 驱动每秒重算） */
function fmtAgo(iso: string): string {
  void tick.value
  const diff = Math.max(0, Date.now() - new Date(iso).getTime())
  const sec = Math.floor(diff / 1000)
  if (sec < 60) return sec + ' 秒前'
  const min = Math.floor(sec / 60)
  if (min < 60) return min + ' 分钟前'
  return Math.floor(min / 60) + ' 小时前'
}

let timer: number | null = null
let tickTimer: number | null = null

function schedule(): void {
  if (timer != null) window.clearInterval(timer)
  timer = null
  if (!autoRefresh.value || document.hidden) return
  timer = window.setInterval(() => {
    void load()
  }, intervalSec.value * 1000)
}

function onVisibility(): void {
  if (document.hidden) return
  void load()
  schedule()
}

let inFlight = false
/** 连续失败去重：只在失败序列首次弹提示，恢复成功后重置 */
let errorShown = false

async function load(): Promise<void> {
  if (inFlight) return
  inFlight = true
  loading.value = true
  try {
    data.value = await request<RealtimeUsageResp>('/api/admin/usage/realtime')
    errorShown = false
    lastUpdated.value = new Date()
    markNewRows(data.value.recent)
  } catch (e) {
    if (!errorShown) {
      ElMessage.error(e instanceof Error ? e.message : '加载实时数据失败')
      errorShown = true
    }
  } finally {
    inFlight = false
    loading.value = false
  }
}

watch([autoRefresh, intervalSec], () => {
  if (autoRefresh.value) void load()
  schedule()
})
watch([autoRefresh, intervalSec], () => {
  localStorage.setItem('rt_auto', autoRefresh.value ? '1' : '0')
  localStorage.setItem('rt_interval', String(intervalSec.value))
})

onMounted(() => {
  document.addEventListener('visibilitychange', onVisibility)
  tickTimer = window.setInterval(() => {
    tick.value++
  }, 1000)
  void load()
  schedule()
})

onBeforeUnmount(() => {
  if (timer != null) window.clearInterval(timer)
  if (tickTimer != null) window.clearInterval(tickTimer)
  if (flashTimer != null) window.clearTimeout(flashTimer)
  document.removeEventListener('visibilitychange', onVisibility)
})
</script>

<style scoped>
.realtime-page {
  padding: 20px;
}

.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 16px;
}

.toolbar-left {
  display: flex;
  align-items: center;
  gap: 10px;
}

.toolbar-right {
  display: flex;
  align-items: center;
  gap: 12px;
}

.label {
  color: #909399;
  font-size: 13px;
}

.live-dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: #67c23a;
  box-shadow: 0 0 0 0 rgba(103, 194, 58, 0.5);
  animation: pulse 2s infinite;
}

.live-dot.paused {
  background: #c0c4cc;
  animation: none;
}

@keyframes pulse {
  0% {
    box-shadow: 0 0 0 0 rgba(103, 194, 58, 0.5);
  }
  70% {
    box-shadow: 0 0 0 8px rgba(103, 194, 58, 0);
  }
  100% {
    box-shadow: 0 0 0 0 rgba(103, 194, 58, 0);
  }
}

.live-text {
  font-size: 14px;
  font-weight: 600;
  color: #303133;
}

.updated {
  font-size: 12px;
  color: #909399;
}

.cards {
  margin-bottom: 16px;
}

.stat-card :deep(.el-card__body) {
  padding: 16px 18px;
}

.stat-title {
  font-size: 13px;
  color: #909399;
  margin-bottom: 10px;
}

.stat-value {
  font-size: 22px;
  font-weight: 600;
  color: #303133;
  line-height: 1.2;
  word-break: break-all;
}

.stat-value.danger {
  color: #f56c6c;
}

.panel {
  margin-bottom: 16px;
}

.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.sub {
  color: #909399;
  font-size: 12px;
}

.username {
  font-weight: 500;
  color: #303133;
}

.display-name {
  color: #909399;
  font-size: 12px;
}

.calls {
  font-weight: 600;
  color: #303133;
}

.err {
  color: #f56c6c;
  font-size: 12px;
}

.active-cell {
  cursor: default;
  text-decoration: underline dotted;
  text-decoration-color: rgba(144, 147, 153, 0.7);
  text-underline-offset: 3px;
}

.active-tip-row {
  display: flex;
  justify-content: space-between;
  gap: 16px;
  line-height: 1.7;
}

.active-tip-n {
  font-weight: 600;
}

.tok-in {
  color: #67c23a;
}

.tok-sep {
  color: #c0c4cc;
  margin: 0 4px;
}

.tok-out {
  color: #e6a23c;
}

.mono {
  font-family: var(--el-font-family-mono);
}

.empty {
  padding: 40px 0;
  text-align: center;
  color: #909399;
  font-size: 13px;
}

.user-error {
  background: #fef0f0;
}

.flash-row {
  animation: flash-bg 2.2s ease-out;
}

@keyframes flash-bg {
  0% {
    background: rgba(64, 158, 255, 0.28);
  }
  100% {
    background: transparent;
  }
}
</style>
