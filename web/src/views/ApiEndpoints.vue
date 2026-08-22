<template>
  <div class="page">
    <div class="toolbar">
      <div>
        <h2 class="page-title">API 端点管理</h2>
        <p class="page-desc">
          查看 Response API 与 Anthropic Messages API 的调用地址、独立启用/停用各 API（停用后立即返回 503 明确提示，
          不影响另一 API），并控制用户页面是否展示对应调用地址。测试区发送真实代理管线请求，展示状态码 / 耗时 / 响应内容 / 错误。
        </p>
      </div>
      <div class="switch-box">
        <span class="switch-label">保存后立即生效，无需重启</span>
      </div>
    </div>

    <div v-if="endpointNote" class="endpoint-note">
      <el-alert type="info" :closable="false" show-icon :title="endpointNote" />
    </div>

    <div class="cards">
      <el-card v-for="item in cardList" :key="item.key" shadow="never" class="api-card">
        <template #header>
          <div class="card-head">
            <span class="card-title">{{ item.title }}</span>
            <el-tag :type="item.info.enabled ? 'success' : 'danger'" effect="dark" size="small">
              {{ item.info.enabled ? '已启用' : '已停用' }}
            </el-tag>
          </div>
        </template>

        <div class="addr-row">
          <span class="addr-label">调用地址</span>
          <code class="mono addr">{{ item.info.address }}</code>
          <el-button link type="primary" :icon="CopyDocument" @click="copyAddr(item.info.address)">
            复制
          </el-button>
        </div>

        <div class="switch-row">
          <span class="switch-label">启用 {{ item.short }} API</span>
          <el-switch v-model="item.info.enabled" :loading="saving" />
          <span class="hint">停用后请求立即返回 503 明确提示，另一 API 不受影响</span>
        </div>

        <div class="switch-row">
          <span class="switch-label">用户页面展示地址</span>
          <el-switch v-model="item.info.visible" :loading="saving" />
          <span class="hint">隐藏后用户控制台不再展示该 API 调用地址</span>
        </div>

        <div class="actions">
          <el-button type="primary" :icon="Check" :loading="saving" @click="save">
            保存配置
          </el-button>
        </div>
      </el-card>
    </div>

    <el-card shadow="never" class="test-card">
      <template #header><span>API 测试</span></template>

      <div class="test-form">
        <div class="test-row">
          <span class="switch-label">测试 API</span>
          <el-radio-group v-model="test.api" @change="onApiChanged">
            <el-radio-button value="responses">Response API（/v1/responses）</el-radio-button>
            <el-radio-button value="messages">Anthropic Messages（/v1/messages）</el-radio-button>
          </el-radio-group>
          <span class="switch-label indent">流式</span>
          <el-switch v-model="test.stream" />
        </div>
        <div class="test-row">
          <span class="switch-label">请求体（JSON）</span>
          <el-input
            v-model="test.bodyText"
            type="textarea"
            :rows="8"
            class="mono body-input"
            placeholder='{"model": "...", "input": "Hello"}'
            @change="onBodyEdited"
          />
        </div>
        <div class="test-row">
          <el-button type="primary" :icon="Promotion" :loading="testing" :disabled="bodyInvalid" @click="runTest">
            发送测试请求
          </el-button>
          <span v-if="bodyInvalid" class="hint error-hint">请求体不是合法 JSON</span>
        </div>
      </div>

      <div v-if="testResult" class="test-result">
        <el-divider />
        <div class="result-head">
          <el-tag :type="testResult.ok ? 'success' : 'danger'" effect="dark">
            HTTP {{ testResult.status_code }}
          </el-tag>
          <el-tag type="info" effect="plain">耗时 {{ testResult.latency_ms }} ms</el-tag>
          <el-tag v-if="testResult.content_type" type="info" effect="plain">{{ testResult.content_type }}</el-tag>
        </div>
        <el-alert
          v-if="testResult.error"
          type="error"
          :closable="false"
          show-icon
          :title="testResult.error"
          class="result-error"
        />
        <pre v-if="testResult.body_preview" class="mono result-body">{{ testResult.body_preview }}</pre>
        <el-empty v-else-if="!testResult.error" description="无响应体" :image-size="60" />
      </div>
    </el-card>

    <el-card shadow="never" class="history-card">
      <template #header>
        <div class="card-head">
          <span>最近测试结果</span>
          <el-button link type="primary" :icon="Refresh" :loading="loadingHistory" @click="loadHistory">刷新</el-button>
        </div>
      </template>
      <el-table v-loading="loadingHistory" :data="history" stripe size="small">
        <el-table-column label="时间" width="170">
          <template #default="{ row }">{{ fmtTime(row.created_at) }}</template>
        </el-table-column>
        <el-table-column label="API" width="110">
          <template #default="{ row }">
            <el-tag size="small" :type="row.api === 'responses' ? 'primary' : 'warning'" effect="plain">
              {{ row.api === 'responses' ? 'Responses' : 'Messages' }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column prop="model" label="模型" min-width="130" show-overflow-tooltip />
        <el-table-column label="流式" width="70">
          <template #default="{ row }">{{ row.stream ? '是' : '否' }}</template>
        </el-table-column>
        <el-table-column label="状态码" width="90" align="center">
          <template #default="{ row }">
            <el-tag size="small" :type="row.ok ? 'success' : 'danger'" effect="plain">{{ row.status_code }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column prop="latency_ms" label="耗时(ms)" width="100" align="right" />
        <el-table-column prop="error" label="错误" min-width="180" show-overflow-tooltip />
      </el-table>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { Check, CopyDocument, Promotion, Refresh } from '@element-plus/icons-vue'
import type { ApiEndpointsResp, ApiTestResp, ApiTestResult } from '@/api/types'
import { api } from '@/api/client'

const loading = ref(false)
const saving = ref(false)
const testing = ref(false)
const loadingHistory = ref(false)

interface CardItem {
  key: 'responses' | 'messages'
  title: string
  short: string
  info: { path: string; address: string; enabled: boolean; visible: boolean }
}

const resp = reactive({
  public_base: '',
  public_base_override: false,
  responses: { path: '/v1/responses', address: '', enabled: true, visible: true },
  messages: { path: '/v1/messages', address: '', enabled: true, visible: true },
})

const cardList = computed<CardItem[]>(() => [
  { key: 'responses', title: 'Response API', short: 'Response', info: resp.responses },
  { key: 'messages', title: 'Anthropic Messages API', short: 'Messages', info: resp.messages },
])

const endpointNote = computed(() =>
  resp.public_base_override
    ? `公开基址由 GATEWAY_PUBLIC_BASE_URL 指定：${resp.public_base}`
    : `公开基址按请求 Host 推导：${resp.public_base}（反代部署可设 GATEWAY_PUBLIC_BASE_URL 覆盖）`,
)

async function load() {
  loading.value = true
  try {
    const r = await api.apiEndpoints()
    resp.public_base = r.public_base
    resp.public_base_override = r.public_base_override
    resp.responses = { ...r.responses }
    resp.messages = { ...r.messages }
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载端点配置失败')
  } finally {
    loading.value = false
  }
}

async function save() {
  saving.value = true
  try {
    const r = await api.putApiEndpoints({
      responses_enabled: resp.responses.enabled,
      responses_visible: resp.responses.visible,
      messages_enabled: resp.messages.enabled,
      messages_visible: resp.messages.visible,
    })
    resp.responses = { ...r.responses }
    resp.messages = { ...r.messages }
    ElMessage.success('端点配置已保存并生效')
  } finally {
    saving.value = false
  }
}

async function copyAddr(addr: string) {
  try {
    await navigator.clipboard.writeText(addr)
    ElMessage.success('地址已复制')
  } catch {
    ElMessage.error('复制失败（浏览器权限受限）')
  }
}

// ---------- 测试区 ----------

const TEMPLATES: Record<'responses' | 'messages', string> = {
  responses: '{\n  "model": "",\n  "input": "Hello, please reply with one sentence."\n}',
  messages:
    '{\n  "model": "",\n  "max_tokens": 256,\n  "messages": [{ "role": "user", "content": "Hello, please reply with one sentence." }]\n}',
}

const test = reactive({
  api: 'responses' as 'responses' | 'messages',
  stream: false,
  bodyText: TEMPLATES.responses,
})

const testResult = ref<ApiTestResp | null>(null)

const bodyInvalid = computed(() => {
  try {
    JSON.parse(test.bodyText)
    return false
  } catch {
    return true
  }
})

function onBodyEdited() {
  testResult.value = null
}

function switchTemplate() {
  test.bodyText = TEMPLATES[test.api]
  testResult.value = null
}

async function runTest() {
  let body: unknown
  try {
    body = JSON.parse(test.bodyText)
  } catch {
    ElMessage.error('请求体不是合法 JSON')
    return
  }
  testing.value = true
  try {
    testResult.value = await api.testApiEndpoint({ api: test.api, stream: test.stream, body })
    await loadHistory()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '测试请求失败')
  } finally {
    testing.value = false
  }
}

// ---------- 历史 ----------

const history = ref<ApiTestResult[]>([])

async function loadHistory() {
  loadingHistory.value = true
  try {
    const r = await api.apiTestResults(20)
    history.value = r.results
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载测试历史失败')
  } finally {
    loadingHistory.value = false
  }
}

function fmtTime(iso: string) {
  const d = new Date(iso)
  return isNaN(d.getTime()) ? iso : d.toLocaleString()
}

onMounted(() => {
  load()
  loadHistory()
})

// API 选择切换时套用对应模板（仅当用户未手动编辑过或结果已清空时替换语义明确）
function onApiChanged() {
  switchTemplate()
}
</script>

<style scoped>
.page {
  padding: 20px;
}
.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 16px;
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
  max-width: 960px;
  line-height: 1.6;
}
.switch-box {
  display: flex;
  align-items: center;
  gap: 8px;
}
.switch-label {
  font-size: 14px;
  color: #303133;
  min-width: 140px;
}
.endpoint-note {
  margin-bottom: 16px;
}
.cards {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(420px, 1fr));
  gap: 16px;
}
.api-card .card-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.card-title {
  font-size: 15px;
  font-weight: 600;
}
.addr-row {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 14px;
}
.addr-label {
  font-size: 13px;
  color: #909399;
  flex-shrink: 0;
}
.addr {
  font-size: 13px;
  background: #f5f7fa;
  padding: 4px 8px;
  border-radius: 4px;
  word-break: break-all;
  flex: 1;
}
.switch-row {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 10px;
}
.hint {
  font-size: 12px;
  color: #909399;
}
.actions {
  margin-top: 14px;
  text-align: right;
}
.test-card {
  margin-top: 16px;
}
.test-form {
  max-width: 900px;
}
.test-row {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 12px;
}
.indent {
  margin-left: 24px;
  min-width: auto;
}
.body-input :deep(textarea) {
  font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', monospace;
  font-size: 13px;
}
.error-hint {
  color: #f56c6c;
}
.test-result {
  margin-top: 8px;
}
.result-head {
  display: flex;
  gap: 8px;
  align-items: center;
  margin-bottom: 10px;
}
.result-error {
  margin-bottom: 10px;
}
.result-body {
  background: #f5f7fa;
  border-radius: 4px;
  padding: 12px;
  font-size: 12px;
  max-height: 420px;
  overflow: auto;
  white-space: pre-wrap;
  word-break: break-all;
}
.history-card {
  margin-top: 16px;
}
.mono {
  font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', monospace;
}
</style>
