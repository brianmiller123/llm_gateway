<script setup lang="ts">
// 分组成员管理对话框（Groups 页与 Coding Plan 成员管理共用）：
// 成员分页检索、批量移除、一键全部、CSV 导出 + 内嵌用户选择器（is_member 标记、多选添加）。
// 任何成员变更成功后 emit('changed')，由父页面刷新成员计数。
import { reactive, ref, watch } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { request, tokenStorage } from '@/api/client'
import type { GroupMember, MembersResp, UserPickResp } from '@/api/types'

const props = defineProps<{
  /** 目标分组（null = 关闭状态） */
  group: { id: number; name: string } | null
}>()

const visible = defineModel<boolean>({ default: false })
const emit = defineEmits<{ changed: [] }>()

const query = reactive({ q: '', page: 1, page_size: 20 })
const members = ref<GroupMember[]>([])
const total = ref(0)
const loading = ref(false)
const selected = ref<GroupMember[]>([])

async function load() {
  if (!props.group) return
  loading.value = true
  try {
    const params = new URLSearchParams({
      page: String(query.page),
      page_size: String(query.page_size),
    })
    if (query.q.trim()) params.set('q', query.q.trim())
    const resp = await request<MembersResp>(`/api/admin/groups/${props.group.id}/members?${params}`)
    members.value = resp.members
    total.value = resp.total
    selected.value = []
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载成员失败')
  } finally {
    loading.value = false
  }
}

watch(visible, (v) => {
  if (v) {
    query.q = ''
    query.page = 1
    load()
  }
})

async function removeSelected() {
  if (!props.group || !selected.value.length) return
  const ids = selected.value.map((m) => m.user_id)
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
      `/api/admin/groups/${props.group.id}/members/remove`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ user_ids: ids }),
      },
    )
    ElMessage.success(`已移除 ${resp.removed} 个成员`)
    await load()
    emit('changed')
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '移除失败')
  }
}

async function addAllUsers() {
  if (!props.group) return
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
      `/api/admin/groups/${props.group.id}/members/add-all`,
      { method: 'POST' },
    )
    ElMessage.success(`已添加 ${resp.added} 个用户`)
    await load()
    emit('changed')
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '添加失败')
  }
}

async function exportCsv() {
  if (!props.group) return
  try {
    const token = tokenStorage.access
    const resp = await fetch(`/api/admin/groups/${props.group.id}/members/export`, {
      headers: token ? { Authorization: `Bearer ${token}` } : {},
    })
    if (!resp.ok) throw new Error(`导出失败（${resp.status}）`)
    const blob = await resp.blob()
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = `group_${props.group.name}_members.csv`
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
  if (!props.group) return
  pickerLoading.value = true
  try {
    const params = new URLSearchParams({
      group_id: String(props.group.id),
      page: String(pickerQuery.page),
      page_size: String(pickerQuery.page_size),
    })
    if (pickerQuery.q.trim()) params.set('q', pickerQuery.q.trim())
    const resp = await request<UserPickResp>(`/api/admin/groups/${props.group.id}/users?${params}`)
    pickerUsers.value = resp.users
    pickerTotal.value = resp.total
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载用户失败')
  } finally {
    pickerLoading.value = false
  }
}

async function doAddSelected() {
  if (!props.group || !pickerSelected.value.length) return
  try {
    const resp = await request<{ added: number }>(
      `/api/admin/groups/${props.group.id}/members/add`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ user_ids: pickerSelected.value }),
      },
    )
    ElMessage.success(`已添加 ${resp.added} 个成员（已在组内者跳过）`)
    pickerVisible.value = false
    await load()
    emit('changed')
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '添加失败')
  }
}
</script>

<template>
  <el-dialog
    v-model="visible"
    :title="`成员管理：${group?.name ?? ''}（${total} 人）`"
    width="860px"
  >
    <div class="member-toolbar">
      <el-input
        v-model="query.q"
        placeholder="搜索用户名 / 显示名 / 邮箱"
        clearable
        style="width: 240px"
        @keyup.enter="load"
        @clear="load"
      />
      <el-button @click="load">搜索</el-button>
      <div class="spacer" />
      <el-button type="primary" @click="openPicker">添加成员</el-button>
      <el-button @click="addAllUsers">一键添加全部用户</el-button>
      <el-button :disabled="!selected.length" type="danger" @click="removeSelected">
        移除选中
      </el-button>
      <el-button @click="exportCsv">导出 CSV</el-button>
    </div>
    <el-table
      v-loading="loading"
      :data="members"
      stripe
      size="small"
      @selection-change="(rows: GroupMember[]) => (selected = rows)"
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
      :total="total"
      :page-size="query.page_size"
      :current-page="query.page"
      @current-change="(p: number) => { query.page = p; load() }"
    />

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
  </el-dialog>
</template>

<style scoped>
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
</style>
