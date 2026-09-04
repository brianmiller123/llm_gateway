<script setup lang="ts">
// Plan 成员管理对话框：用户（直连）/ 分组（加入）双 Tab。
// 每个 Tab 提供分页检索、批量移除与「添加用户 / 添加分组」入口（选择器对话框）。
// 任何增删成功后 emit('changed')，由父页面刷新 Plan 汇总（成员数即时更新）。
import { reactive, ref, watch } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { request } from '@/api/client'
import type {
  CodingPlan,
  PlanGroupsResp,
  PlanMemberGroup,
  PlanMemberUser,
  PlanUsersResp,
} from '@/api/types'
import MemberPickerDialog from '@/components/MemberPickerDialog.vue'
import GroupMembersDialog from '@/components/GroupMembersDialog.vue'

const props = defineProps<{
  /** 目标 Plan（null = 关闭状态） */
  plan: CodingPlan | null
}>()

const visible = defineModel<boolean>({ default: false })
const emit = defineEmits<{ changed: [] }>()

type Tab = 'users' | 'groups'
const tab = ref<Tab>('users')

const userQuery = reactive({ q: '', page: 1, page_size: 20 })
const users = ref<PlanMemberUser[]>([])
const userTotal = ref(0)
const userSelected = ref<PlanMemberUser[]>([])
const userLoading = ref(false)

const groupQuery = reactive({ q: '', page: 1, page_size: 20 })
const groups = ref<PlanMemberGroup[]>([])
const groupTotal = ref(0)
const groupSelected = ref<PlanMemberGroup[]>([])
const groupLoading = ref(false)

const pickerVisible = ref(false)

// 分组 tab 行内「成员」入口：就地管理该分组的成员（增删即改 Plan 生效人群）
const groupMembersVisible = ref(false)
const memberGroup = ref<{ id: number; name: string } | null>(null)

function openGroupMembers(g: PlanMemberGroup) {
  memberGroup.value = { id: g.group_id, name: g.name }
  groupMembersVisible.value = true
}

/** 分组成员变更：刷新分组表（组内成员列），并同步父页面 Plan 汇总 */
async function onGroupMembersChanged() {
  await loadGroups()
  emit('changed')
}

async function loadUsers() {
  if (!props.plan) return
  userLoading.value = true
  try {
    const params = new URLSearchParams({
      page: String(userQuery.page),
      page_size: String(userQuery.page_size),
    })
    if (userQuery.q.trim()) params.set('q', userQuery.q.trim())
    const resp = await request<PlanUsersResp>(
      `/api/admin/plans/${props.plan.id}/users?${params}`,
    )
    users.value = resp.members
    userTotal.value = resp.total
    userSelected.value = []
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载用户失败')
  } finally {
    userLoading.value = false
  }
}

async function loadGroups() {
  if (!props.plan) return
  groupLoading.value = true
  try {
    const params = new URLSearchParams({
      page: String(groupQuery.page),
      page_size: String(groupQuery.page_size),
    })
    if (groupQuery.q.trim()) params.set('q', groupQuery.q.trim())
    const resp = await request<PlanGroupsResp>(
      `/api/admin/plans/${props.plan.id}/groups?${params}`,
    )
    groups.value = resp.groups
    groupTotal.value = resp.total
    groupSelected.value = []
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载分组失败')
  } finally {
    groupLoading.value = false
  }
}

function fmtTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '-'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

async function removeSelected() {
  if (!props.plan) return
  const isUserTab = tab.value === 'users'
  const count = isUserTab ? userSelected.value.length : groupSelected.value.length
  if (!count) return
  try {
    await ElMessageBox.confirm(`确定移除选中的 ${count} 项吗？`, '批量移除', {
      type: 'warning',
      confirmButtonText: '移除',
      cancelButtonText: '取消',
    })
  } catch {
    return
  }
  try {
    const resp = await request<{ removed: number }>(
      isUserTab
        ? `/api/admin/plans/${props.plan.id}/users/remove`
        : `/api/admin/plans/${props.plan.id}/groups/remove`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(
          isUserTab
            ? { user_ids: userSelected.value.map((m) => m.user_id) }
            : { group_ids: groupSelected.value.map((g) => g.group_id) },
        ),
      },
    )
    ElMessage.success(`已移除 ${resp.removed} 项`)
    if (isUserTab) await loadUsers()
    else await loadGroups()
    emit('changed')
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '移除失败')
  }
}

/** 选择器添加成功：刷新当前 Tab，并同步另一 Tab（新分组会改变其候选） */
async function onAdded() {
  await (tab.value === 'users' ? loadUsers() : loadGroups())
  emit('changed')
}

watch(visible, (v) => {
  if (v) {
    tab.value = 'users'
    userQuery.q = ''
    userQuery.page = 1
    groupQuery.q = ''
    groupQuery.page = 1
    loadUsers()
    loadGroups()
  }
})
</script>

