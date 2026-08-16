<template>
  <div class="status-page">
    <header class="top">
      <div class="brand">
        <span class="logo">LG</span>
        <div>
          <h1>LLM Gateway 服务状态</h1>
          <p class="sub">网关 · 数据库 · 上游供应商运行状态一览</p>
        </div>
      </div>
      <div class="refresh">
        <span v-if="data" class="updated">最后更新 {{ updatedText }}</span>
        <span class="countdown">{{ countdown }}s 后自动刷新</span>
        <el-button size="small" :loading="loading" @click="load(true)">立即刷新</el-button>
      </div>
    </header>

    <main class="content">
      <section class="banner" :class="data?.overall ?? 'operational'">
        <span class="dot"></span>
        <div class="btext">
          <h2>{{ overallText }}</h2>
          <p>{{ bannerSub }}</p>
        </div>
        <div class="generated">近 30 天共 {{ incidentCount }} 起事故</div>
      </section>

      <section class="card">
        <h3>系统组件</h3>
        <p v-if="!data" class="empty">加载中…</p>
        <template v-else>
          <div v-for="c in data.components" :key="c.key" class="row">
            <span class="dot" :class="c.status"></span>
            <div class="cinfo">
              <div class="cline">
                <span class="cname">{{ c.name }}</span>
                <el-tag size="small" effect="plain">{{ c.kind === 'system' ? '系统' : '上游' }}</el-tag>
                <span class="cstatus" :class="c.status">{{ statusText(c.status) }}</span>
              </div>
              <div class="cmeta">
                <template v-if="c.detail">{{ c.detail }}</template>
                <template v-if="c.uptime_30d != null">
                  <template v-if="c.detail"> · </template>30 天可用率 {{ (c.uptime_30d * 100).toFixed(2) }}%
                </template>
                <template v-if="c.latency_ms != null">
                  <template v-if="c.detail || c.uptime_30d != null"> · </template>延迟 {{ c.latency_ms }}ms
                </template>
                <template v-if="c.calls_30d > 0">
                  <template v-if="c.detail || c.uptime_30d != null || c.latency_ms != null"> · </template
                  >{{ c.calls_30d }} 次调用
                </template>
              </div>
            </div>
          </div>
        </template>
      </section>

      <section v-if="data && uptimeSeries.length" class="card">
        <h3>近 30 天可用率（按日，UTC）</h3>
        <div v-for="s in uptimeSeries" :key="s.key" class="strip-row">
          <span class="sname">{{ s.name }}</span>
          <div class="strip">
            <el-tooltip v-for="(d, i) in s.days" :key="i" :content="dayTip(d)" placement="top">
              <span class="cell" :class="cellClass(d)"></span>
            </el-tooltip>
          </div>
          <span class="suptime">{{ seriesUptime(s) }}</span>
        </div>
        <div class="legend">
          <span><i class="cell ok"></i>正常</span>
          <span><i class="cell warn"></i>波动</span>
          <span><i class="cell bad"></i>异常</span>
          <span><i class="cell none"></i>无数据</span>
        </div>
      </section>

      <section class="card">
        <h3>事故记录（近 30 天）</h3>
        <p v-if="!data" class="empty">加载中…</p>
        <p v-else-if="!incidents.length" class="empty">过去 30 天无重大事故</p>
        <template v-else>
          <div v-for="(inc, i) in incidents" :key="i" class="incident" :class="inc.severity">
            <span class="bar"></span>
            <div class="ibody">
              <div class="ititle">
                {{ inc.title }}
                <el-tag size="small" :type="inc.severity === 'major' ? 'danger' : 'warning'" effect="plain">
                  {{ inc.severity === 'major' ? '重大' : '轻微' }}
                </el-tag>
                <el-tag v-if="inc.ongoing" size="small" type="danger" effect="dark">进行中</el-tag>
              </div>
              <div class="itime">{{ fmtRange(inc) }}</div>
              <div class="idetail">
                错误率 {{ (inc.error_rate * 100).toFixed(1) }}%（{{ inc.errors }}/{{ inc.calls }} 次调用失败）
              </div>
            </div>
          </div>
        </template>
      </section>

      <footer class="foot">数据基于最近 30 天真实调用记录统计 · 页面每 60 秒自动刷新</footer>
    </main>
  </div>
