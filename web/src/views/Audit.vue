<template>
  <div class="page">
    <div class="toolbar">
      <div>
        <h2 class="page-title">审计日志</h2>
        <p class="page-desc">管理操作与 Key 操作留痕</p>
      </div>
      <div class="filters">
        <el-select
          v-model="actionFilter"
          placeholder="全部动作"
          clearable
          style="width: 150px"
          @change="search"
        >
          <el-option v-for="o in actionOptions" :key="o.value" :label="o.label" :value="o.value" />
        </el-select>
        <el-input
          v-model="keyword"
          placeholder="搜索动作 / 目标 / 详情"
          clearable
          style="width: 220px"
          @keyup.enter="search"
          @clear="search"
        >
          <template #append>
            <el-button :icon="Search" @click="search" />
          </template>
        </el-input>
        <el-button :icon="Refresh" :loading="loading" @click="load">刷新</el-button>
      </div>
    </div>

    <el-table v-loading="loading" :data="logs" stripe class="audit-table">
      <el-table-column prop="id" label="ID" width="80" />
      <el-table-column label="时间" width="180">
        <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
      </el-table-column>
      <el-table-column label="操作者" width="140">
        <template #default="{ row }">{{ row.actor_username ?? (row.actor_id ?? '-') }}</template>
      </el-table-column>
      <el-table-column label="动作" width="150">
        <template #default="{ row }">
          <el-tag :type="actionInfo(row.action).type" size="small">{{ actionInfo(row.action).label }}</el-tag>
        </template>
      </el-table-column>
      <el-table-column label="目标" width="160">
        <template #default="{ row }">{{ targetText(row) }}</template>
      </el-table-column>
      <el-table-column label="详情" min-width="260">
        <template #default="{ row }">
          <code class="detail-text">{{ detailText(row.detail) }}</code>
        </template>
      </el-table-column>
      <template #empty>
        <el-empty description="暂无审计日志" :image-size="80" />
      </template>
    </el-table>

    <div class="pager">
      <el-pagination
        v-model:current-page="page"
        v-model:page-size="pageSize"
        :total="total"
        :page-sizes="[20, 50, 100, 200]"
        layout="total, sizes, prev, pager, next"
        background
        @current-change="load"
        @size-change="search"
      />
    </div>
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { Refresh, Search } from '@element-plus/icons-vue'
import { request } from '@/api/client'
import type { AuditResp, AuditRow } from '@/api/types'

type TagType = 'primary' | 'success' | 'warning' | 'danger' | 'info'

const actionMap: Record<string, { label: string; type: TagType }> = {
  'api_key.create': { label: '创建 Key', type: 'success' },
  'api_key.revoke': { label: '吊销 Key', type: 'danger' },
  'user.create': { label: '新建用户', type: 'success' },
  'user.status': { label: '用户状态变更', type: 'warning' },
  'user.force_logout': { label: '强制下线', type: 'danger' },
  'user.reset_password': { label: '重置密码', type: 'warning' },
}

const actionOptions = Object.entries(actionMap).map(([value, v]) => ({
  value,
  label: v.label,
}))

const logs = ref<AuditRow[]>([])
const total = ref(0)
const loading = ref(false)
const actionFilter = ref('')
const keyword = ref('')
const page = ref(1)
const pageSize = ref(50)

function actionInfo(action: string): { label: string; type: TagType } {
  return actionMap[action] ?? { label: action, type: 'info' }
}

function formatTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const pad = (n: number) => String(n).padStart(2, '0')
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  )
}

function targetText(row: AuditRow): string {
  if (row.target_type === null || row.target_id === null) return '-'
  return `${row.target_type}#${row.target_id}`
}

function detailText(detail: unknown): string {
  if (detail === null || detail === undefined) return '-'
  try {
    return JSON.stringify(detail)
  } catch {
    return String(detail)
  }
}

async function load(): Promise<void> {
  loading.value = true
  try {
    const params = new URLSearchParams()
    params.set('limit', String(pageSize.value))
    params.set('offset', String((page.value - 1) * pageSize.value))
    if (actionFilter.value) params.set('action', actionFilter.value)
    if (keyword.value.trim()) params.set('q', keyword.value.trim())
    const resp = await request<AuditResp>(`/api/admin/audit?${params.toString()}`)
    logs.value = resp.audit_logs
    total.value = resp.total
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载审计日志失败')
  } finally {
    loading.value = false
  }
}

/** 筛选/搜索/翻页大小变化 → 回到第一页重新加载 */
function search(): void {
  page.value = 1
  load()
}

onMounted(load)
</script>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.toolbar {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
}

.page-title {
  margin: 0;
  font-size: 18px;
  font-weight: 600;
  color: #303133;
}

.page-desc {
  margin: 4px 0 0;
  font-size: 13px;
  color: #909399;
}

.filters {
  display: flex;
  align-items: center;
  gap: 8px;
}

.pager {
  display: flex;
  justify-content: flex-end;
}

.audit-table {
  background: #fff;
}

.detail-text {
  font-family: 'JetBrains Mono', Consolas, Menlo, monospace;
  font-size: 12px;
  color: #606266;
  background: #f5f7fa;
  border-radius: 4px;
  padding: 2px 6px;
  word-break: break-all;
}
</style>
