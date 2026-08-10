<template>
  <div class="settings-page">
    <div class="toolbar">
      <div>
        <h2 class="page-title">系统设置</h2>
        <p class="page-desc">LDAP 登录配置（保存后立即生效，无需重启；环境变量作为未配置时的兜底）</p>
      </div>
      <el-tag :type="enabled ? 'success' : 'info'" effect="plain">
        LDAP {{ enabled ? '已启用' : '未启用' }}
      </el-tag>
    </div>

    <el-card shadow="never" class="panel" v-loading="loading">
      <template #header><span>LDAP / AD 登录</span></template>
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
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { Check, Connection } from '@element-plus/icons-vue'
import { request } from '@/api/client'
import type { LdapSettingsResp } from '@/api/types'

const loading = ref(false)
const saving = ref(false)
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

onMounted(load)
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
</style>
