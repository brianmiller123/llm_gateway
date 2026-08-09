<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import type { FormInstance, FormRules } from 'element-plus'
import { request } from '@/api/client'
import type { ProviderRow, ProvidersResp, TestConnectionResp } from '@/api/types'

const loading = ref(false)
const providers = ref<ProviderRow[]>([])

/** 格式化 ISO 时间戳为 YYYY-MM-DD HH:mm，空值显示 '-' */
function fmt(iso: string | null): string {
  if (!iso) return '-'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '-'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

/** 毫秒超时 → 秒展示 */
function fmtTimeout(ms: number): string {
  const s = ms / 1000
  return `${Number.isInteger(s) ? s : s.toFixed(1)} 秒`
}

async function loadProviders() {
  loading.value = true
  try {
    const resp = await request<ProvidersResp>('/api/admin/providers')
    providers.value = resp.providers
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载供应商列表失败')
  } finally {
    loading.value = false
  }
}

async function toggleProvider(p: ProviderRow) {
  const disable = p.enabled
  try {
    if (disable) {
      await ElMessageBox.confirm(
        '禁用后该供应商的路由将失效，确定禁用吗？',
        '禁用供应商',
        { type: 'warning', confirmButtonText: '禁用', cancelButtonText: '取消' },
      )
    }
  } catch {
    return // 用户取消
  }
  try {
    await request(`/api/admin/providers/${p.id}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enabled: !disable }),
    })
    ElMessage.success(disable ? '供应商已禁用' : '供应商已启用')
    await loadProviders()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '操作失败')
  }
}

// —— 测试连接（拉取供应商模型列表） ——
const testing = ref(false)
const testModels = ref<string[] | null>(null)
const testError = ref('')

async function testConnection() {
  if (!formData.base_url.trim()) {
    ElMessage.warning('请先填写 Base URL')
    return
  }
  testing.value = true
  testModels.value = null
  testError.value = ''
  try {
    const resp = await request<TestConnectionResp>('/api/admin/providers/test-connection', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        base_url: formData.base_url.trim(),
        api_key: formData.api_key.trim() || null,
        // 编辑已有供应商时传 id：api_key 留空则后端回退用已保存的 Key
        provider_id: formData.id,
      }),
    })
    testModels.value = resp.models
    if (resp.count === 0) {
      ElMessage.warning('连接成功，但未获取到模型（上游 /models 返回空列表）')
    } else {
      ElMessage.success(`连接成功，获取到 ${resp.count} 个模型`)
    }
  } catch (e) {
    testError.value = e instanceof Error ? e.message : '连接失败'
    ElMessage.error(testError.value)
  } finally {
    testing.value = false
  }
}

// —— 新建 / 编辑 ——
interface ProviderForm {
  id: number | null
  name: string
  base_url: string
  api_key: string
  timeout_ms: number
  enabled: boolean
}

const dialogVisible = ref(false)
const editing = ref(false)
const saving = ref(false)
const formRef = ref<FormInstance>()
const formData = reactive<ProviderForm>({
  id: null,
  name: '',
  base_url: '',
  api_key: '',
  timeout_ms: 120000,
  enabled: true,
})

const rules = computed<FormRules>(() => ({
  name: [
    { required: true, message: '请输入供应商名称', trigger: 'blur' },
    { max: 64, message: '名称不能超过 64 个字符', trigger: 'blur' },
  ],
  base_url: [{ required: true, message: '请输入 Base URL', trigger: 'blur' }],
  // 编辑时 api_key 可留空（留空则不修改）
  api_key: editing.value
    ? []
    : [{ required: true, message: '请输入 API Key', trigger: 'blur' }],
}))

const dialogTitle = computed(() => (editing.value ? '编辑供应商' : '新建供应商'))

function resetForm() {
  formData.id = null
  formData.name = ''
  formData.base_url = ''
  formData.api_key = ''
  formData.timeout_ms = 120000
  formData.enabled = true
}

function openCreate() {
  editing.value = false
  resetForm()
  dialogVisible.value = true
}

function openEdit(p: ProviderRow) {
  editing.value = true
  formData.id = p.id
  formData.name = p.name
  formData.base_url = p.base_url
  formData.api_key = ''
  formData.timeout_ms = p.timeout_ms
  formData.enabled = p.enabled
  dialogVisible.value = true
}

function onDialogClosed() {
  resetForm()
  formRef.value?.clearValidate()
  testModels.value = null
  testError.value = ''
}

async function doSave() {
  const form = formRef.value
  if (!form) return
  try {
    await form.validate()
  } catch {
    return // 校验未通过
  }
  saving.value = true
  try {
    const payload: Record<string, unknown> = {
      name: formData.name.trim(),
      base_url: formData.base_url.trim(),
      timeout_ms: formData.timeout_ms,
      enabled: formData.enabled,
    }
    // 编辑时留空表示不修改 Key
    if (formData.api_key.trim() !== '') {
      payload.api_key = formData.api_key.trim()
    }
    if (editing.value && formData.id !== null) {
      await request(`/api/admin/providers/${formData.id}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      })
      ElMessage.success('供应商已更新')
    } else {
      await request('/api/admin/providers', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      })
      ElMessage.success('供应商已创建')
    }
    dialogVisible.value = false
    await loadProviders()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    saving.value = false
  }
}

