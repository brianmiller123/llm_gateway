<script setup lang="ts">
// Plan 成员选择器：分页模糊搜索 + 多选确认。
// mode=user：按用户名/显示名/邮箱搜索；mode=group：按组名/描述搜索。
// 已在该 Plan 中的行（is_member）标记「已加入」且不可勾选（后端唯一约束兜底重复添加 409）。
import { computed, reactive, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { request } from '@/api/client'
import type {
  PlanGroupPick,
  PlanGroupPickResp,
  UserPick,
  UserPickResp,
} from '@/api/types'

const props = defineProps<{
  /** 添加目标 Plan id */
  planId: number | null
  /** user = 添加直连用户；group = 添加分组 */
  mode: 'user' | 'group'
}>()

const visible = defineModel<boolean>({ default: false })
const emit = defineEmits<{ added: [] }>()

const isUser = () => props.mode === 'user'
const query = reactive({ q: '', page: 1, page_size: 20 })
const users = ref<UserPickResp['users']>([])
const groups = ref<PlanGroupPickResp['groups']>([])
const total = ref(0)
const loading = ref(false)
const selected = ref<number[]>([])

async function load() {
  if (!props.planId) return
  loading.value = true
  try {
    const params = new URLSearchParams({
      page: String(query.page),
      page_size: String(query.page_size),
    })
    if (query.q.trim()) params.set('q', query.q.trim())
    if (isUser()) {
      const resp = await request<UserPickResp>(
        `/api/admin/plans/${props.planId}/user-candidates?${params}`,
      )
      users.value = resp.users
      groups.value = []
      total.value = resp.total
    } else {
      const resp = await request<PlanGroupPickResp>(
        `/api/admin/plans/${props.planId}/group-candidates?${params}`,
      )
      groups.value = resp.groups
      users.value = []
      total.value = resp.total
    }
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载列表失败')
  } finally {
    loading.value = false
  }
}

function reset() {
  query.q = ''
  query.page = 1
  selected.value = []
  load()
}

watch(visible, (v) => {
  if (v) reset()
})
// 空态引导：候选为空时点名入口（反馈：只看到搜索框，不知道分组从哪里来）
const emptyText = computed(() => {
  if (query.q.trim()) return '没有匹配的结果'
  return isUser() ? '暂无候选用户' : '暂无分组：点击上方「新建分组」创建后再加入 Plan'
})

function onSelectionChange(rows: Array<UserPick | PlanGroupPick>) {
  selected.value = rows.map((r) => r.id)
}

async function doAdd() {
  if (!props.planId || !selected.value.length) return
  try {
    const resp = await request<{ added: number }>(
      isUser()
        ? `/api/admin/plans/${props.planId}/users/add`
        : `/api/admin/plans/${props.planId}/groups/add`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(
          isUser() ? { user_ids: selected.value } : { group_ids: selected.value },
        ),
      },
    )
    ElMessage.success(`已添加 ${resp.added} 个${isUser() ? '用户' : '分组'}`)
    visible.value = false
    emit('added')
  } catch (e) {
    // 409 = 部分成员已被加入；400 = 参数/存在性错误；提示信息取自后端
    ElMessage.error(e instanceof Error ? e.message : '添加失败')
  }
}

// 分组模式下内联新建分组（控制台分组页之外就地补齐；成功后刷新候选列表）
const createVisible = ref(false)
const creating = ref(false)
const createForm = reactive({ name: '', description: '' })

function openCreate() {
  createForm.name = query.q.trim()
  createForm.description = ''
  createVisible.value = true
}

async function doCreate() {
  const name = createForm.name.trim()
  if (!name) {
    ElMessage.warning('请输入分组名称')
    return
  }
  creating.value = true
  try {
    await request('/api/admin/groups', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, description: createForm.description.trim() }),
    })
    ElMessage.success(`分组「${name}」已创建`)
    createVisible.value = false
    query.q = ''
    query.page = 1
    await load()
  } catch (e) {
    // 重名等错误提示取自后端
    ElMessage.error(e instanceof Error ? e.message : '创建失败')
  } finally {
    creating.value = false
  }
}
</script>

<template>
  <el-dialog
    v-model="visible"
    :title="isUser() ? '添加用户' : '添加分组'"
    width="720px"
    append-to-body
  >
    <div class="picker-toolbar">
      <el-input
        v-model="query.q"
        :placeholder="isUser() ? '搜索用户名 / 显示名 / 邮箱' : '搜索组名 / 描述'"
        clearable
        style="width: 260px"
        @keyup.enter="reset"
        @clear="reset"
      />
      <el-button @click="reset">搜索</el-button>
      <el-button v-if="!isUser()" type="primary" plain @click="openCreate">新建分组</el-button>
    </div>
    <el-table
      v-loading="loading"
      :data="isUser() ? users : groups"
      stripe
      size="small"
      max-height="380"
      :empty-text="emptyText"
      @selection-change="onSelectionChange"
    >
      <el-table-column
        type="selection"
        width="42"
        :selectable="(row: UserPick | PlanGroupPick) => !row.is_member"
      />
      <template v-if="isUser()">
        <el-table-column label="用户名" prop="username" min-width="130" />
        <el-table-column label="显示名" min-width="110">
          <template #default="{ row }">{{ row.display_name || '-' }}</template>
        </el-table-column>
        <el-table-column label="邮箱" min-width="170">
          <template #default="{ row }">{{ row.email || '-' }}</template>
        </el-table-column>
        <el-table-column label="来源" width="80">
          <template #default="{ row }">{{ row.source === 'ldap' ? 'LDAP' : '本地' }}</template>
        </el-table-column>
      </template>
      <template v-else>
        <el-table-column label="组名" prop="name" min-width="130" />
        <el-table-column label="描述" min-width="150">
          <template #default="{ row }">{{ row.description || '-' }}</template>
        </el-table-column>
        <el-table-column label="成员数" prop="member_count" width="90" />
      </template>
      <el-table-column label="状态" width="90">
        <template #default="{ row }">
          <el-tag v-if="row.is_member" size="small" effect="plain">已加入</el-tag>
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
    <template #footer>
      <el-button @click="visible = false">取消</el-button>
      <el-button type="primary" :disabled="!selected.length" @click="doAdd">
        添加选中（{{ selected.length }}）
      </el-button>
    </template>
  </el-dialog>

  <el-dialog v-model="createVisible" title="新建分组" width="420px" append-to-body>
    <el-form label-width="80px" @submit.prevent>
      <el-form-item label="组名" required>
        <el-input v-model="createForm.name" maxlength="128" placeholder="如：研发组" @keyup.enter="doCreate" />
      </el-form-item>
      <el-form-item label="描述">
        <el-input v-model="createForm.description" type="textarea" :rows="2" placeholder="可选" />
      </el-form-item>
    </el-form>
    <template #footer>
      <el-button @click="createVisible = false">取消</el-button>
      <el-button type="primary" :loading="creating" @click="doCreate">创建</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.picker-toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 12px;
}
.pager {
  margin-top: 12px;
  justify-content: flex-end;
}
</style>
