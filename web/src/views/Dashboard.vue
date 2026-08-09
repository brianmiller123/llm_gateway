<template>
  <div class="dashboard" v-loading="loading">
    <el-row :gutter="16">
      <el-col :xs="12" :sm="12" :md="6">
        <el-card shadow="never" class="stat-card">
          <div class="stat-title">当月 Token</div>
          <div class="stat-value">
            {{ monthTokens }}
            <span class="stat-unit">tokens</span>
          </div>
        </el-card>
      </el-col>
      <el-col :xs="12" :sm="12" :md="6">
        <el-card shadow="never" class="stat-card">
          <div class="stat-title">当月成本</div>
          <div class="stat-value">
            <span class="stat-currency">¥</span>{{ monthCost }}
            <span class="stat-unit">元</span>
          </div>
        </el-card>
      </el-col>
      <el-col :xs="12" :sm="12" :md="6">
        <el-card shadow="never" class="stat-card">
          <div class="stat-title">活跃 API Key</div>
          <div class="stat-value">
            {{ activeKeys }}
            <span class="stat-unit">个</span>
          </div>
        </el-card>
      </el-col>
      <el-col :xs="12" :sm="12" :md="6">
        <el-card shadow="never" class="stat-card">
          <div class="stat-title">近 7 天调用</div>
          <div class="stat-value">
            {{ weekCalls }}
            <span class="stat-unit">次</span>
          </div>
        </el-card>
      </el-col>
    </el-row>

    <el-row :gutter="16" class="chart-row">
      <el-col :xs="24" :md="12">
        <el-card shadow="never" header="近 7 天 Token 趋势">
          <div ref="trendRef" class="chart"></div>
        </el-card>
      </el-col>
      <el-col :xs="24" :md="12">
        <el-card shadow="never" header="本月模型调用分布">
          <el-empty
            v-if="usage && usage.by_model.length === 0"
            description="暂无调用数据"
            class="chart-empty"
          />
          <div v-else ref="pieRef" class="chart"></div>
        </el-card>
      </el-col>
    </el-row>
  </div>
</template>

<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import * as echarts from 'echarts'
import { ElMessage } from 'element-plus'
import { ApiError, request } from '@/api/client'
import type { MeResp, UsageResp } from '@/api/types'

const loading = ref(false)
const me = ref<MeResp | null>(null)
const usage = ref<UsageResp | null>(null)

const trendRef = ref<HTMLDivElement | null>(null)
const pieRef = ref<HTMLDivElement | null>(null)

let trendChart: echarts.ECharts | null = null
let pieChart: echarts.ECharts | null = null

const monthTokens = computed(() => (me.value?.usage.tokens ?? 0).toLocaleString('zh-CN'))
const monthCost = computed(() => (me.value?.usage.cost ?? 0).toFixed(2))
const activeKeys = computed(() => me.value?.active_keys ?? 0)
const weekCalls = computed(() =>
  (usage.value?.last_7_days ?? []).reduce((sum, d) => sum + d.call_count, 0).toLocaleString('zh-CN'),
)

function renderTrendChart() {
  if (!usage.value || !trendRef.value) return
  trendChart ??= echarts.init(trendRef.value)
  const days = usage.value.last_7_days
  trendChart.setOption({
    tooltip: { trigger: 'axis' },
    legend: { data: ['输入 Token', '输出 Token'] },
    grid: { left: 56, right: 20, top: 40, bottom: 30 },
    xAxis: {
      type: 'category',
      boundaryGap: false,
      data: days.map((d) => d.stat_date.slice(5)),
    },
    yAxis: { type: 'value' },
    series: [
      {
        name: '输入 Token',
        type: 'line',
        smooth: true,
        symbolSize: 6,
        data: days.map((d) => d.input_tokens),
        lineStyle: { width: 2 },
        itemStyle: { color: '#409eff' },
        areaStyle: { opacity: 0.08 },
      },
      {
        name: '输出 Token',
        type: 'line',
        smooth: true,
        symbolSize: 6,
        data: days.map((d) => d.output_tokens),
        lineStyle: { width: 2 },
        itemStyle: { color: '#67c23a' },
        areaStyle: { opacity: 0.08 },
      },
    ],
  })
}

function renderPieChart() {
  if (!usage.value || !pieRef.value) return
  pieChart ??= echarts.init(pieRef.value)
  pieChart.setOption({
    tooltip: { trigger: 'item', formatter: '{b}: {c} 次（{d}%）' },
    legend: { bottom: 0, type: 'scroll' },
    series: [
      {
        name: '调用次数',
        type: 'pie',
        radius: ['40%', '65%'],
        center: ['50%', '44%'],
        avoidLabelOverlap: true,
        data: usage.value.by_model.map((m) => ({ name: m.model, value: m.call_count })),
        emphasis: {
          itemStyle: { shadowBlur: 10, shadowOffsetX: 0, shadowColor: 'rgba(0, 0, 0, 0.2)' },
        },
      },
    ],
  })
}

function handleResize() {
  trendChart?.resize()
  pieChart?.resize()
}

async function loadData() {
  loading.value = true
  try {
    const [meResp, usageResp] = await Promise.all([
      request<MeResp>('/api/me'),
      request<UsageResp>('/api/usage'),
    ])
    me.value = meResp
    usage.value = usageResp
    renderTrendChart()
    renderPieChart()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载数据失败，请稍后重试')
  } finally {
    loading.value = false
  }
}

onMounted(() => {
  window.addEventListener('resize', handleResize)
  loadData()
})

onBeforeUnmount(() => {
  window.removeEventListener('resize', handleResize)
  trendChart?.dispose()
  pieChart?.dispose()
  trendChart = null
  pieChart = null
})
</script>

<style scoped>
.stat-card :deep(.el-card__body) {
  padding: 18px 20px;
}
.stat-title {
  font-size: 13px;
  color: #909399;
  margin-bottom: 10px;
}
.stat-value {
  font-size: 26px;
  font-weight: 600;
  color: #303133;
  line-height: 1;
}
.stat-currency {
  font-size: 16px;
  font-weight: 500;
  color: #606266;
  margin-right: 2px;
}
.stat-unit {
  font-size: 13px;
  font-weight: 400;
  color: #909399;
  margin-left: 4px;
}
.chart-row {
  margin-top: 16px;
}
.chart {
  height: 320px;
}
.chart-empty {
  height: 320px;
  display: flex;
  align-items: center;
  justify-content: center;
}
</style>
