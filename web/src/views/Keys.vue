<template>
  <div class="keys-page">
    <el-card shadow="never" class="keys-card">
      <div class="toolbar">
        <p class="desc">API Key 用于调用 /v1/* 接口，创建后仅显示一次</p>
        <el-button type="primary" @click="openCreate">
          <el-icon class="btn-icon"><Plus /></el-icon>
          <span>新建 Key</span>
        </el-button>
      </div>

      <el-table :data="keys" v-loading="loading" stripe>
        <el-table-column prop="name" label="名称" min-width="140" show-overflow-tooltip />
        <el-table-column label="Key 前缀" min-width="170">
          <template #default="{ row }">
            <code class="key-prefix">{{ row.key_prefix }}</code>
          </template>
        </el-table-column>
        <el-table-column label="创建时间" width="170">
          <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
        </el-table-column>
        <el-table-column label="过期时间" width="110">
          <template #default="{ row }">
            <span v-if="row.expires_at">{{ formatTime(row.expires_at) }}</span>
            <el-tag v-else type="info" size="small" effect="plain">永久</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="最后使用" width="170">
          <template #default="{ row }">{{ row.last_used_at ? formatTime(row.last_used_at) : '-' }}</template>
        </el-table-column>
        <el-table-column label="操作" width="140" fixed="right">
          <template #default="{ row }">
            <el-tooltip
              v-if="!row.copyable"
              content="加密存储上线前的旧 Key 明文不可恢复，请删除后重建"
              placement="top"
            >
              <span>
                <el-button link type="primary" disabled>复制 Key</el-button>
              </span>
            </el-tooltip>
            <el-button
              v-else
              link
              type="primary"
              :loading="copyingId === row.id"
              @click="copyKeyAgain(row)"
            >复制 Key</el-button>
            <el-button type="danger" link @click="revokeKey(row)">吊销</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty description="暂无 API Key" :image-size="80" />
        </template>
      </el-table>
    </el-card>

    <el-dialog
      v-model="dialogVisible"
      :title="createdKey ? '创建成功' : '新建 API Key'"
      width="480px"
      :close-on-click-modal="false"
      :close-on-press-escape="!creating"
      @closed="resetDialog"
    >
      <el-form v-if="!createdKey" label-position="top" @submit.prevent>
        <el-form-item label="名称">
          <el-input
            v-model="form.name"
            placeholder="生产环境-主服务"
            maxlength="64"
            show-word-limit
            clearable
            @keyup.enter="submitCreate"
          />
        </el-form-item>
      </el-form>

      <div v-else class="success-view">
        <p class="key-label">API Key（仅显示一次）</p>
        <code class="plain-key">{{ createdKey.key }}</code>
        <p class="tip">请立即复制保存，关闭后无法再次查看</p>
      </div>

      <template #footer>
        <template v-if="createdKey">
          <el-button type="primary" @click="copyKey">
            <el-icon class="btn-icon"><CopyDocument /></el-icon>
            <span>复制</span>
          </el-button>
          <el-button @click="finishCreate">完成</el-button>
        </template>
        <template v-else>
          <el-button @click="dialogVisible = false">取消</el-button>
          <el-button type="primary" :loading="creating" @click="submitCreate">创建</el-button>
        </template>
      </template>
    </el-dialog>
  </div>
</template>

<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { ApiError, request } from '@/api/client'
import type { CreateKeyResp, KeyMeta, KeysResp } from '@/api/types'

const keys = ref<KeyMeta[]>([])
const loading = ref(false)

const dialogVisible = ref(false)
const creating = ref(false)
const createdKey = ref<CreateKeyResp | null>(null)
const copyingId = ref<number | null>(null)
const form = reactive({ name: '' })

async function loadKeys() {
  loading.value = true
  try {
    const resp = await request<KeysResp>('/api/keys')
    keys.value = resp.keys
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载失败，请稍后重试')
  } finally {
    loading.value = false
  }
}

function openCreate() {
  form.name = ''
  createdKey.value = null
  dialogVisible.value = true
}

function resetDialog() {
  form.name = ''
  createdKey.value = null
  creating.value = false
}

async function submitCreate() {
  const name = form.name.trim()
  if (!name) {
    ElMessage.warning('请输入 Key 名称')
    return
  }
  creating.value = true
  try {
    const resp = await request<CreateKeyResp>('/api/keys', {
      method: 'POST',
      body: JSON.stringify({ name }),
    })
    createdKey.value = resp
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '创建失败，请稍后重试')
  } finally {
    creating.value = false
  }
}

async function copyKey() {
  if (!createdKey.value) return
  try {
    await navigator.clipboard.writeText(createdKey.value.key)
    ElMessage.success('已复制')
  } catch {
    ElMessage.error('复制失败，请手动选择复制')
  }
}

async function copyKeyAgain(row: KeyMeta) {
  copyingId.value = row.id
  try {
    const resp = await request<{ key: string }>(`/api/keys/${row.id}/reveal`)
    await navigator.clipboard.writeText(resp.key)
    ElMessage.success(`已复制 ${row.name} 的完整 Key`)
  } catch (e) {
    // 兜底：列表页 copyable 过期时（如并发重建）仍给出明确指引
    if (e instanceof ApiError && e.message.includes('created before encrypted storage')) {
      ElMessage.error('该 Key 为加密存储上线前创建，明文不可恢复，请删除后重建')
      loadKeys()
    } else {
      ElMessage.error(e instanceof ApiError ? e.message : '复制失败，请稍后重试')
    }
  } finally {
    copyingId.value = null
  }
}

function finishCreate() {
  dialogVisible.value = false
  loadKeys()
}

async function revokeKey(row: KeyMeta) {
  try {
    await ElMessageBox.confirm(`吊销后该 Key 立即失效，确定吊销 ${row.name} 吗？`, '吊销 API Key', {
      type: 'warning',
      confirmButtonText: '吊销',
      cancelButtonText: '取消',
    })
  } catch {
    return // 用户取消
  }
  try {
    await request<{ ok: boolean }>(`/api/keys/${row.id}`, { method: 'DELETE' })
    ElMessage.success('Key 已吊销')
    loadKeys()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '吊销失败，请稍后重试')
  }
}

function formatTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

onMounted(loadKeys)
</script>

<style scoped>
.keys-page {
  padding: 16px;
}

.keys-card {
  border-radius: 8px;
}

.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 16px;
}

.desc {
  margin: 0;
  font-size: 13px;
  color: #909399;
}

.btn-icon {
  margin-right: 4px;
}

.key-prefix {
  font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', Menlo, monospace;
  font-size: 13px;
  background: #f5f7fa;
  border-radius: 4px;
  padding: 2px 6px;
  color: #303133;
}

.success-view {
  text-align: center;
  padding: 8px 0 4px;
}

.key-label {
  margin: 0 0 12px;
  font-size: 13px;
  color: #606266;
}

.plain-key {
  display: block;
  font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', Menlo, monospace;
  font-size: 18px;
  font-weight: 600;
  word-break: break-all;
  color: #303133;
  background: #f5f7fa;
  border: 1px solid #e4e7ed;
  border-radius: 6px;
  padding: 12px 16px;
}

.tip {
  margin: 12px 0 0;
  font-size: 13px;
  color: #e6a23c;
}
</style>