</template>

<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { api } from '@/api/client'
import type { StatusDayPoint, StatusIncident, StatusResp, StatusUptimeSeries } from '@/api/types'

const data = ref<StatusResp | null>(null)
const loading = ref(false)
const countdown = ref(60)

const REFRESH_MS = 60_000

let timer: number | undefined
let countdownTimer: number | undefined

async function load(force = false) {
  if (loading.value || (!force && document.hidden)) return
  loading.value = true
  try {
    data.value = await api.status()
  } catch {
    // 保持旧数据，下次轮询重试
  } finally {
    loading.value = false
    countdown.value = 60
  }
}

onMounted(() => {
  document.title = '服务状态 - LLM Gateway'
  load(true)
  timer = window.setInterval(() => load(), REFRESH_MS)
  countdownTimer = window.setInterval(() => {
    countdown.value = countdown.value > 0 ? countdown.value - 1 : 60
  }, 1000)
})

onBeforeUnmount(() => {
  window.clearInterval(timer)
  window.clearInterval(countdownTimer)
})

const overallText = computed(() => {
  switch (data.value?.overall) {
    case 'down':
      return '服务不可用'
    case 'degraded':
      return '部分服务异常'
    default:
      return '全部系统正常'
  }
})

const bannerSub = computed(() => {
  switch (data.value?.overall) {
    case 'down':
      return '存在不可用组件，请检查下方组件列表'
    case 'degraded':
      return '部分组件出现性能下降，详情见下方组件列表'
    default:
      return '所有系统组件均运行正常'
  }
})

const incidentCount = computed(() => data.value?.incidents.length ?? 0)
const incidents = computed(() => data.value?.incidents ?? [])
const uptimeSeries = computed(() => data.value?.uptime ?? [])
const updatedText = computed(() =>
  data.value ? new Date(data.value.generated_at).toLocaleString('zh-CN', { hour12: false }) : '',
)

const STATUS_TEXT: Record<string, string> = {
  operational: '运行正常',
  degraded: '性能下降',
  down: '不可用',
  unknown: '未知',
}

function statusText(s: string): string {
  return STATUS_TEXT[s] ?? s
}

function cellClass(d: StatusDayPoint): string {
  if (d.success_rate == null || d.calls === 0) return 'none'
  if (d.success_rate >= 0.99) return 'ok'
  if (d.success_rate >= 0.9) return 'warn'
  if (d.success_rate >= 0.5) return 'bad'
  return 'down'
}

function dayTip(d: StatusDayPoint): string {
  if (d.calls === 0) return `${d.date}：无数据`
  const rate = ((d.success_rate ?? 0) * 100).toFixed(1)
  return `${d.date}：可用率 ${rate}%（${d.calls - d.errors}/${d.calls} 成功）`
}

function seriesUptime(s: StatusUptimeSeries): string {
  const calls = s.days.reduce((a, d) => a + d.calls, 0)
  if (calls === 0) return '—'
  const errors = s.days.reduce((a, d) => a + d.errors, 0)
  return `${(((calls - errors) / calls) * 100).toFixed(2)}%`
}

function fmtTime(iso: string): string {
  return new Date(iso).toLocaleString('zh-CN', { hour12: false })
}

function fmtRange(inc: StatusIncident): string {
  return `${fmtTime(inc.start)} → ${fmtTime(inc.end)}${inc.ongoing ? '（持续中）' : ''}`
}
</script>

<style scoped>
.status-page {
  min-height: 100vh;
  background: #f5f7fa;
  color: #303133;
}

.top {
  background: #1f2d3d;
  color: #fff;
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 18px 32px;
  gap: 16px;
  flex-wrap: wrap;
}
.brand {
  display: flex;
  align-items: center;
  gap: 12px;
}
.brand .logo {
  width: 40px;
  height: 40px;
  border-radius: 10px;
  background: #409eff;
  color: #fff;
  font-size: 15px;
  font-weight: 700;
  display: flex;
  align-items: center;
  justify-content: center;
}
.brand h1 {
  font-size: 17px;
  margin: 0;
}
.brand .sub {
  font-size: 12px;
  color: #aeb9c7;
  margin: 2px 0 0;
}
.refresh {
  display: flex;
  align-items: center;
  gap: 14px;
  font-size: 13px;
  color: #aeb9c7;
}
.refresh .updated {
  color: #c8d3e0;
}

