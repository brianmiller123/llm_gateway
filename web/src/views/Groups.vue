<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { request } from '@/api/client'
import type { GroupMember, GroupsResp, MembersResp, UserGroup, UserPickResp } from '@/api/types'

const loading = ref(false)
const groups = ref<GroupsResp['groups']>([])
const plans = ref<{ id: number; name: string; enabled: boolean }[]>([])

async function loadGroups() {
  loading.value = true
  try {
    const resp = await request<GroupsResp>('/api/admin/groups')
    groups.value = resp.groups
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载分组失败')
  } finally {
    loading.value = false
  }
}

async function loadPlans() {
  try {
    const resp = await request<{ plans: { id: number; name: string; enabled: boolean }[] }>(
      '/api/admin/plans',
    )
    plans.value = resp.plans
  } catch {
    plans.value = []
  }
}

function fmtTime(iso: string | null): string {
  if (!iso) return '-'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '-'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

// —— 新建 / 编辑 ——
const dialogVisible = ref(false)
const editing = ref<UserGroup | null>(null)
const saving = ref(false)
const form = reactive({
  name: '',
  description: '',
  plan_id: null as number | null,
  clear_plan: false,
  ldap_sync: false,
})

function openCreate() {
  editing.value = null
  form.name = ''
  form.description = ''
  form.plan_id = null
  form.clear_plan = false
  form.ldap_sync = false
  dialogVisible.value = true
}

function openEdit(g: UserGroup) {
  editing.value = g
  form.name = g.name
  form.description = g.description
  form.plan_id = g.plan_id
  form.clear_plan = false
  form.ldap_sync = g.ldap_sync
  dialogVisible.value = true
}

async function doSave() {
  if (!form.name.trim()) {
    ElMessage.warning('请输入分组名称')
    return
  }
  saving.value = true
  try {
    if (editing.value) {
      const body: Record<string, unknown> = {
        name: form.name.trim(),
        description: form.description.trim(),
        ldap_sync: form.ldap_sync,
      }
      if (form.clear_plan) {
        body.clear_plan = true
      } else {
        body.plan_id = form.plan_id
      }
      await request(`/api/admin/groups/${editing.value.id}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      ElMessage.success('分组已更新，绑定调整即时生效')
    } else {
      await request('/api/admin/groups', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          name: form.name.trim(),
          description: form.description.trim(),
          plan_id: form.plan_id,
          ldap_sync: form.ldap_sync,
        }),
      })
      ElMessage.success('分组已创建')
    }
    dialogVisible.value = false
    await loadGroups()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    saving.value = false
  }
}

async function doDelete(g: UserGroup) {
  try {
    await ElMessageBox.confirm(
      `删除分组将移除全部成员关系（成员的 Plan 配额随之失效），确定删除「${g.name}」吗？`,
      '删除分组',
      { type: 'warning', confirmButtonText: '删除', cancelButtonText: '取消' },
    )
  } catch {
    return
  }
  try {
    await request(`/api/admin/groups/${g.id}`, { method: 'DELETE' })
    ElMessage.success('分组已删除')
    await loadGroups()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '删除失败')
  }
}

// —— 成员管理 ——
const memberVisible = ref(false)
const memberGroup = ref<UserGroup | null>(null)
const memberQuery = reactive({ q: '', page: 1, page_size: 20 })
const members = ref<GroupMember[]>([])
const memberTotal = ref(0)
const memberLoading = ref(false)
const selectedMembers = ref<GroupMember[]>([])

async function openMembers(g: UserGroup) {
  memberGroup.value = g
  memberQuery.q = ''
  memberQuery.page = 1
  memberVisible.value = true
  await loadMembers()
}

async function loadMembers() {
  if (!memberGroup.value) return
  memberLoading.value = true
  try {
    const params = new URLSearchParams({
      page: String(memberQuery.page),
      page_size: String(memberQuery.page_size),
    })
    if (memberQuery.q.trim()) params.set('q', memberQuery.q.trim())
    const resp = await request<MembersResp>(`/api/admin/groups/${memberGroup.value.id}/members?${params}`)
    members.value = resp.members
    memberTotal.value = resp.total
    selectedMembers.value = []
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载成员失败')
  } finally {
    memberLoading.value = false
  }
}

async function removeSelected() {
  if (!memberGroup.value || !selectedMembers.value.length) return
  const ids = selectedMembers.value.map((m) => m.user_id)
  try {
    await ElMessageBox.confirm(`确定移除选中的 ${ids.length} 个成员吗？`, '批量移除', {
      type: 'warning',
      confirmButtonText: '移除',
      cancelButtonText: '取消',
    })
  } catch {
    return
  }
  try {
    const resp = await request<{ removed: number }>(
      `/api/admin/groups/${memberGroup.value.id}/members/remove`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ user_ids: ids }),
      },
    )
    ElMessage.success(`已移除 ${resp.removed} 个成员`)
    await loadMembers()
    await loadGroups()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '移除失败')
  }
}

async function removeAllMembers() {
  if (!memberGroup.value) return
  try {
    await ElMessageBox.confirm(
      `将清空分组「${memberGroup.value.name}」的全部 ${memberGroup.value.member_count} 个成员（按当前搜索结果需逐页操作），此操作直接删除全部成员，确定吗？`,
      '清空成员',
      { type: 'warning', confirmButtonText: '全部移除', cancelButtonText: '取消' },
    )
  } catch {
    return
  }
  try {
    // 全量移除 = 逐用户 id 传入；成员多时按后端批量接口一次完成
    const resp = await request<{ removed: number }>(
      `/api/admin/groups/${memberGroup.value.id}/members/remove`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ user_ids: members.value.map((m) => m.user_id) }),
      },
    )
    ElMessage.success(`已移除 ${resp.removed} 个成员`)
    await loadMembers()
    await loadGroups()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '移除失败')
  }
}

async function addAllUsers() {
  if (!memberGroup.value) return
  try {
    await ElMessageBox.confirm(
      '将一键添加系统全部启用用户为成员（已在组内的自动跳过），确定吗？',
      '一键添加全部用户',
      { type: 'warning', confirmButtonText: '添加', cancelButtonText: '取消' },
    )
  } catch {
    return
  }
  try {
    const resp = await request<{ added: number }>(
      `/api/admin/groups/${memberGroup.value.id}/members/add-all`,
      { method: 'POST' },
    )
    ElMessage.success(`已添加 ${resp.added} 个用户`)
    await loadMembers()
    await loadGroups()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '添加失败')
  }
}

async function exportMembers() {
  if (!memberGroup.value) return
  try {
    const { tokenStorage } = await import('@/api/client')
    const token = tokenStorage.access
    const resp = await fetch(`/api/admin/groups/${memberGroup.value.id}/members/export`, {
      headers: token ? { Authorization: `Bearer ${token}` } : {},
    })
    if (!resp.ok) throw new Error(`导出失败（${resp.status}）`)
    const blob = await resp.blob()
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = `group_${memberGroup.value.name}_members.csv`
    a.click()
    URL.revokeObjectURL(url)
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '导出失败')
  }
}

// —— 添加成员（分页搜索 + 批量多选）——
const pickerVisible = ref(false)
const pickerQuery = reactive({ q: '', page: 1, page_size: 20 })
const pickerUsers = ref<UserPickResp['users']>([])
const pickerTotal = ref(0)
const pickerLoading = ref(false)
const pickerSelected = ref<number[]>([])

function openPicker() {
  pickerQuery.q = ''
  pickerQuery.page = 1
  pickerSelected.value = []
  pickerVisible.value = true
  loadPicker()
}

async function loadPicker() {
  if (!memberGroup.value) return
  pickerLoading.value = true
  try {
    const params = new URLSearchParams({
      group_id: String(memberGroup.value.id),
      page: String(pickerQuery.page),
      page_size: String(pickerQuery.page_size),
    })
    if (pickerQuery.q.trim()) params.set('q', pickerQuery.q.trim())
    const resp = await request<UserPickResp>(`/api/admin/groups/${memberGroup.value.id}/users?${params}`)
    pickerUsers.value = resp.users
    pickerTotal.value = resp.total
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载用户失败')
  } finally {
    pickerLoading.value = false
  }
}

async function doAddSelected() {
  if (!memberGroup.value || !pickerSelected.value.length) return
  try {
    const resp = await request<{ added: number }>(
      `/api/admin/groups/${memberGroup.value.id}/members/add`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ user_ids: pickerSelected.value }),
      },
    )
    ElMessage.success(`已添加 ${resp.added} 个成员（已在组内者跳过）`)
    pickerVisible.value = false
    await loadMembers()
    await loadGroups()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '添加失败')
  }
}

// —— LDAP 同步 ——
const syncing = ref(false)

async function syncLdap(g: UserGroup) {
  syncing.value = true
  try {
    const resp = await request<{ added: number; removed: number }>(`/api/admin/groups/${g.id}/sync`, {
      method: 'POST',
    })
    ElMessage.success(`同步完成：新增 ${resp.added}，移除 ${resp.removed}`)
    await loadGroups()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '同步失败')
  } finally {
    syncing.value = false
  }
}

onMounted(async () => {
  await Promise.all([loadGroups(), loadPlans()])
})
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <div class="title">用户分组（绑定 Coding Plan）</div>
      <el-button type="primary" @click="openCreate">新建分组</el-button>
    </div>

    <el-card shadow="never" body-class="table-card">
      <el-table v-loading="loading" :data="groups" stripe>
        <el-table-column label="分组" min-width="150">
          <template #default="{ row }">
            <div class="g-name">{{ row.name }}</div>
            <div class="g-desc">{{ row.description || '-' }}</div>
          </template>
        </el-table-column>
        <el-table-column label="绑定 Plan" min-width="160">
          <template #default="{ row }">
            <template v-if="row.plan_name">
              {{ row.plan_name }}
              <el-tag v-if="row.plan_enabled === false" size="small" type="info" effect="plain">
                已停用
              </el-tag>
            </template>
            <span v-else class="muted">未绑定（成员不限额）</span>
          </template>
        </el-table-column>
        <el-table-column label="成员数" prop="member_count" width="90" />
        <el-table-column label="LDAP 同步" width="110">
          <template #default="{ row }">
            <el-tag v-if="row.ldap_sync" size="small" type="warning" effect="plain">已开启</el-tag>
            <span v-else class="muted">-</span>
          </template>
        </el-table-column>
        <el-table-column label="最近同步" min-width="170">
          <template #default="{ row }">
            <template v-if="row.ldap_sync">
              {{ fmtTime(row.last_sync_at) }}
              <div v-if="row.last_sync_result" class="g-desc">{{ row.last_sync_result }}</div>
            </template>
            <span v-else class="muted">-</span>
          </template>
        </el-table-column>
        <el-table-column label="操作" width="270" fixed="right">
          <template #default="{ row }">
            <el-button link type="primary" @click="openMembers(row)">成员</el-button>
            <el-button v-if="row.ldap_sync" link type="warning" :loading="syncing" @click="syncLdap(row)">
              立即同步
            </el-button>
            <el-button link type="primary" @click="openEdit(row)">编辑</el-button>
            <el-button link type="danger" @click="doDelete(row)">删除</el-button>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <!-- 新建/编辑 -->
    <el-dialog
      v-model="dialogVisible"
      :title="editing ? `编辑分组：${editing.name}` : '新建分组'"
      width="520px"
    >
      <el-form label-width="110px">
        <el-form-item label="名称" required>
          <el-input v-model="form.name" maxlength="128" placeholder="如：研发团队" />
        </el-form-item>
        <el-form-item label="描述">
          <el-input v-model="form.description" type="textarea" :rows="2" placeholder="可选" />
        </el-form-item>
        <el-form-item label="绑定 Plan">
          <el-select v-model="form.plan_id" clearable placeholder="不绑定 = 成员不限额" style="width: 100%">
            <el-option
              v-for="p in plans"
              :key="p.id"
              :value="p.id"
              :label="`${p.name}${p.enabled ? '' : '（已停用）'}`"
            />
          </el-select>
        </el-form-item>
        <el-form-item v-if="editing && editing.plan_id && !form.plan_id" label-width="110px">
          <el-checkbox v-model="form.clear_plan">解绑当前 Plan</el-checkbox>
        </el-form-item>
        <el-form-item label="LDAP 同步">
          <el-switch v-model="form.ldap_sync" />
          <span class="hint">开启后成员随 LDAP 目录自动增删（每 10 分钟），手动添加的成员不受影响</span>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="doSave">保存</el-button>
      </template>
    </el-dialog>

    <!-- 成员管理 -->
    <el-dialog
      v-model="memberVisible"
      :title="`成员管理：${memberGroup?.name ?? ''}（${memberTotal} 人）`"
      width="860px"
    >
      <div class="member-toolbar">
        <el-input
          v-model="memberQuery.q"
          placeholder="搜索用户名 / 显示名 / 邮箱"
          clearable
          style="width: 240px"
          @keyup.enter="loadMembers"
          @clear="loadMembers"
        />
        <el-button @click="loadMembers">搜索</el-button>
        <div class="spacer" />
        <el-button type="primary" @click="openPicker">添加成员</el-button>
        <el-button @click="addAllUsers">一键添加全部用户</el-button>
        <el-button :disabled="!selectedMembers.length" type="danger" @click="removeSelected">
          移除选中
        </el-button>
        <el-button @click="exportMembers">导出 CSV</el-button>
      </div>
      <el-table
        v-loading="memberLoading"
        :data="members"
        stripe
        size="small"
        @selection-change="(rows: GroupMember[]) => (selectedMembers = rows)"
      >
        <el-table-column type="selection" width="42" />
        <el-table-column label="用户名" prop="username" min-width="130" />
        <el-table-column label="显示名" min-width="120">
          <template #default="{ row }">{{ row.display_name || '-' }}</template>
        </el-table-column>
        <el-table-column label="邮箱" min-width="160">
          <template #default="{ row }">{{ row.email || '-' }}</template>
        </el-table-column>
        <el-table-column label="来源" width="90">
          <template #default="{ row }">
            <el-tag
              size="small"
              :type="row.source === 'ldap' ? 'warning' : row.source === 'all' ? 'info' : 'success'"
              effect="plain"
            >
              {{ row.source === 'ldap' ? 'LDAP' : row.source === 'all' ? '全部' : '手动' }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="80">
          <template #default="{ row }">
            <el-tag size="small" :type="row.status === 1 ? 'success' : 'danger'" effect="plain">
              {{ row.status === 1 ? '启用' : '禁用' }}
            </el-tag>
          </template>
        </el-table-column>
      </el-table>
      <el-pagination
        class="pager"
        layout="total, prev, pager, next"
        :total="memberTotal"
        :page-size="memberQuery.page_size"
        :current-page="memberQuery.page"
        @current-change="(p: number) => { memberQuery.page = p; loadMembers() }"
      />
    </el-dialog>

    <!-- 添加成员选择器 -->
    <el-dialog v-model="pickerVisible" title="添加成员" width="720px" append-to-body>
      <div class="member-toolbar">
        <el-input
          v-model="pickerQuery.q"
          placeholder="搜索用户名 / 显示名 / 邮箱"
          clearable
          style="width: 240px"
          @keyup.enter="loadPicker"
          @clear="loadPicker"
        />
        <el-button @click="loadPicker">搜索</el-button>
      </div>
      <el-table
        v-loading="pickerLoading"
        :data="pickerUsers"
        stripe
        size="small"
        max-height="380"
        @selection-change="(rows: UserPickResp['users']) => (pickerSelected = rows.map((u) => u.id))"
      >
        <el-table-column type="selection" width="42" />
        <el-table-column label="用户名" prop="username" min-width="130" />
        <el-table-column label="显示名" min-width="120">
          <template #default="{ row }">{{ row.display_name || '-' }}</template>
        </el-table-column>
        <el-table-column label="邮箱" min-width="160">
          <template #default="{ row }">{{ row.email || '-' }}</template>
        </el-table-column>
        <el-table-column label="来源" width="90">
          <template #default="{ row }">{{ row.source === 'ldap' ? 'LDAP' : '本地' }}</template>
        </el-table-column>
        <el-table-column label="组内" width="80">
          <template #default="{ row }">
            <el-tag v-if="row.is_member" size="small" effect="plain">已加入</el-tag>
          </template>
        </el-table-column>
      </el-table>
      <el-pagination
        class="pager"
        layout="total, prev, pager, next"
        :total="pickerTotal"
        :page-size="pickerQuery.page_size"
        :current-page="pickerQuery.page"
        @current-change="(p: number) => { pickerQuery.page = p; loadPicker() }"
      />
      <template #footer>
        <el-button @click="pickerVisible = false">取消</el-button>
        <el-button type="primary" :disabled="!pickerSelected.length" @click="doAddSelected">
          添加选中（{{ pickerSelected.length }}）
        </el-button>
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
.g-name {
  font-weight: 500;
}
.g-desc {
  font-size: 12px;
  color: #909399;
  margin-top: 2px;
}
.muted {
  color: #909399;
  font-size: 12px;
}
.member-toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 12px;
  flex-wrap: wrap;
}
.spacer {
  flex: 1;
}
.pager {
  margin-top: 12px;
  justify-content: flex-end;
}
.hint {
  font-size: 12px;
  color: #909399;
  margin-left: 8px;
}
</style>
