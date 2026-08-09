<script setup lang="ts">
import { computed, onMounted, reactive, ref, watch } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import type { FormInstance, FormRules } from 'element-plus'
import { request } from '@/api/client'
import type { ModelRow, ModelsResp, ProviderRow, ProvidersResp, RouteRow, RoutesResp } from '@/api/types'

const loading = ref(false)
const routes = ref<RouteRow[]>([])
const providers = ref<ProviderRow[]>([])
const models = ref<ModelRow[]>([])

/** 模型库去重后的模型 id（下拉选项；仍可手输自定义模式） */
const modelOptions = computed(() => [...new Set(models.value.map((m) => m.model_id))].sort())

/** 供应商 id → 名称；找不到时回退显示 id */
function providerName(id: number): string {
  return providers.value.find((p) => p.id === id)?.name ?? String(id)
}

/** fallback 供应商 id 列表 → 逗号分隔的名称；空列表显示 '-' */
function fmtFallback(ids: number[]): string {
  if (!ids.length) return '-'
  return ids.map(providerName).join(', ')
}

async function loadData() {
  loading.value = true
  try {
    const [routesResp, providersResp, modelsResp] = await Promise.all([
      request<RoutesResp>('/api/admin/routes'),
      request<ProvidersResp>('/api/admin/providers'),
      request<ModelsResp>('/api/admin/models'),
    ])
    routes.value = [...routesResp.routes].sort((a, b) => a.priority - b.priority)
    providers.value = providersResp.providers
    models.value = modelsResp.models
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载路由规则失败')
  } finally {
    loading.value = false
  }
}

async function toggleEnabled(r: RouteRow) {
  const disable = r.enabled
  try {
    if (disable) {
      await ElMessageBox.confirm(
        `禁用后该路由将不再参与模型匹配，确定禁用「${r.model_pattern}」吗？`,
        '禁用路由',
        { type: 'warning', confirmButtonText: '禁用', cancelButtonText: '取消' },
      )
    }
  } catch {
    return // 用户取消
  }
  try {
    await request(`/api/admin/routes/${r.id}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enabled: !r.enabled }),
    })
    ElMessage.success(disable ? '路由已禁用' : '路由已启用')
    await loadData()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '操作失败')
  }
}

// —— 新建 / 编辑 ——
interface RouteForm {
  model_pattern: string
  provider_id: number | null
  priority: number | null
  fallback_ids: number[]
  upstream_model: string
  enabled: boolean
}

const dialogVisible = ref(false)
const editingId = ref<number | null>(null)
const saving = ref(false)
const formRef = ref<FormInstance>()
const form = reactive<RouteForm>({
  model_pattern: '',
  provider_id: null,
  priority: 100,
  fallback_ids: [],
  upstream_model: '',
  enabled: true,
})

const rules: FormRules<RouteForm> = {
  model_pattern: [
    { required: true, message: '请输入模型匹配模式', trigger: 'blur' },
    { max: 128, message: '模型匹配模式不能超过 128 个字符', trigger: 'blur' },
  ],
  provider_id: [{ required: true, message: '请选择供应商', trigger: 'change' }],
  priority: [{ required: true, message: '请输入优先级', trigger: 'change' }],
}

/** 下拉只列启用中的供应商；若当前选中的供应商已被禁用，也保留展示 */
const providerOptions = computed(() => {
  const enabled = providers.value.filter((p) => p.enabled)
  if (form.provider_id !== null && !enabled.some((p) => p.id === form.provider_id)) {
    const current = providers.value.find((p) => p.id === form.provider_id)
    if (current) return [...enabled, current]
  }
  return enabled
})

/** fallback 多选排除主供应商 */
const fallbackOptions = computed(() =>
  providerOptions.value.filter((p) => p.id !== form.provider_id),
)

/** 主供应商变更时，从已选 fallback 中剔除 */
watch(
  () => form.provider_id,
  (pid) => {
    if (pid !== null) {
      form.fallback_ids = form.fallback_ids.filter((id) => id !== pid)
    }
  },
)

function resetForm() {
  editingId.value = null
  form.model_pattern = ''
  form.provider_id = null
  form.priority = 100
  form.fallback_ids = []
  form.upstream_model = ''
  form.enabled = true
  formRef.value?.clearValidate()
}

function openCreate() {
  resetForm()
  dialogVisible.value = true
}

function openEdit(r: RouteRow) {
  editingId.value = r.id
  form.model_pattern = r.model_pattern
  form.provider_id = r.provider_id
  form.priority = r.priority
  form.fallback_ids = [...r.fallback_ids]
  form.upstream_model = r.upstream_model ?? ''
  form.enabled = r.enabled
  formRef.value?.clearValidate()
  dialogVisible.value = true
}

async function submit() {
  if (!formRef.value) return
  try {
    await formRef.value.validate()
  } catch {
    return // 校验未通过
  }
  if (form.provider_id === null || form.priority === null) return
  saving.value = true
  try {
    const body = {
      model_pattern: form.model_pattern.trim(),
      provider_id: form.provider_id,
      priority: form.priority,
      fallback_ids: form.fallback_ids,
      upstream_model: form.upstream_model.trim() || null,
      enabled: form.enabled,
    }
    if (editingId.value === null) {
      await request('/api/admin/routes', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      ElMessage.success('路由规则已创建')
    } else {
      await request(`/api/admin/routes/${editingId.value}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      ElMessage.success('路由规则已更新')
    }
    dialogVisible.value = false
    await loadData()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    saving.value = false
  }
}