.content {
  max-width: 860px;
  margin: 0 auto;
  padding: 24px 20px 40px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.banner {
  display: flex;
  align-items: center;
  gap: 14px;
  padding: 20px 24px;
  border-radius: 10px;
  background: #f0f9eb;
  border: 1px solid #e1f3d8;
}
.banner.degraded {
  background: #fdf6ec;
  border-color: #faecd8;
}
.banner.down {
  background: #fef0f0;
  border-color: #fde2e2;
}
.banner h2 {
  font-size: 18px;
  margin: 0 0 4px;
}
.banner p {
  font-size: 13px;
  margin: 0;
  color: #606266;
}
.banner .generated {
  margin-left: auto;
  font-size: 12px;
  color: #909399;
  white-space: nowrap;
}

.dot {
  width: 12px;
  height: 12px;
  border-radius: 50%;
  flex-shrink: 0;
  background: #909399;
}
.dot.operational,
.dot.ok {
  background: #67c23a;
}
.dot.degraded,
.dot.warn {
  background: #e6a23c;
}
.dot.down,
.dot.bad {
  background: #f56c6c;
}
.dot.unknown,
.dot.none {
  background: #c0c4cc;
}

.card {
  background: #fff;
  border-radius: 10px;
  padding: 18px 24px;
  box-shadow: 0 1px 4px rgba(0, 0, 0, 0.05);
}
.card h3 {
  font-size: 15px;
  margin: 0 0 14px;
  color: #303133;
}
.empty {
  color: #909399;
  font-size: 13px;
  margin: 8px 0;
}

.row {
  display: flex;
  align-items: flex-start;
  gap: 12px;
  padding: 10px 0;
  border-bottom: 1px solid #f0f2f5;
}
.row:last-child {
  border-bottom: none;
}
.row .dot {
  margin-top: 4px;
}
.cline {
  display: flex;
  align-items: center;
  gap: 8px;
}
.cname {
  font-size: 14px;
  font-weight: 600;
}
.cstatus {
  font-size: 13px;
  font-weight: 600;
}
.cstatus.operational {
  color: #67c23a;
}
.cstatus.degraded {
  color: #e6a23c;
}
.cstatus.down {
  color: #f56c6c;
}
.cstatus.unknown {
  color: #909399;
}
.cmeta {
  font-size: 12px;
  color: #909399;
  margin-top: 3px;
}

.strip-row {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 6px 0;
}
.sname {
  width: 130px;
  font-size: 13px;
  flex-shrink: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.strip {
  display: flex;
  gap: 2px;
  flex: 1;
}
.cell {
  flex: 1;
  min-width: 4px;
  height: 16px;
  border-radius: 2px;
  background: #c0c4cc;
}
.cell.ok {
  background: #67c23a;
}
.cell.warn {
  background: #e6a23c;
}
.cell.bad {
  background: #f56c6c;
}
.cell.down {
  background: #b33131;
}
.cell.none {
  background: #e4e7ed;
}
.suptime {
  width: 64px;
  text-align: right;
  font-size: 12px;
  color: #606266;
  flex-shrink: 0;
}
.legend {
  display: flex;
  gap: 18px;
  margin-top: 12px;
  font-size: 12px;
  color: #909399;
}
.legend .cell {
  display: inline-block;
  flex: none;
  width: 12px;
  height: 12px;
  margin-right: 5px;
  vertical-align: -2px;
}

.incident {
  display: flex;
  gap: 12px;
  padding: 12px 0;
  border-bottom: 1px solid #f0f2f5;
}
.incident:last-child {
  border-bottom: none;
}
.incident .bar {
  width: 4px;
  border-radius: 2px;
  background: #e6a23c;
  flex-shrink: 0;
}
.incident.major .bar {
  background: #f56c6c;
}
.ititle {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 14px;
  font-weight: 600;
}
.itime {
  font-size: 12px;
  color: #909399;
  margin-top: 3px;
}
.idetail {
  font-size: 12px;
  color: #606266;
  margin-top: 3px;
}

.foot {
  text-align: center;
  font-size: 12px;
  color: #909399;
  padding: 8px 0;
}
</style>
