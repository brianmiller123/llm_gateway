<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { request } from '@/api/client'
import type {
  AccessRuleInput,
  ProvidersResp,
  UserAccessResp,
  UserWithUsage,
  UsersResp,
} from '@/api/types'
import type { ProviderRow } from '@/api/types'

const loading = ref(false)
const users = ref<UserWithUsage[]>([])

/** 格式化 ISO 时间戳为 YYYY-MM-DD HH:mm，空值显示 '-' */
function fmt(iso: string | null): string {
  if (!iso) return '-'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '-'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

async function loadUsers() {
  loading.value = true
  try {
    const resp = await request<UsersResp>('/api/admin/users')
    users.value = resp.users
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载用户列表失败')
  } finally {
    loading.value = false
  }
}

async function toggleStatus(u: UserWithUsage) {
  const disable = u.status === 1
  try {
    if (disable) {
      await ElMessageBox.confirm(
        `禁用后该用户所有会话与 API Key 立即失效，确定禁用 ${u.username} 吗？`,
        '禁用用户',
        { type: 'warning', confirmButtonText: '禁用', cancelButtonText: '取消' },
      )
    }
  } catch {
    return // 用户取消
  }
  try {
    await request(`/api/admin/users/${u.id}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ status: disable ? 0 : 1 }),
    })
    ElMessage.success(disable ? '用户已禁用' : '用户已启用')
    await loadUsers()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '操作失败')
  }
}

async function forceLogout(u: UserWithUsage) {
  try {
    await ElMessageBox.confirm(
      '将吊销该用户所有会话（含 refresh token），确定？',
      '强制下线',
      { type: 'warning', confirmButtonText: '强制下线', cancelButtonText: '取消' },
    )
  } catch {
    return // 用户取消
  }
  try {
    await request(`/api/admin/users/${u.id}/force-logout`, { method: 'POST' })
    ElMessage.success('已强制该用户下线')
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '操作失败')
  }
}

// —— 新建用户 ——
const createVisible = ref(false)
const createForm = reactive({
  username: '',
  display_name: '',
  password: '',
  confirm: '',
  is_admin: false,
})
const creating = ref(false)

function openCreate() {
  createForm.username = ''
  createForm.display_name = ''
  createForm.password = ''
  createForm.confirm = ''
  createForm.is_admin = false
  createVisible.value = true
}

async function doCreate() {
  const username = createForm.username.trim()
  if (!username) {
    ElMessage.warning('请输入用户名')
    return
  }
  if (createForm.password.length < 8) {
    ElMessage.warning('初始密码至少 8 位')
    return
  }
  if (createForm.password !== createForm.confirm) {
    ElMessage.warning('两次输入的密码不一致')
    return
  }
  creating.value = true
  try {
    await request('/api/admin/users', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        username,
        display_name: createForm.display_name.trim() || undefined,
        password: createForm.password,
        is_admin: createForm.is_admin,
      }),
    })
    ElMessage.success(`用户 ${username} 已创建`)
    createVisible.value = false
    await loadUsers()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '创建失败')
  } finally {
    creating.value = false
  }
}

// —— 重置密码 ——
const resetVisible = ref(false)
const resetTarget = ref<UserWithUsage | null>(null)
const resetForm = reactive({ password: '', confirm: '' })
const resetting = ref(false)

function openReset(u: UserWithUsage) {
  resetTarget.value = u
  resetForm.password = ''
  resetForm.confirm = ''
  resetVisible.value = true
}

async function doReset() {
  if (resetForm.password.length < 8) {
    ElMessage.warning('新密码至少 8 位')
    return
  }
  if (resetForm.password !== resetForm.confirm) {
    ElMessage.warning('两次输入的密码不一致')
    return
  }
  const target = resetTarget.value
  if (!target) return
  resetting.value = true
  try {
    await request(`/api/admin/users/${target.id}/reset-password`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ password: resetForm.password }),
    })
    ElMessage.success('密码已重置，该用户需重新登录')
    resetVisible.value = false
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '重置失败')
  } finally {
    resetting.value = false
  }
}

// —— 模型/供应商授权 ——
const accessVisible = ref(false)
const accessTarget = ref<UserWithUsage | null>(null)
const accessConfigured = ref(false)
const accessRules = ref<AccessRuleInput[]>([])
const accessSaving = ref(false)
const providers = ref<ProviderRow[]>([])

async function loadProviders() {
  try {
    const resp = await request<ProvidersResp>('/api/admin/providers')
    providers.value = resp.providers
  } catch {
    providers.value = []
  }
}

async function openAccess(u: UserWithUsage) {
  accessTarget.value = u
  accessRules.value = []
  accessConfigured.value = false
  accessVisible.value = true
  await loadProviders()
  try {
    const resp = await request<UserAccessResp>(`/api/admin/users/${u.id}/access`)
    accessConfigured.value = resp.configured
    accessRules.value = resp.rules.map((r) => ({
      provider_id: r.provider_id,
      model_pattern: r.model_pattern,
    }))
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载授权失败')
  }
}

function addAccessRule() {
  accessRules.value.push({ provider_id: null, model_pattern: null })
}

function removeAccessRule(i: number) {
  accessRules.value.splice(i, 1)
}

async function doSaveAccess() {
  const target = accessTarget.value
  if (!target) return
  accessSaving.value = true
  try {
    await request(`/api/admin/users/${target.id}/access`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ rules: accessRules.value }),
    })
    ElMessage.success('授权已更新，立即生效')
    accessVisible.value = false
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    accessSaving.value = false
  }
}

onMounted(loadUsers)
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <span class="title">用户生命周期管理</span>
      <div>
        <el-button type="primary" @click="openCreate">新建用户</el-button>
        <el-button :loading="loading" @click="loadUsers">刷新</el-button>
      </div>
    </div>

    <el-card shadow="never" body-class="table-card">
      <el-table v-loading="loading" :data="users" stripe>
        <el-table-column prop="id" label="ID" width="70" />
        <el-table-column prop="username" label="用户名" min-width="130" show-overflow-tooltip />
        <el-table-column label="显示名" min-width="130" show-overflow-tooltip>
          <template #default="{ row }">{{ row.display_name ?? '-' }}</template>
        </el-table-column>
        <el-table-column label="来源" width="90">
          <template #default="{ row }">
            <el-tag v-if="row.source === 'ldap'" type="warning" size="small">LDAP</el-tag>
            <el-tag v-else type="success" size="small">本地</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="角色" width="90">
          <template #default="{ row }">
            <el-tag v-if="row.is_admin" type="primary" size="small">管理员</el-tag>
            <el-tag v-else type="info" size="small">成员</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="80">
          <template #default="{ row }">
            <el-tag v-if="row.status === 1" type="success" size="small">启用</el-tag>
            <el-tag v-else type="danger" size="small">禁用</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="当月 Token" min-width="110" align="right">
          <template #default="{ row }">{{ (row.month_tokens ?? 0).toLocaleString('en-US') }}</template>
        </el-table-column>
        <el-table-column label="当月成本" min-width="100" align="right">
          <template #default="{ row }">{{ (row.month_cost ?? 0).toFixed(2) }}</template>
        </el-table-column>
        <el-table-column label="最后登录" min-width="140">
          <template #default="{ row }">{{ fmt(row.last_login_at) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="250" fixed="right">
          <template #default="{ row }">
            <el-button size="small" type="primary" plain @click="openAccess(row)">模型授权</el-button>
            <el-button
              v-if="row.status === 1"
              size="small"
              type="danger"
              plain
              @click="toggleStatus(row)"
            >禁用</el-button>
            <el-button
              v-else
              size="small"
              type="success"
              plain
              @click="toggleStatus(row)"
            >启用</el-button>
            <el-button size="small" @click="forceLogout(row)">强制下线</el-button>
            <el-button
              v-if="row.source === 'local'"
              size="small"
              @click="openReset(row)"
            >重置密码</el-button>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-dialog
      v-model="accessVisible"
      :title="`模型授权 - ${accessTarget?.username ?? ''}`"
      width="640px"
      :close-on-click-modal="false"
    >
      <el-alert
        type="info"
        :closable="false"
        show-icon
        class="access-hint"
        :title="accessConfigured
          ? '白名单模式：用户仅可用下列规则命中的供应商/模型'
          : '当前为默认放行：用户可用所有路由。配置任意规则后切换为白名单模式'"
      />
      <div class="access-rules">
        <div v-for="(rule, i) in accessRules" :key="i" class="access-rule">
          <el-select
            v-model="rule.provider_id"
            placeholder="任意供应商"
            clearable
            style="width: 200px"
          >
            <el-option
              v-for="p in providers"
              :key="p.id"
              :label="p.name"
              :value="p.id"
            />
          </el-select>
          <el-input
            v-model="rule.model_pattern"
            placeholder="模型名或通配符，如 mock-*（留空=全部）"
            clearable
            style="width: 300px"
          />
          <el-button
            link
            type="danger"
            :disabled="accessRules.length <= 1"
            @click="removeAccessRule(i)"
          >删除</el-button>
        </div>
      </div>
      <el-button plain class="access-add" @click="addAccessRule">+ 添加规则</el-button>
      <template #footer>
        <el-button @click="accessVisible = false">取消</el-button>
        <el-button type="primary" :loading="accessSaving" @click="doSaveAccess">保存</el-button>
      </template>
    </el-dialog>

    <el-dialog
      v-model="createVisible"
      title="新建用户"
      width="460px"
      :close-on-click-modal="false"
    >
      <el-form label-width="80px">
        <el-form-item label="用户名">
          <el-input v-model="createForm.username" placeholder="登录用户名，唯一" maxlength="64" />
        </el-form-item>
        <el-form-item label="显示名">
          <el-input v-model="createForm.display_name" placeholder="可选" maxlength="128" />
        </el-form-item>
        <el-form-item label="初始密码">
          <el-input v-model="createForm.password" type="password" show-password placeholder="至少 8 位" />
        </el-form-item>
        <el-form-item label="确认密码">
          <el-input v-model="createForm.confirm" type="password" show-password placeholder="再次输入新密码" />
        </el-form-item>
        <el-form-item label="管理员">
          <el-switch v-model="createForm.is_admin" />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="createVisible = false">取消</el-button>
        <el-button type="primary" :loading="creating" @click="doCreate">创建</el-button>
      </template>
    </el-dialog>

    <el-dialog
      v-model="resetVisible"
      title="重置密码"
      width="420px"
      :close-on-click-modal="false"
    >
      <el-form label-width="80px">
        <el-form-item label="新密码">
          <el-input
            v-model="resetForm.password"
            type="password"
            show-password
            placeholder="至少 8 位"
          />
        </el-form-item>
        <el-form-item label="确认密码">
          <el-input
            v-model="resetForm.confirm"
            type="password"
            show-password
            placeholder="再次输入新密码"
          />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="resetVisible = false">取消</el-button>
        <el-button type="primary" :loading="resetting" @click="doReset">确定</el-button>
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
  font-size: 16px;
  font-weight: 600;
  color: #303133;
}

.access-hint {
  margin-bottom: 12px;
}

.access-rules {
  display: flex;
  flex-direction: column;
  gap: 10px;
  max-height: 320px;
  overflow-y: auto;
}

.access-rule {
  display: flex;
  align-items: center;
  gap: 8px;
}

.access-add {
  margin-top: 12px;
}
</style>
