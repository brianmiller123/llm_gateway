<template>
  <div class="settings-page">
    <div class="toolbar">
      <div>
        <h2 class="page-title">系统设置</h2>
        <p class="page-desc">LDAP 登录与自定义 Header 配置（保存后立即生效，无需重启；环境变量作为未配置时的兜底）</p>
      </div>
      <el-tag :type="enabled ? 'success' : 'info'" effect="plain">
        LDAP {{ enabled ? '已启用' : '未启用' }}
      </el-tag>
    </div>

    <el-tabs v-model="settingsTab">
      <el-tab-pane label="LDAP / AD 登录" name="ldap">
    <el-card shadow="never" class="panel" v-loading="loading">
      <el-form label-position="top" class="ldap-form" @submit.prevent>
        <el-form-item label="LDAP URL" required>
          <el-input
            v-model="form.url"
            placeholder="ldap://dc.example.com:389 或 ldaps://dc.example.com:636（留空 = 关闭 LDAP 登录）"
            clearable
          />
        </el-form-item>

        <el-form-item label="STARTTLS">
          <el-switch v-model="form.starttls" />
          <span class="hint">对 ldap:// 端口启用 STARTTLS 升级</span>
        </el-form-item>

        <el-form-item label="服务账号 Bind DN（可选）">
          <el-input v-model="form.bindDn" placeholder="CN=svc-gateway,OU=Service,DC=example,DC=com" clearable />
        </el-form-item>

        <el-form-item label="服务账号密码">
          <el-input
            v-model="form.bindPassword"
            type="password"
            show-password
            :placeholder="hasPassword ? '已保存（留空则保持不变）' : '服务账号密码'"
            autocomplete="new-password"
          />
        </el-form-item>

        <el-form-item label="Base DN" required>
          <el-input v-model="form.baseDn" placeholder="DC=example,DC=com" clearable />
        </el-form-item>

        <el-form-item label="用户过滤器" required>
          <el-input v-model="form.userFilter" placeholder="(&amp;(objectClass=person)(sAMAccountName={0}))" clearable />
          <span class="hint">{0} 为登录名占位符，不可省略</span>
        </el-form-item>

        <el-form-item label="管理员组（逗号分隔的 DN）">
          <el-input
            v-model="form.adminGroupsText"
            type="textarea"
            :rows="2"
            placeholder="CN=LLM-Admins,OU=Groups,DC=example,DC=com"
          />
        </el-form-item>

        <div class="actions">
          <el-button :icon="Connection" :loading="testing" @click="doTest">测试连接</el-button>
          <el-button type="primary" :icon="Check" :loading="saving" @click="doSave">保存</el-button>
        </div>
      </el-form>
    </el-card>
      </el-tab-pane>
      <el-tab-pane label="自定义 Header" name="headers">
    <el-card shadow="never" class="panel" v-loading="headersLoading">
      <el-form label-position="top" @submit.prevent>
        <el-form-item label="上游请求头（网关 → 提供商）">
          <div v-for="(row, i) in upstreamRows" :key="i" class="header-row">
            <el-input v-model="row.name" placeholder="Header 名，如 X-Title" style="width: 220px" />
            <el-input v-model="row.value" placeholder="值" style="flex: 1" />
            <el-button :icon="Delete" circle text type="danger" @click="upstreamRows.splice(i, 1)" />
          </div>
          <el-button :icon="Plus" text type="primary" @click="upstreamRows.push({ name: '', value: '' })">
            添加上游请求头
          </el-button>
          <div class="header-tip">
            追加到网关发往上游提供商的每个代理请求（与渠道级 extra_headers 并存）。
            认证 / Content-Type / Accept 等网关管理头不可设置；Connection: close 等
            仅对 HTTP/1.1 上游生效。
          </div>
        </el-form-item>

        <el-form-item label="客户端响应头（网关 → 客户端）">
          <div v-for="(row, i) in responseRows" :key="i" class="header-row">
            <el-input v-model="row.name" placeholder="Header 名，如 X-Gateway-Region" style="width: 220px" />
            <el-input v-model="row.value" placeholder="值" style="flex: 1" />
            <el-button :icon="Delete" circle text type="danger" @click="responseRows.splice(i, 1)" />
          </div>
          <el-button :icon="Plus" text type="primary" @click="responseRows.push({ name: '', value: '' })">
            添加客户端响应头
          </el-button>
          <div class="header-tip">
            附加到 /v1/* 的所有响应（含错误响应）。Content-Type / Content-Length / Connection
            等由 HTTP 层管理，不可设置。
          </div>
        </el-form-item>

        <div class="actions">
          <el-button type="primary" :loading="headersSaving" @click="saveHeaders">保存</el-button>
        </div>
      </el-form>
    </el-card>
      </el-tab-pane>
      <el-tab-pane label="告警邮件（SMTP）" name="smtp">
        <el-card shadow="never" class="panel" v-loading="smtpLoading">
          <el-form label-position="top" @submit.prevent>
            <el-form-item label="SMTP 服务器" required>
              <el-input v-model="smtpForm.host" placeholder="smtp.example.com" clearable />
            </el-form-item>
            <el-form-item label="端口">
              <el-input-number v-model="smtpForm.port" :min="0" :max="65535" controls-position="right" />
              <span class="hint">465 使用隐式 TLS，其余端口使用 STARTTLS；0 = 未配置</span>
            </el-form-item>
            <el-form-item label="账号（可选）">
              <el-input v-model="smtpForm.username" placeholder="服务器要求认证时填写" clearable />
            </el-form-item>
            <el-form-item label="密码">
              <el-input
                v-model="smtpForm.password"
                type="password"
                show-password
                :placeholder="smtpHasPassword ? '已保存（留空则保持不变）' : 'SMTP 密码'"
                autocomplete="new-password"
              />
            </el-form-item>
            <el-form-item label="发件人" required>
              <el-input v-model="smtpForm.from" placeholder="LLM Gateway <gateway@example.com>" clearable />
            </el-form-item>
            <el-form-item label="测试收件人">
              <div class="actions">
                <el-input v-model="smtpTestTo" placeholder="you@example.com" style="width: 260px" />
                <el-button :loading="smtpTesting" @click="doSmtpTest">发送测试邮件</el-button>
              </div>
            </el-form-item>
            <div class="actions">
              <el-button type="primary" :icon="Check" :loading="smtpSaving" @click="doSmtpSave">保存</el-button>
            </div>
          </el-form>
        </el-card>
       </el-tab-pane>
    </el-tabs>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { Check, Connection, Delete, Plus } from '@element-plus/icons-vue'
import { request } from '@/api/client'
import type { HeaderSettingsResp, LdapSettingsResp, SmtpSettingsResp } from '@/api/types'

const loading = ref(false)
const saving = ref(false)
const settingsTab = ref('ldap')
const testing = ref(false)
const hasPassword = ref(false)

const form = reactive({
  url: '',
  starttls: false,
  bindDn: '',
  bindPassword: '',
  baseDn: '',
  userFilter: '(&(objectClass=person)(sAMAccountName={0}))',
  adminGroupsText: '',
})

const enabled = computed(() => form.url.trim() !== '')

function payload() {
  return {
    url: form.url.trim(),
    starttls: form.starttls,
    bind_dn: form.bindDn.trim() || null,
    bind_password: form.bindPassword,
    base_dn: form.baseDn.trim(),
    user_filter: form.userFilter.trim(),
    admin_groups: form.adminGroupsText
      .split(',')
      .map((s) => s.trim())
      .filter((s) => s !== ''),
  }
}

async function load() {
  loading.value = true
  try {
    const resp = await request<LdapSettingsResp>('/api/admin/settings/ldap')
    form.url = resp.url
    form.starttls = resp.starttls
    form.bindDn = resp.bind_dn ?? ''
    form.bindPassword = ''
    form.baseDn = resp.base_dn
    form.userFilter = resp.user_filter
    form.adminGroupsText = resp.admin_groups.join(', ')
    hasPassword.value = resp.has_password
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载设置失败')
  } finally {
    loading.value = false
  }
}

async function doTest() {
  testing.value = true
  try {
    const resp = await request<{ ok: boolean; message: string }>('/api/admin/settings/ldap/test', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload()),
    })
    ElMessage.success(resp.message)
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '连接测试失败')
  } finally {
    testing.value = false
  }
}

async function doSave() {
  saving.value = true
  try {
    await request('/api/admin/settings/ldap', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload()),
    })
    form.bindPassword = ''
    ElMessage.success(enabled.value ? 'LDAP 设置已保存并生效' : 'LDAP 登录已关闭')
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    saving.value = false
  }
}

// ============ 自定义 Header ============
interface HeaderRow {
  name: string
  value: string
}

const headersLoading = ref(false)
const headersSaving = ref(false)
const upstreamRows = ref<HeaderRow[]>([])
const responseRows = ref<HeaderRow[]>([])

function rowsFrom(map: Record<string, string>): HeaderRow[] {
  return Object.entries(map).map(([name, value]) => ({ name, value }))
}

function rowsToMap(rows: HeaderRow[]): Record<string, string> {
  const out: Record<string, string> = {}
  for (const r of rows) {
    const name = r.name.trim()
    if (name !== '') out[name] = r.value.trim()
  }
  return out
}

async function loadHeaders() {
  headersLoading.value = true
  try {
    const resp = await request<HeaderSettingsResp>('/api/admin/settings/headers')
    upstreamRows.value = rowsFrom(resp.upstream_headers)
    responseRows.value = rowsFrom(resp.response_headers)
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载 Header 设置失败')
  } finally {
    headersLoading.value = false
  }
}

async function saveHeaders() {
  headersSaving.value = true
  try {
    const payloadHeaders = {
      upstream_headers: rowsToMap(upstreamRows.value),
      response_headers: rowsToMap(responseRows.value),
    }
    const resp = await request<HeaderSettingsResp>('/api/admin/settings/headers', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payloadHeaders),
    })
    upstreamRows.value = rowsFrom(resp.upstream_headers)
    responseRows.value = rowsFrom(resp.response_headers)
    ElMessage.success('Header 设置已保存并生效')
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    headersSaving.value = false
  }
}

// ---------- 告警邮件（SMTP） ----------
const smtpLoading = ref(false)
const smtpSaving = ref(false)
const smtpTesting = ref(false)
const smtpHasPassword = ref(false)
const smtpTestTo = ref('')
const smtpForm = reactive({
  host: '',
  port: 0,
  username: '',
  password: '',
  from: '',
})

async function loadSmtp() {
  smtpLoading.value = true
  try {
    const resp = await request<SmtpSettingsResp>('/api/admin/smtp')
    smtpForm.host = resp.host
    smtpForm.port = resp.port
    smtpForm.username = resp.username
    smtpForm.from = resp.from
    smtpForm.password = ''
    smtpHasPassword.value = resp.has_password
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载 SMTP 配置失败')
  } finally {
    smtpLoading.value = false
  }
}

async function doSmtpSave() {
  if (!smtpForm.host.trim() || !smtpForm.from.trim()) {
    ElMessage.warning('SMTP 服务器与发件人为必填项')
    return
  }
  smtpSaving.value = true
  try {
    await request('/api/admin/smtp', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        host: smtpForm.host.trim(),
        port: smtpForm.port,
        username: smtpForm.username.trim(),
        password: smtpForm.password === '' ? null : smtpForm.password,
        from: smtpForm.from.trim(),
      }),
    })
    ElMessage.success('SMTP 配置已保存')
    smtpForm.password = ''
    await loadSmtp()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    smtpSaving.value = false
  }
}

async function doSmtpTest() {
  if (!smtpTestTo.value.trim()) {
    ElMessage.warning('请填写测试收件人邮箱')
    return
  }
  smtpTesting.value = true
  try {
    await request('/api/admin/smtp/test', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ to: smtpTestTo.value.trim() }),
    })
    ElMessage.success('测试邮件已发送')
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '发送失败（请先保存配置）')
  } finally {
    smtpTesting.value = false
  }
}

onMounted(loadHeaders)
onMounted(loadSmtp)
</script>

<style scoped>
.settings-page {
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
}

.panel {
  max-width: 640px;
}

.ldap-form {
  padding: 8px 4px 0;
}

.hint {
  margin-left: 10px;
  font-size: 12px;
  color: #909399;
}

.actions {
  display: flex;
  gap: 12px;
}

.header-row {
  display: flex;
  gap: 8px;
  width: 100%;
  margin-bottom: 8px;
  align-items: center;
}

.header-tip {
  font-size: 12px;
  line-height: 1.5;
  color: #909399;
  margin-top: 4px;
  width: 100%;
}
</style>
