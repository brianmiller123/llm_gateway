<template>
  <div class="page">
    <div class="toolbar">
      <div>
        <h2 class="page-title">高级请求配置</h2>
        <p class="page-desc">
          定义 extra_body，网关每次请求将其深合并进转发给上游的请求体（配置值覆盖客户端同名叶键，客户端独有字段保留）。
          解决 vLLM / SGLang 等推理引擎上 Qwen3 思考参数（chat_template_kwargs.thinking / reasoning_effort）无法透传的问题，
          亦兼容 top_k、repetition_penalty 等后端特有参数。
        </p>
      </div>
      <div class="switch-box">
        <span class="switch-label">extra_body 合并</span>
        <el-switch v-model="globalEnabled" :loading="switchSaving" @change="saveSwitch" />
      </div>
    </div>

    <el-alert
      v-if="!globalEnabled"
      type="warning"
      :closable="false"
      show-icon
      title="合并已全局停用：配置保留但不会注入请求体，恢复开关即可生效"
    />

    <el-tabs v-model="activeTab">
      <el-tab-pane label="按渠道（供应商）" name="provider">
        <el-card shadow="never">
          <el-table v-loading="loadingProviders" :data="providers" stripe>
            <el-table-column prop="id" label="ID" width="60" />
            <el-table-column prop="name" label="供应商" min-width="120" show-overflow-tooltip />
            <el-table-column label="类型" width="130">
              <template #default="{ row }">
                <el-tag type="info" size="small">{{ row.api_type }}</el-tag>
              </template>
            </el-table-column>
            <el-table-column label="extra_body" min-width="280">
              <template #default="{ row }">
                <span v-if="hasConfig(row)" class="mono">{{ compact(row) }}</span>
                <el-tag v-else type="info" size="small" effect="plain">未配置</el-tag>
              </template>
            </el-table-column>
            <el-table-column label="操作" width="90" fixed="right">
              <template #default="{ row }">
                <el-button link type="primary" @click="openEdit('provider', row)">配置</el-button>
              </template>
            </el-table-column>
          </el-table>
        </el-card>
      </el-tab-pane>

      <el-tab-pane label="按模型（路由规则）" name="route">
        <el-card shadow="never">
          <el-table v-loading="loadingRoutes" :data="routes" stripe>
            <el-table-column prop="model_pattern" label="模型 Pattern" min-width="150" show-overflow-tooltip />
            <el-table-column label="主供应商" min-width="110">
              <template #default="{ row }">{{ providerName(row.provider_id) }}</template>
            </el-table-column>
            <el-table-column prop="upstream_model" label="上游模型映射" min-width="120" show-overflow-tooltip>
              <template #default="{ row }">{{ row.upstream_model ?? '透传' }}</template>
            </el-table-column>
            <el-table-column label="extra_body" min-width="260">
              <template #default="{ row }">
                <span v-if="hasConfig(row)" class="mono">{{ compact(row) }}</span>
                <el-tag v-else type="info" size="small" effect="plain">未配置</el-tag>
              </template>
            </el-table-column>
            <el-table-column label="操作" width="90" fixed="right">
              <template #default="{ row }">
                <el-button link type="primary" @click="openEdit('route', row)">配置</el-button>
              </template>
            </el-table-column>
          </el-table>
        </el-card>
      </el-tab-pane>
    </el-tabs>

    <el-card shadow="never">
      <template #header>
        <span>最终请求体预览</span>
        <span class="hint">合并顺序：基础请求 ← 渠道 extra_body ← 模型（路由）extra_body，与网关热路径一致</span>
      </template>
      <div class="preview-grid">
        <div class="preview-col">
          <div class="col-label">基础请求（示例，可编辑）</div>
          <el-input
            v-model="baseText"
            type="textarea"
            :rows="10"
            class="json-input"
            spellcheck="false"
          />
          <div v-if="baseError" class="json-error">{{ baseError }}</div>
        </div>
        <div class="preview-col">
          <div class="col-label">
            组合条件
            <el-select v-model="previewProvider" placeholder="选择渠道" size="small" style="width: 170px; margin-left: 8px">
              <el-option v-for="p in providers" :key="p.id" :label="p.name" :value="p.id" />
            </el-select>
            <el-select v-model="previewRoute" placeholder="选择路由（模型）" size="small" style="width: 190px; margin-left: 8px">
              <el-option
                v-for="r in routes"
                :key="r.id"
                :label="`${r.model_pattern} → ${providerName(r.provider_id)}`"
                :value="r.id"
              />
            </el-select>
          </div>
          <el-input
            v-model="mergedText"
            type="textarea"
            :rows="10"
            class="json-input"
            readonly
            spellcheck="false"
          />
          <div class="hint">readonly：模型名（model）与流式语义（stream / stream_options）由网关管理，配置中的这些顶层字段被忽略</div>
        </div>
      </div>
    </el-card>

    <el-dialog
      v-model="dialogVisible"
      :title="dialogTitle"
      width="640px"
      :close-on-click-modal="false"
    >
      <div class="edit-scope">
        <span class="scope-label">{{ editKind === 'provider' ? '渠道' : '模型' }}：</span>
        <span class="scope-name">{{ editTarget }}</span>
        <span class="scope-effective" v-if="editKind === 'provider'">该渠道下未单独配置模型级覆盖的请求生效</span>
        <span class="scope-effective" v-else>模型级配置覆盖渠道级同名叶键</span>
      </div>
      <div class="presets">
        <span class="preset-label">快捷预设：</span>
        <el-button size="small" @click="applyPreset('thinking_max')">Qwen3 深度思考 max</el-button>
        <el-button size="small" @click="applyPreset('thinking_on')">Qwen3 思考开启</el-button>
        <el-button size="small" @click="applyPreset('thinking_off')">Qwen3 思考关闭</el-button>
        <el-button size="small" @click="applyPreset('enable_thinking_on')">enable_thinking 开</el-button>
        <el-button size="small" @click="applyPreset('enable_thinking_off')">enable_thinking 关</el-button>
        <el-button size="small" @click="applyPreset('top_k')">top_k 采样</el-button>
        <el-button size="small" @click="applyPreset('repetition_penalty')">repetition_penalty</el-button>
        <el-button size="small" @click="clearEditor">清空</el-button>
      </div>
      <el-input
        v-model="editorText"
        type="textarea"
        :rows="14"
        class="json-input"
        placeholder='{ "chat_template_kwargs": { "thinking": true, "reasoning_effort": "max" } }'
        spellcheck="false"
        @input="editorError = validateJson(editorText)"
      />
      <div v-if="editorError" class="json-error">{{ editorError }}</div>
      <div class="hint">必须是 JSON 对象；空对象 {} 或清空 = 不注入。model / stream / stream_options 由网关管理，不可配置</div>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" :disabled="!!editorError" @click="saveEditor">保存</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { request } from '@/api/client'