<template>
  <el-dialog
    v-model="visible"
    :title="`成员管理：${plan?.name ?? ''}（${groupTotal} 分组 · ${userTotal} 直连用户）`"
    width="880px"
  >
    <el-tabs v-model="tab">
      <el-tab-pane name="users">
        <template #label>
          直连用户（{{ userTotal }}）
        </template>
        <div class="member-toolbar">
          <el-input
            v-model="userQuery.q"
            placeholder="搜索用户名 / 显示名 / 邮箱"
            clearable
            style="width: 240px"
            @keyup.enter="loadUsers"
            @clear="loadUsers"
          />
          <el-button @click="loadUsers">搜索</el-button>
          <div class="spacer" />
          <el-button type="primary" @click="pickerVisible = true">添加用户</el-button>
          <el-button :disabled="!userSelected.length" type="danger" @click="removeSelected">
            移除选中
          </el-button>
        </div>
        <el-table
          v-loading="userLoading"
          :data="users"
          stripe
          size="small"
          @selection-change="(rows: PlanMemberUser[]) => (userSelected = rows)"
        >
          <el-table-column type="selection" width="42" />
          <el-table-column label="用户名" prop="username" min-width="130" />
          <el-table-column label="显示名" min-width="110">
            <template #default="{ row }">{{ row.display_name || '-' }}</template>
          </el-table-column>
          <el-table-column label="邮箱" min-width="180">
            <template #default="{ row }">{{ row.email || '-' }}</template>
          </el-table-column>
          <el-table-column label="状态" width="80">
            <template #default="{ row }">
              <el-tag size="small" :type="row.status === 1 ? 'success' : 'danger'" effect="plain">
                {{ row.status === 1 ? '启用' : '禁用' }}
              </el-tag>
            </template>
          </el-table-column>
          <el-table-column label="加入时间" width="150">
            <template #default="{ row }">{{ fmtTime(row.added_at) }}</template>
          </el-table-column>
        </el-table>
        <el-pagination
          class="pager"
          layout="total, prev, pager, next"
          :total="userTotal"
          :page-size="userQuery.page_size"
          :current-page="userQuery.page"
          @current-change="(p: number) => { userQuery.page = p; loadUsers() }"
        />
      </el-tab-pane>

      <el-tab-pane name="groups">
        <template #label>
          分组（{{ groupTotal }}）
        </template>
        <div class="member-toolbar">
          <el-input
            v-model="groupQuery.q"
            placeholder="搜索组名 / 描述"
            clearable
            style="width: 240px"
            @keyup.enter="loadGroups"
            @clear="loadGroups"
          />
          <el-button @click="loadGroups">搜索</el-button>
          <div class="spacer" />
          <el-button type="primary" @click="pickerVisible = true">添加分组</el-button>
          <el-button :disabled="!groupSelected.length" type="danger" @click="removeSelected">
            移除选中
          </el-button>
        </div>
        <el-table
          v-loading="groupLoading"
          :data="groups"
          stripe
          size="small"
          @selection-change="(rows: PlanMemberGroup[]) => (groupSelected = rows)"
        >
          <el-table-column type="selection" width="42" />
          <el-table-column label="组名" prop="name" min-width="130" />
          <el-table-column label="描述" min-width="160">
            <template #default="{ row }">{{ row.description || '-' }}</template>
          </el-table-column>
          <el-table-column label="组内成员" prop="member_count" width="90" />
          <el-table-column label="LDAP 同步" width="100">
            <template #default="{ row }">
              <el-tag v-if="row.ldap_sync" size="small" type="warning" effect="plain">已开启</el-tag>
              <span v-else>-</span>
            </template>
          </el-table-column>
          <el-table-column label="加入时间" width="150">
            <template #default="{ row }">{{ fmtTime(row.added_at) }}</template>
          </el-table-column>
          <el-table-column label="操作" width="80" fixed="right">
            <template #default="{ row }">
              <el-button link type="primary" @click="openGroupMembers(row)">成员</el-button>
            </template>
          </el-table-column>
        </el-table>
        <el-pagination
          class="pager"
          layout="total, prev, pager, next"
          :total="groupTotal"
          :page-size="groupQuery.page_size"
          :current-page="groupQuery.page"
          @current-change="(p: number) => { groupQuery.page = p; loadGroups() }"
        />
      </el-tab-pane>
    </el-tabs>

    <MemberPickerDialog
      v-model="pickerVisible"
      :plan-id="plan?.id ?? null"
      :mode="tab === 'users' ? 'user' : 'group'"
      @added="onAdded"
    />

    <GroupMembersDialog
      v-model="groupMembersVisible"
      :group="memberGroup"
      @changed="onGroupMembersChanged"
    />
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
