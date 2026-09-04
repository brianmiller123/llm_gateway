<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { request } from '@/api/client'
import type { GroupsResp, UserGroup } from '@/api/types'
import GroupMembersDialog from '@/components/GroupMembersDialog.vue'

const loading = ref(false)
const groups = ref<GroupsResp['groups']>([])

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

const form = reactive({
  name: '',
  description: '',
  ldap_sync: false,
})

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

function openCreate() {
  editing.value = null
  form.name = ''
  form.description = ''
  form.ldap_sync = false
  dialogVisible.value = true
}

function openEdit(g: UserGroup) {
  editing.value = g
  form.name = g.name
  form.description = g.description
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
      await request(`/api/admin/groups/${editing.value.id}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      ElMessage.success('分组已更新')
    } else {
      await request('/api/admin/groups', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          name: form.name.trim(),
          description: form.description.trim(),
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

// —— 成员管理（共享对话框：列表/搜索/移除/一键全部/导出/选择器都在组件内）——
const memberVisible = ref(false)
const memberGroup = ref<UserGroup | null>(null)

function openMembers(g: UserGroup) {
  memberGroup.value = g
  memberVisible.value = true
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

onMounted(loadGroups)
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <div class="title">用户分组</div>
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
        <el-table-column label="加入 Plan" min-width="160">
          <template #default="{ row }">
            <template v-if="row.plans.length">
              <el-tag
                v-for="p in row.plans"
                :key="p.id"
                size="small"
                effect="plain"
                style="margin-right: 4px"
                :type="p.enabled ? 'primary' : 'info'"
              >
                {{ p.name }}{{ p.enabled ? '' : '（已停用）' }}
              </el-tag>
            </template>
            <span v-else class="muted">未加入（在 Coding Plan 页添加）</span>
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

    <!-- 成员管理（共享组件） -->
    <GroupMembersDialog v-model="memberVisible" :group="memberGroup" @changed="loadGroups" />
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
.hint {
  font-size: 12px;
  color: #909399;
  margin-left: 8px;
}
</style>