import type { ExtraBodySettingsResp, ProviderRow, ProvidersResp, RouteRow, RoutesResp } from '@/api/types'

/** 网关管理的顶层字段（后端同样拒绝/忽略，前端预览保持一致） */
const MANAGED_KEYS = ['model', 'stream', 'stream_options']

const loadingProviders = ref(false)
const loadingRoutes = ref(false)
const providers = ref<ProviderRow[]>([])
const routes = ref<RouteRow[]>([])
const activeTab = ref('provider')

// —— 全局开关 ——
const globalEnabled = ref(true)
const switchSaving = ref(false)

async function loadSettings() {
  try {
    const resp = await request<ExtraBodySettingsResp>('/api/admin/settings/extra-body')
    globalEnabled.value = resp.enabled
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载全局开关失败')
  }
}

async function saveSwitch(v: string | number | boolean) {
  switchSaving.value = true
  try {
    await request('/api/admin/settings/extra-body', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enabled: Boolean(v) }),
    })
    ElMessage.success(Boolean(v) ? 'extra_body 合并已启用' : 'extra_body 合并已停用（配置保留）')
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    switchSaving.value = false
  }
}

// —— 列表加载 ——
async function loadAll() {
  loadingProviders.value = true
  loadingRoutes.value = true
  try {
    const [p, r] = await Promise.all([
      request<ProvidersResp>('/api/admin/providers'),
      request<RoutesResp>('/api/admin/routes'),
    ])
    providers.value = p.providers
    routes.value = r.routes
    if (previewProvider.value === null && p.providers.length > 0) previewProvider.value = p.providers[0].id
    if (previewRoute.value === null && r.routes.length > 0) previewRoute.value = r.routes[0].id
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载配置失败')
  } finally {
    loadingProviders.value = false
    loadingRoutes.value = false
  }
}