onMounted(loadProviders)
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <span class="title">上游模型供应商，Key 加密存储；禁用后对应路由自动失效</span>
      <el-button type="primary" @click="openCreate">
        <el-icon class="mr-icon"><Plus /></el-icon>
        新建
      </el-button>
    </div>

    <el-card shadow="never" body-class="table-card">
      <el-table v-loading="loading" :data="providers" stripe>
        <el-table-column prop="id" label="ID" width="70" />
        <el-table-column prop="name" label="名称" min-width="130" show-overflow-tooltip />
        <el-table-column label="类型" width="110">
          <template #default="{ row }">
            <el-tag type="info" size="small">{{ row.api_type }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="Base URL" min-width="220" show-overflow-tooltip>
          <template #default="{ row }">
            <span class="mono">{{ row.base_url }}</span>
          </template>
        </el-table-column>
        <el-table-column label="API Key" min-width="170" show-overflow-tooltip>
          <template #default="{ row }">
            <span class="mono">{{ row.api_key_encrypted }}</span>
          </template>
        </el-table-column>
        <el-table-column label="超时" width="90">
          <template #default="{ row }">{{ fmtTimeout(row.timeout_ms) }}</template>
        </el-table-column>
        <el-table-column label="状态" width="80">
          <template #default="{ row }">
            <el-tag v-if="row.enabled" type="success" size="small">启用</el-tag>
            <el-tag v-else type="info" size="small">禁用</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="创建时间" min-width="150">
          <template #default="{ row }">{{ fmt(row.created_at) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="130" fixed="right">
          <template #default="{ row }">
            <el-button link type="primary" @click="openEdit(row)">编辑</el-button>
            <el-button
              v-if="row.enabled"
              link
              type="danger"
              @click="toggleProvider(row)"
            >禁用</el-button>
            <el-button
              v-else
              link
              type="success"
              @click="toggleProvider(row)"
            >启用</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty description="暂无供应商" />
        </template>
      </el-table>
    </el-card>

    <el-dialog
      v-model="dialogVisible"
      :title="dialogTitle"
      width="520px"
      :close-on-click-modal="false"
      @closed="onDialogClosed"
    >
      <el-form ref="formRef" :model="formData" :rules="rules" label-width="90px">
        <el-form-item label="名称" prop="name">
          <el-input
            v-model="formData.name"
            maxlength="64"
            placeholder="例如：OpenAI"
            clearable
          />
        </el-form-item>
        <el-form-item label="Base URL" prop="base_url">
          <el-input
            v-model="formData.base_url"
            placeholder="https://api.example.com/v1"
            clearable
          />
        </el-form-item>
        <el-form-item label="API Key" prop="api_key">
          <el-input
            v-model="formData.api_key"
            type="password"
            show-password
            :placeholder="editing ? '留空则不修改' : 'sk-... 仅提交时加密存储'"
          />
        </el-form-item>
        <el-form-item label="超时(ms)" prop="timeout_ms">
          <el-input-number
            v-model="formData.timeout_ms"
            :min="1000"
            :step="1000"
            :step-strictly="true"
          />
        </el-form-item>
        <el-form-item label="启用">
          <el-switch v-model="formData.enabled" />
        </el-form-item>
        <el-form-item label="模型探测">
          <div class="probe">
            <el-button
              :loading="testing"
              :disabled="!formData.base_url.trim()"
              @click="testConnection"
            >测试连接并获取模型</el-button>
            <div v-if="testError" class="probe-error mono">{{ testError }}</div>
            <div v-if="testModels" class="probe-models">
              <el-tag
                v-for="m in testModels"
                :key="m"
                size="small"
                class="probe-tag"
              >{{ m }}</el-tag>
              <span v-if="testModels.length === 0" class="probe-empty">上游 /models 返回空列表</span>
            </div>
          </div>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="doSave">保存</el-button>
      </template>
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

.title {
  font-size: 14px;
  color: #606266;
}

.mr-icon {
  margin-right: 4px;
}

.mono {
  font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', Menlo, monospace;
  font-size: 12px;
  word-break: break-all;
}

.probe {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 8px;
  width: 100%;
}

.probe-error {
  color: #f56c6c;
  max-width: 100%;
}

.probe-models {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.probe-tag {
  font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', Menlo, monospace;
}

.probe-empty {
  font-size: 12px;
  color: #909399;
}
</style>
