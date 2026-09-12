<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { request } from '@/api/client'
import type { ModelRow, ModelTestResp, ModelTestResult, ModelsRefreshResp, ModelsResp } from '@/api/types'

const loading = ref(false)
const refreshing = ref(false)
const models = ref<ModelRow[]>([])

/** 测试全部进行中 */
const testing = ref(false)
/** 单模型测试进行中（行 id） */
const testingId = ref<number | null>(null)
/** 启停切换进行中（行 id） */
const togglingId = ref<number | null>(null)
/** 测试全部结果弹窗 */
const resultDialog = ref(false)
const results = ref<ModelTestResult[]>([])

function fmt(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '-'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

async function loadModels() {
  loading.value = true
  try {
    const resp = await request<ModelsResp>('/api/admin/models')
    models.value = resp.models
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载模型列表失败')
  } finally {
    loading.value = false
  }
}

async function refreshModels() {
  refreshing.value = true
  try {
    const resp = await request<ModelsRefreshResp>('/api/admin/models/refresh', { method: 'POST' })
    const okCount = resp.updated.length
    const failCount = resp.failed.length
    const summary = `已同步 ${okCount} 个供应商`
    if (failCount > 0) {
      ElMessage.warning(
        `${summary}，${failCount} 个失败：${resp.failed.map((f) => `${f.provider_name}（${f.error}）`).join('；')}`,
      )
    } else {
      ElMessage.success(`${summary}，共 ${models.value.length} 个模型`)
    }
    await loadModels()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '刷新失败')
  } finally {
    refreshing.value = false
  }
}

async function deleteModel(row: ModelRow) {
  try {
    await ElMessageBox.confirm(
      `确定删除模型「${row.model_id}」（${row.provider_name}）吗？仅影响下拉选项，不影响已存在的路由规则。`,
      '删除模型',
      { type: 'warning', confirmButtonText: '删除', cancelButtonText: '取消' },
    )
  } catch {
    return // 用户取消
  }
  try {
    await request(`/api/admin/models/${row.id}`, { method: 'DELETE' })
    ElMessage.success('模型已删除')
    await loadModels()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '删除失败')
  }
}

/**
 * 启停切换：禁用后该 (供应商, 模型) 不再作为路由候选、不在 /v1/models 列出；
 * 路由规则与价格配置不受影响。el-switch 已乐观翻转 row.enabled，失败回滚。
 */
async function toggleEnabled(row: ModelRow) {
  const next = row.enabled
  togglingId.value = row.id
  try {
    await request(`/api/admin/models/${row.id}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enabled: next }),
    })
    ElMessage.success(`「${row.model_id}」已${next ? '启用' : '禁用'}`)
  } catch (e) {
    row.enabled = !next
    ElMessage.error(e instanceof Error ? e.message : '切换失败')
  } finally {
    togglingId.value = null
  }
}

/** 单模型测试：发最小 chat 请求验证上游可用性 */
async function testOne(row: ModelRow) {
  testingId.value = row.id
  try {
    const resp = await request<ModelTestResp>('/api/admin/models/test', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ model_id: row.id }),
    })
    const r = resp.results[0]
    if (r?.ok) {
      ElMessage.success(`「${row.model_id}」测试通过（${r.latency_ms} ms）`)
    } else {
      ElMessage.error(`「${row.model_id}」测试失败：${r?.error ?? '未知错误'}`)
    }
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '测试失败')
  } finally {
    testingId.value = null
  }
}

/** 一键测试全部模型：并发上限 5，结果弹窗展示明细 */
async function testAll() {
  testing.value = true
  try {
    const resp = await request<ModelTestResp>('/api/admin/models/test', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({}),
    })
    results.value = resp.results
    resultDialog.value = true
    if (resp.failed > 0) {
      ElMessage.warning(`测试完成：通过 ${resp.ok} / ${resp.ok + resp.failed}，失败明细见弹窗`)
    } else {
      ElMessage.success(`测试完成：${resp.ok} 个模型全部可用`)
    }
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '测试失败')
  } finally {
    testing.value = false
  }
}

onMounted(loadModels)
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <span class="title">模型库：来自供应商「测试连接」或手动刷新；路由规则与价格页可下拉选择。禁用的模型不参与路由、不在 /v1/models 列出</span>
      <div class="toolbar-actions">
        <el-button type="primary" :loading="testing" @click="testAll">
          <el-icon style="margin-right: 4px"><Cpu /></el-icon>
          测试全部
        </el-button>
        <el-button :loading="refreshing" @click="refreshModels">
          <el-icon style="margin-right: 4px"><Refresh /></el-icon>
          刷新模型库
        </el-button>
        <el-button :loading="loading" @click="loadModels">刷新列表</el-button>
      </div>
    </div>

    <el-card shadow="never" body-class="table-card">
      <el-table v-loading="loading" :data="models" stripe>
        <el-table-column prop="model_id" label="模型 ID" min-width="200" show-overflow-tooltip>
          <template #default="{ row }">
            <span class="mono" :class="{ disabled: !row.enabled }">{{ row.model_id }}</span>
          </template>
        </el-table-column>
        <el-table-column label="供应商" min-width="130" show-overflow-tooltip>
          <template #default="{ row }">{{ row.provider_name }}</template>
        </el-table-column>
        <el-table-column label="状态" width="90" align="center">
          <template #default="{ row }">
            <el-switch
              v-model="row.enabled"
              :loading="togglingId === row.id"
              :disabled="togglingId != null && togglingId !== row.id"
              @change="toggleEnabled(row)"
            />
          </template>
        </el-table-column>
        <el-table-column label="创建时间" min-width="150">
          <template #default="{ row }">{{ fmt(row.created_at) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="150" fixed="right">
          <template #default="{ row }">
            <el-button
              size="small"
              type="primary"
              plain
              :loading="testingId === row.id"
              @click="testOne(row)"
            >测试</el-button>
            <el-button size="small" type="danger" plain @click="deleteModel(row)">删除</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty description="暂无模型：请先在供应商页测试连接，或点击「刷新模型库」" />
        </template>
      </el-table>
    </el-card>

    <el-dialog v-model="resultDialog" title="模型测试结果" width="720px">
      <el-table :data="results" stripe max-height="480">
        <el-table-column prop="model" label="模型 ID" min-width="180" show-overflow-tooltip>
          <template #default="{ row }">
            <span class="mono">{{ row.model }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="provider_name" label="供应商" min-width="120" show-overflow-tooltip />
        <el-table-column label="状态" width="90">
          <template #default="{ row }">
            <el-tag v-if="row.ok" type="success" size="small">通过</el-tag>
            <el-tag v-else type="danger" size="small">失败</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="延迟" width="100" align="right">
          <template #default="{ row }">{{ row.ok ? `${row.latency_ms} ms` : '—' }}</template>
        </el-table-column>
        <el-table-column prop="error" label="错误信息" min-width="220" show-overflow-tooltip />
        <template #empty>
          <el-empty description="暂无测试结果" :image-size="80" />
        </template>
      </el-table>
    </el-dialog>
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

.toolbar-actions {
  display: flex;
  align-items: center;
  gap: 8px;
}

.title {
  font-size: 14px;
  color: #606266;
}

.mono {
  font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', Menlo, monospace;
  font-size: 12px;
  word-break: break-all;
}

.mono.disabled {
  color: #a8abb2;
  text-decoration: line-through;
}
</style>