function providerName(id: number): string {
  return providers.value.find((p) => p.id === id)?.name ?? `#${id}`
}

function hasConfig(row: { extra_body?: Record<string, unknown> }): boolean {
  return !!row.extra_body && Object.keys(row.extra_body).length > 0
}

function compact(row: { extra_body?: Record<string, unknown> }): string {
  const s = JSON.stringify(row.extra_body ?? {})
  return s.length > 96 ? `${s.slice(0, 96)}…` : s
}

// —— 编辑器 ——
const dialogVisible = ref(false)
const saving = ref(false)
const editorText = ref('')
const editorError = ref('')
const editKind = ref<'provider' | 'route'>('provider')
const editId = ref<number | null>(null)
const editTarget = ref('')

const dialogTitle = computed(() =>
  editKind.value === 'provider' ? '配置渠道 extra_body' : '配置模型 extra_body（覆盖渠道）',
)

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

/** 与后端 merge_deep 同语义：对象递归合并、标量/数组替换、保留字段跳过 */
function mergeDeep(dst: Record<string, unknown>, src: Record<string, unknown>) {
  for (const [k, v] of Object.entries(src)) {
    if (MANAGED_KEYS.includes(k)) continue
    const dv = dst[k]
    if (isPlainObject(dv) && isPlainObject(v)) {
      mergeDeep(dv, v)
    } else {
      dst[k] = v
    }
  }
}

/** 基础请求校验：仅语法 + 对象（model/stream 等在基础请求中是合法字段） */
function validateBaseJson(text: string): string {
  const t = text.trim()
  if (t === '') return ''
  try {
    const v = JSON.parse(t)
    if (!isPlainObject(v)) return '必须是 JSON 对象（{ ... }）'
    return ''
  } catch (e) {
    return `JSON 格式错误：${e instanceof Error ? e.message : String(e)}`
  }
}

/** extra_body 校验：语法 + 对象 + 托管字段 + 大小上限 */
function validateJson(text: string): string {
  const baseErr = validateBaseJson(text)
  if (baseErr || text.trim() === '') return baseErr
  const v = JSON.parse(text)
  for (const k of Object.keys(v)) {
    if (MANAGED_KEYS.includes(k)) return `顶层字段 "${k}" 由网关管理，不可配置`
  }
  if (text.length > 8192) return '配置过大（上限 8KB）'
  return ''
}

function openEdit(kind: 'provider' | 'route', row: ProviderRow | RouteRow) {
  editKind.value = kind
  editId.value = row.id
  editTarget.value =
    kind === 'provider' ? (row as ProviderRow).name : (row as RouteRow).model_pattern
  editorText.value = hasConfig(row) ? JSON.stringify(row.extra_body, null, 2) : ''
  editorError.value = ''
  dialogVisible.value = true
}

function clearEditor() {
  editorText.value = ''
  editorError.value = ''
}

