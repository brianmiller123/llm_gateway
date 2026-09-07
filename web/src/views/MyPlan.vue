<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import * as echarts from 'echarts'
import { nextTick, onBeforeUnmount } from 'vue'
import { request } from '@/api/client'
import type { MyNotificationsResp, MyPlanResp } from '@/api/types'

const loading = ref(false)
const plan = ref<MyPlanResp['plan']>(null)
const period = ref<MyPlanResp['period']>(null)
const daily = ref<MyPlanResp['daily']>([])
const inactivePlans = ref<MyPlanResp['inactive_plans']>([])
const notifications = ref<MyNotificationsResp['notifications']>([])

const PERIOD_LABELS: Record<string, string> = {
  hourly: '小时级重置',
  daily: '按自然日重置',
  monthly: '按自然月重置',
  total: '总量（不重置）',
}
function periodLabel(pt: string, hours?: number): string {
  return pt === 'hourly' ? `每 ${hours ?? 1} 小时重置` : PERIOD_LABELS[pt] ?? pt
}
const OVERAGE_LABELS: Record<string, string> = {
  block: '超额后暂停请求',
  downgrade: '超额后降级到更慢模型',
  log: '超额后仅记录告警',
}

const trendRef = ref<HTMLDivElement>()
let trendChart: echarts.ECharts | null = null

function fmtTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '-'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

function renderTrend() {
  if (!trendRef.value) return
  if (!trendChart) {
    trendChart = echarts.init(trendRef.value)
  }
  trendChart.setOption({
    grid: { left: 56, right: 16, top: 32, bottom: 28 },
    tooltip: { trigger: 'axis' },
    xAxis: { type: 'category', data: daily.value.map((d) => d.stat_date) },
    yAxis: { type: 'value' },
    series: [
      {
        name: 'tokens',
        type: 'bar',
        data: daily.value.map((d) => d.tokens),
      },
    ],
  })
}

function onResize() {
  trendChart?.resize()
}

async function load() {
  loading.value = true
  try {
    const resp = await request<MyPlanResp>('/api/me/plan')
    plan.value = resp.plan
    period.value = resp.period
    daily.value = resp.daily
    inactivePlans.value = resp.inactive_plans ?? []
    const notif = await request<MyNotificationsResp>('/api/me/notifications')
    notifications.value = notif.notifications
    await nextTick()
    renderTrend()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载套餐信息失败')
  } finally {
    loading.value = false
  }
}

/** 「已加入但当前不生效」的原因文案（与后端 reason 对应） */
function inactiveReason(p: MyPlanResp['inactive_plans'][number]): string {
  switch (p.reason) {
    case 'disabled':
      return '该 Plan 已停用'
    case 'outside_active_window':
      return `不在生效时段（${p.active_start ?? '--'} - ${p.active_end ?? '--'}）`
    case 'model_scope':
      return '该模型不在 Plan 的模型作用域内'
    default:
      return '当前不生效'
  }
}

onMounted(() => {
  load()
  window.addEventListener('resize', onResize)
})

onBeforeUnmount(() => {
  window.removeEventListener('resize', onResize)
  trendChart?.dispose()
})
</script>

<template>
  <div class="page" v-loading="loading">
    <el-card v-if="plan && period" shadow="never">
      <div class="plan-head">
        <div>
          <div class="plan-name">{{ plan.name }}</div>
          <div class="plan-meta">
            所属分组：{{ plan.group }} · {{ periodLabel(plan.period_type, plan.period_hours) }} ·
            {{ OVERAGE_LABELS[plan.overage_action] }}
            <template v-if="plan.downgrade_model">（{{ plan.downgrade_model }}）</template>
          </div>
          </div>
        <div class="limit-chip">配额 {{ plan.token_limit_display }} tokens</div>
      </div>
      <el-progress
        :percentage="Math.min(period.percent, 100)"
        :status="period.percent >= 100 ? 'exception' : period.percent >= 80 ? 'warning' : 'success'"
        :stroke-width="14"
        :format="(p: number) => `${p.toFixed(1)}%`"
      />
      <div class="usage-row">
        <span>已用 {{ period.used.toLocaleString() }}</span>
        <span>剩余 {{ period.remaining.toLocaleString() }} tokens</span>
        <span class="muted">当前周期：{{ period.key }}</span>
      </div>
    </el-card>

    <el-alert
      v-if="!loading && !plan && inactivePlans.length"
      type="warning"
      :closable="false"
      title="已加入 Coding Plan，但当前不生效"
    >
      <div v-for="p in inactivePlans" :key="p.plan_id">
        「{{ p.name }}」：{{ inactiveReason(p) }}
      </div>
    </el-alert>

    <el-alert
      v-if="!loading && !plan && !inactivePlans.length"
      type="info"
      :closable="false"
      title="尚未加入任何 Coding Plan"
      description="您还未被加入任何 Coding Plan（成员可由管理员按「直连用户」或「用户分组」两种方式添加），当前不受配额限制。请联系管理员。"
    />

    <el-card v-if="daily.length" shadow="never">
      <div class="stat-label">近 30 日消耗明细</div>
      <div ref="trendRef" class="chart" />
    </el-card>

    <el-card shadow="never">
      <div class="stat-label">用量通知</div>
      <el-table v-if="notifications.length" :data="notifications" stripe size="small">
        <el-table-column label="级别" width="90">
          <template #default="{ row }">
            <el-tag
              size="small"
              :type="row.level >= 95 ? 'danger' : 'warning'"
              effect="plain"
            >
              {{ row.level }}%
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="Plan" prop="plan_name" min-width="130" />
        <el-table-column label="用量" min-width="180">
          <template #default="{ row }">
            {{ row.used.toLocaleString() }} / {{ row.limit_tokens.toLocaleString() }}
          </template>
        </el-table-column>
        <el-table-column label="时间" width="150">
          <template #default="{ row }">{{ fmtTime(row.created_at) }}</template>
        </el-table-column>
      </el-table>
      <el-empty v-else description="暂无通知" :image-size="60" />
    </el-card>
  </div>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.plan-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  margin-bottom: 16px;
}
.plan-name {
  font-size: 18px;
  font-weight: 600;
  color: #303133;
}
.plan-meta {
  font-size: 13px;
  color: #909399;
  margin-top: 4px;
}
.limit-chip {
  font-size: 13px;
  color: #409eff;
  background: #ecf5ff;
  border-radius: 6px;
  padding: 4px 10px;
}
.usage-row {
  display: flex;
  gap: 20px;
  font-size: 13px;
  color: #606266;
  margin-top: 10px;
}
.muted {
  color: #909399;
}
.stat-label {
  font-size: 12px;
  color: #909399;
  margin-bottom: 10px;
}
.chart {
  height: 240px;
}
</style>