onMounted(loadData)
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <span class="title">模型 → 供应商路由，按 priority 升序匹配；可配 fallback 链</span>
      <div class="toolbar-actions">
        <el-button :loading="loading" @click="loadData">刷新</el-button>
        <el-button type="primary" @click="openCreate">
          <el-icon style="margin-right: 4px"><Plus /></el-icon>新建
        </el-button>
      </div>
    </div>

    <el-card shadow="never" body-class="table-card">
      <el-table v-loading="loading" :data="routes" stripe>
        <el-table-column label="模型匹配" min-width="170" show-overflow-tooltip>
          <template #default="{ row }">
            <span class="mono">{{ row.model_pattern }}</span>
          </template>
        </el-table-column>
        <el-table-column label="供应商" min-width="120" show-overflow-tooltip>
          <template #default="{ row }">{{ providerName(row.provider_id) }}</template>
        </el-table-column>
        <el-table-column label="上游模型" min-width="140" show-overflow-tooltip>
          <template #default="{ row }">
            <span v-if="row.upstream_model" class="mono">{{ row.upstream_model }}</span>
            <span v-else style="color: #c0c4cc">透传</span>
          </template>
        </el-table-column>
        <el-table-column label="优先级" prop="priority" width="90" align="right" />
        <el-table-column label="Fallback" min-width="160" show-overflow-tooltip>
          <template #default="{ row }">{{ fmtFallback(row.fallback_ids) }}</template>
        </el-table-column>
        <el-table-column label="状态" width="80">
          <template #default="{ row }">
            <el-tag v-if="row.enabled" type="success" size="small">启用</el-tag>
            <el-tag v-else type="danger" size="small">禁用</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="操作" width="130" fixed="right">
          <template #default="{ row }">
            <el-button size="small" @click="openEdit(row)">编辑</el-button>
            <el-button
              v-if="row.enabled"
              size="small"
              type="danger"
              plain
              @click="toggleEnabled(row)"
            >禁用</el-button>
            <el-button
              v-else
              size="small"
              type="success"
              plain
              @click="toggleEnabled(row)"
            >启用</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty description="暂无路由规则" />
        </template>
      </el-table>
    </el-card>

    <el-dialog
      v-model="dialogVisible"
      :title="editingId === null ? '新建路由规则' : '编辑路由规则'"
      width="520px"
      :close-on-click-modal="false"
      @closed="resetForm"
    >
      <el-form ref="formRef" :model="form" :rules="rules" label-width="90px">
        <el-form-item label="模型匹配" prop="model_pattern">
          <el-select
            v-model="form.model_pattern"
            filterable
            allow-create
            default-first-option
            clearable
            placeholder="选择模型库中的模型，或输入自定义模式（支持 * 通配）"
            style="width: 100%"
          >
            <el-option v-for="m in modelOptions" :key="m" :label="m" :value="m" />
          </el-select>
        </el-form-item>
        <el-form-item label="供应商" prop="provider_id">
          <el-select v-model="form.provider_id" placeholder="选择供应商" style="width: 100%">
            <el-option v-for="p in providerOptions" :key="p.id" :label="p.name" :value="p.id" />
          </el-select>
        </el-form-item>
        <el-form-item label="上游模型" prop="upstream_model">
          <el-input
            v-model="form.upstream_model"
            maxlength="128"
            placeholder="留空 = 透传客户端模型名；填写后客户端请求该模型将改写为上游实际模型名（如 gpt-4 → claude-3-5-sonnet）"
          />
        </el-form-item>
        <el-form-item label="优先级" prop="priority">
          <el-input-number v-model="form.priority" :min="0" :max="999999" />
        </el-form-item>
        <el-form-item label="Fallback" prop="fallback_ids">
          <el-select
            v-model="form.fallback_ids"
            multiple
            collapse-tags
            placeholder="选择备用供应商（可多选，自动排除主供应商）"
            style="width: 100%"
          >
            <el-option v-for="p in fallbackOptions" :key="p.id" :label="p.name" :value="p.id" />
          </el-select>
        </el-form-item>
        <el-form-item label="启用">
          <el-switch v-model="form.enabled" />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="submit">保存</el-button>
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

.toolbar-actions {
  display: flex;
  align-items: center;
  gap: 8px;
}

.title {
  font-size: 16px;
  font-weight: 600;
  color: #303133;
}

.mono {
  font-family: 'JetBrains Mono', Consolas, 'Courier New', monospace;
  font-size: 13px;
}
</style>