const PRESETS: Record<string, Record<string, unknown>> = {
  thinking_max: { chat_template_kwargs: { thinking: true, reasoning_effort: 'max' } },
  thinking_on: { chat_template_kwargs: { thinking: true } },
  thinking_off: { chat_template_kwargs: { thinking: false } },
  enable_thinking_on: { enable_thinking: true },
  enable_thinking_off: { enable_thinking: false },
  top_k: { top_k: 20 },
  repetition_penalty: { repetition_penalty: 1.05 },
}

function applyPreset(key: string) {
  const preset = PRESETS[key]
  if (!preset) return
  const current =
    editorError.value === '' && editorText.value.trim() !== '' ? JSON.parse(editorText.value) : {}
  const target = isPlainObject(current) ? current : {}
  mergeDeep(target, JSON.parse(JSON.stringify(preset)))
  editorText.value = JSON.stringify(target, null, 2)
  editorError.value = ''
}

async function saveEditor() {
  if (editorError.value) return
  let parsed: Record<string, unknown>
  if (editorText.value.trim() === '') {
    parsed = {}
  } else {
    try {
      parsed = JSON.parse(editorText.value)
    } catch {
      return
    }
  }
  if (!isPlainObject(parsed)) return
  saving.value = true
  try {
    const path =
      editKind.value === 'provider' ? `/api/admin/providers/${editId.value}` : `/api/admin/routes/${editId.value}`
    await request(path, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ extra_body: parsed }),
    })
    ElMessage.success('已保存并生效（热加载，无需重启）')
    dialogVisible.value = false
    await loadAll()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    saving.value = false
  }
}

// —— 预览 ——
const baseText = ref(
  JSON.stringify(
    {
      model: 'qwen3-32b',
      messages: [{ role: 'user', content: '你好' }],
      temperature: 0.7,
    },
    null,
    2,
  ),
)
const baseError = ref('')
const previewProvider = ref<number | null>(null)
const previewRoute = ref<number | null>(null)
const mergedText = computed(() => {
  const baseErr = validateBaseJson(baseText.value)
  if (baseErr || baseText.value.trim() === '') return `// ${baseErr || '基础请求为空'}`
  const merged: Record<string, unknown> = JSON.parse(baseText.value)
  const p = providers.value.find((x) => x.id === previewProvider.value)
  const r = routes.value.find((x) => x.id === previewRoute.value)
  if (p && hasConfig(p)) mergeDeep(merged, JSON.parse(JSON.stringify(p.extra_body)))
  if (r && hasConfig(r)) mergeDeep(merged, JSON.parse(JSON.stringify(r.extra_body)))
  return JSON.stringify(merged, null, 2)
})

onMounted(() => {
  loadSettings()
  loadAll()
})
</script>

<style scoped>
.page {
  padding: 20px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.toolbar {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
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
  max-width: 900px;
  line-height: 1.6;
}
.switch-box {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}
.switch-label {
  font-size: 13px;
  color: #606266;
}
.mono {
  font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', Menlo, monospace;
  font-size: 12px;
  word-break: break-all;
}
.hint {
  margin-left: 10px;
  font-size: 12px;
  color: #909399;
}
.preview-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 16px;
}
.preview-col {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.col-label {
  font-size: 13px;
  color: #606266;
  display: flex;
  align-items: center;
  min-height: 24px;
}
.json-input :deep(textarea) {
  font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', Menlo, monospace;
  font-size: 12px;
  line-height: 1.5;
}
.json-error {
  color: #f56c6c;
  font-size: 12px;
  font-family: 'SFMono-Regular', Consolas, Menlo, monospace;
}
.edit-scope {
  display: flex;
  align-items: baseline;
  gap: 8px;
  margin-bottom: 10px;
  flex-wrap: wrap;
}
.scope-label {
  font-size: 13px;
  color: #606266;
}
.scope-name {
  font-weight: 600;
  color: #303133;
}
.scope-effective {
  font-size: 12px;
  color: #909399;
}
.presets {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
  margin-bottom: 10px;
}
.preset-label {
  font-size: 13px;
  color: #606266;
}
</style>
