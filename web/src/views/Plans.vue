<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { request } from '@/api/client'
import type { CodingPlan, PlansResp, PeriodType, OverageAction } from '@/api/types'
import PlanMembersDialog from '@/components/PlanMembersDialog.vue'

const loading = ref(false)
const plans = ref<PlansResp['plans']>([])

const PERIOD_LABELS: Record<PeriodType, string> = {
  hourly: '小时级重置',
  daily: '自然日重置',
  monthly: '自然月重置',
  total: '总量不重置',
}
function periodText(pt: PeriodType, hours?: number): string {
  return pt === 'hourly' ? `每 ${hours ?? 1} 小时重置` : PERIOD_LABELS[pt]
}
const OVERAGE_LABELS: Record<OverageAction, string> = {
  block: '拦截请求（429）',
  downgrade: '降级到指定模型',
  log: '仅记录并告警',
}
const CHANNEL_LABELS: Record<string, string> = {
  in_site: '站内通知',
  email: '邮件',
  webhook: 'Webhook',
}

function windowText(p: CodingPlan): string {
  if (!p.active_start || !p.active_end) return '全天'
  return `${p.active_start.slice(0, 5)} - ${p.active_end.slice(0, 5)}`
}
function percent(used: number, limit: number): number {
  if (limit <= 0) return 0
  return Math.round((used * 10000) / limit) / 100
}

function usageStatus(p: number): string {
  if (p >= 100) return 'exception'
  if (p >= 80) return 'warning'
  return 'success'
}

async function loadPlans() {
  loading.value = true
  try {
    const resp = await request<PlansResp>('/api/admin/plans')
    plans.value = resp.plans
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载 Plan 列表失败')
  } finally {
    loading.value = false
  }
}

// —— 成员管理（直连用户 / 加入分组）——
const membersVisible = ref(false)
const membersPlan = ref<CodingPlan | null>(null)

function openMembers(p: CodingPlan) {
  membersPlan.value = p
  membersVisible.value = true
}

// —— 新建/编辑（token 上限 = 数字 + 单位，如 1.5G）——
const dialogVisible = ref(false)
const editing = ref<CodingPlan | null>(null)
const saving = ref(false)
const form = reactive({
  name: '',
  description: '',
  priority: 0,
  limit_value: 500,
  limit_unit: 'M',
  period_type: 'monthly' as PeriodType,
  period_hours: 1,
  period_anchor_mode: 'fixed' as 'fixed' | 'join',
  overage_action: 'block' as OverageAction,
  downgrade_model: '',
  alert_channels: ['in_site'] as string[],
  active_enabled: false,
  active_start: '',
  active_end: '',
  webhook_url: '',
  enabled: true,
})

const UNITS = ['K', 'M', 'G', 'T']

function tokenLimitString(): string {
  return `${form.limit_value}${form.limit_unit}`
}

function openCreate() {
  editing.value = null
  form.name = ''
  form.description = ''
  form.priority = 0
  form.limit_value = 500
  form.limit_unit = 'M'
  form.period_type = 'monthly'
  form.period_hours = 1
  form.period_anchor_mode = 'fixed'
  form.downgrade_model = ''
  form.alert_channels = ['in_site']
  form.webhook_url = ''
  form.enabled = true
  form.active_enabled = false
  form.active_start = ''
  form.active_end = ''
  dialogVisible.value = true
}

function openEdit(p: CodingPlan) {
  editing.value = p
  form.name = p.name
  form.description = p.description
  form.priority = p.priority
  // 拆 display 尾缀单位（1.5G → 1.5 / G；无单位 = 纯数字）
  const m = p.token_limit_display.match(/^([\d.]+)([KMGT]?)$/)
  if (m) {
    form.limit_value = Number(m[1])
    form.limit_unit = m[2] || 'K'
  }
  form.period_type = p.period_type
  form.period_hours = p.period_hours
  form.period_anchor_mode = p.period_anchor_mode
  form.downgrade_model = p.downgrade_model ?? ''
  form.alert_channels = p.alert_channels.length ? p.alert_channels : ['in_site']
  form.webhook_url = p.webhook_url
  form.enabled = p.enabled
  const hasWindow = !!(p.active_start && p.active_end)
  form.active_enabled = hasWindow
  form.active_start = p.active_start?.slice(0, 5) ?? ''
  form.active_end = p.active_end?.slice(0, 5) ?? ''
  dialogVisible.value = true
}

async function doSave() {
  if (!form.name.trim()) {
    ElMessage.warning('请输入 Plan 名称')
    return
  }
  if (!(form.limit_value > 0)) {
    ElMessage.warning('请输入大于 0 的 token 上限')
    return
  }
  if (form.overage_action === 'downgrade' && !form.downgrade_model.trim()) {
    ElMessage.warning('降级策略需要指定降级目标模型')
    return
  }
  if (form.active_enabled && (!form.active_start || !form.active_end)) {
    ElMessage.warning('请选择生效时段的起止时间')
    return
  }
  if (form.active_enabled && form.active_start === form.active_end) {
    ElMessage.warning('起止时间相同；全天启用请关闭「自定义生效时段」')
    return
  }
  saving.value = true
  const body = {
    name: form.name.trim(),
    description: form.description.trim(),
    priority: form.priority,
    token_limit: tokenLimitString(),
    period_type: form.period_type,
    period_hours: form.period_type === 'hourly' ? form.period_hours : undefined,
    period_anchor_mode: form.period_type === 'hourly' ? form.period_anchor_mode : undefined,
    overage_action: form.overage_action,
    downgrade_model: form.overage_action === 'downgrade' ? form.downgrade_model.trim() : null,
    alert_channels: form.alert_channels,
    webhook_url: form.webhook_url.trim(),
    active_window: form.active_enabled
      ? { start: form.active_start, end: form.active_end }
      : { start: null, end: null },
    enabled: form.enabled,
  }
  try {
    if (editing.value) {
      await request(`/api/admin/plans/${editing.value.id}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      ElMessage.success('Plan 已更新，新配额即时生效')
    } else {
      await request('/api/admin/plans', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      ElMessage.success('Plan 已创建')
    }
    dialogVisible.value = false
    await loadPlans()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    saving.value = false
  }
}

async function toggleEnabled(p: CodingPlan) {
  if (p.enabled) {
    try {
      await ElMessageBox.confirm(
        `停用后绑定该 Plan 的分组将回退到其他分组的 Plan 或暂不限额，确定停用「${p.name}」吗？`,
        '停用 Plan',
        { type: 'warning', confirmButtonText: '停用', cancelButtonText: '取消' },
      )
    } catch {
      return
    }
  }
  try {
    await request(`/api/admin/plans/${p.id}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enabled: !p.enabled }),
    })
    ElMessage.success(p.enabled ? '已停用' : '已启用')
    await loadPlans()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '操作失败')
  }
}

async function doDelete(p: CodingPlan) {
  try {
    await ElMessageBox.confirm(
      `删除后历史用量保留、绑定分组自动解绑（成员回退其他分组的 Plan 或暂不限额），确定删除「${p.name}」吗？`,
      '删除 Plan',
      { type: 'warning', confirmButtonText: '删除', cancelButtonText: '取消' },
    )
  } catch {
    return
  }
  try {
    const resp = await request<{ affected_groups: number; affected_direct_users: number }>(
      `/api/admin/plans/${p.id}`,
      { method: 'DELETE' },
    )
    ElMessage.success(
      `已删除（解绑 ${resp.affected_groups} 个分组，移除 ${resp.affected_direct_users} 个直连用户）`,
    )
    await loadPlans()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '删除失败')
  }
}

onMounted(loadPlans)
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <div class="title">Coding Plan（用量配额包）</div>
      <el-button type="primary" @click="openCreate">新建 Plan</el-button>
    </div>

    <el-card shadow="never" body-class="table-card">
      <el-table v-loading="loading" :data="plans" stripe>
        <el-table-column label="名称" min-width="140">
          <template #default="{ row }">
            <div class="plan-name">
              {{ row.name }}
              <el-tag v-if="!row.enabled" size="small" type="info" effect="plain">已停用</el-tag>
            </div>
            <div class="plan-desc">{{ row.description || '-' }}</div>
          </template>
        </el-table-column>
        <el-table-column label="优先级" prop="priority" width="80" />
        <el-table-column label="token 上限" width="120">
          <template #default="{ row }">{{ row.token_limit_display }}</template>
        </el-table-column>
        <el-table-column label="统计周期" width="130">
          <template #default="{ row }">{{ periodText(row.period_type, row.period_hours) }}</template>
        </el-table-column>
        <el-table-column label="生效时段" width="120">
          <template #default="{ row }">{{ windowText(row) }}</template>
        </el-table-column>
        <el-table-column label="超额策略" min-width="150">
          <template #default="{ row }">
            {{ OVERAGE_LABELS[row.overage_action as OverageAction] }}
            <div v-if="row.downgrade_model" class="plan-desc">→ {{ row.downgrade_model }}</div>
          </template>
        </el-table-column>
        <el-table-column label="当期用量" min-width="180">
          <template #default="{ row }">
            <el-progress
              :percentage="Math.min(percent(row.used_tokens, row.token_limit), 100)"
              :status="usageStatus(percent(row.used_tokens, row.token_limit))"
              :stroke-width="8"
            />
            <div class="plan-desc">
              {{ row.used_tokens.toLocaleString() }} / {{ row.token_limit_display }}
              · {{ row.active_users }} 活跃用户
            </div>
          </template>
        </el-table-column>
        <el-table-column label="分组 / 成员" width="110">
          <template #default="{ row }">{{ row.group_count }} / {{ row.member_count }}</template>
        </el-table-column>
        <el-table-column label="告警渠道" min-width="120">
          <template #default="{ row }">
            <el-tag
              v-for="c in row.alert_channels"
              :key="c"
              size="small"
              effect="plain"
              class="channel-tag"
            >
              {{ CHANNEL_LABELS[c] ?? c }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="操作" width="240" fixed="right">
          <template #default="{ row }">
            <el-button link type="primary" @click="openEdit(row)">编辑</el-button>
            <el-button link :type="row.enabled ? 'warning' : 'success'" @click="toggleEnabled(row)">
              {{ row.enabled ? '停用' : '启用' }}
            </el-button>
            <el-button link type="primary" @click="openMembers(row)">成员</el-button>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-dialog
      v-model="dialogVisible"
      :title="editing ? `编辑 Plan：${editing.name}` : '新建 Plan'"
      width="560px"
    >
      <el-form label-width="110px">
        <el-form-item label="名称" required>
          <el-input v-model="form.name" placeholder="如：Pro 团队套餐" maxlength="128" />
        </el-form-item>
        <el-form-item label="描述">
          <el-input v-model="form.description" type="textarea" :rows="2" placeholder="可选" />
        </el-form-item>
        <el-form-item label="优先级">
          <el-input-number v-model="form.priority" :min="-1000" :max="1000" />
          <span class="hint">用户同属多分组时取优先级最高的 Plan 生效</span>
        </el-form-item>
        <el-form-item label="token 上限" required>
          <div class="limit-row">
            <el-input-number v-model="form.limit_value" :min="0.001" :precision="3" :step="100" controls-position="right" />
            <el-select v-model="form.limit_unit" class="unit-select">
              <el-option v-for="u in UNITS" :key="u" :label="u" :value="u" />
            </el-select>
            <span class="hint">= {{ tokenLimitString() }}</span>
          </div>
        </el-form-item>
        <el-form-item label="统计周期">
          <el-radio-group v-model="form.period_type">
            <el-radio-button value="hourly">小时级重置</el-radio-button>
            <el-radio-button value="daily">自然日重置</el-radio-button>
            <el-radio-button value="monthly">自然月重置</el-radio-button>
            <el-radio-button value="total">总量不重置</el-radio-button>
          </el-radio-group>
        </el-form-item>
        <el-form-item v-if="form.period_type === 'hourly'" label="重置间隔">
          <div class="limit-row">
            <el-input-number v-model="form.period_hours" :min="1" :max="168" :step="1" controls-position="right" />
            <span class="hint">每 N 小时清零一次（1..168）</span>
          </div>
        </el-form-item>
        <el-form-item v-if="form.period_type === 'hourly'" label="重置锚点">
          <el-radio-group v-model="form.period_anchor_mode">
            <el-radio-button value="fixed">固定整点（UTC）</el-radio-button>
            <el-radio-button value="join">按开通时间偏移</el-radio-button>
          </el-radio-group>
          <div class="hint">
            固定整点：从 UTC 1970-01-01 起每 N 小时切窗；开通时间：以成员加入分组时刻为锚点
          </div>
        </el-form-item>
        <el-form-item label="生效时段">
          <div>
            <el-switch v-model="form.active_enabled" active-text="自定义时段" />
            <div v-if="form.active_enabled" class="limit-row" style="margin-top: 8px">
              <el-time-picker
                v-model="form.active_start"
                value-format="HH:mm"
                format="HH:mm"
                placeholder="开始（如 09:00）"
              />
              <span class="hint">至</span>
              <el-time-picker
                v-model="form.active_end"
                value-format="HH:mm"
                format="HH:mm"
                placeholder="结束（如 18:00）"
              />
            </div>
            <div class="hint" style="margin-top: 4px">
              仅该时段内本 Plan 参与生效（按服务器时区；支持跨零点，如 22:00 - 06:00）
            </div>
          </div>
        </el-form-item>
        <el-form-item label="超额策略">
          <el-radio-group v-model="form.overage_action">
            <el-radio-button value="block">拦截请求</el-radio-button>
            <el-radio-button value="downgrade">降级模型</el-radio-button>
            <el-radio-button value="log">仅记录告警</el-radio-button>
          </el-radio-group>
        </el-form-item>
        <el-form-item v-if="form.overage_action === 'downgrade'" label="降级目标模型" required>
          <el-input v-model="form.downgrade_model" placeholder="如 gpt-4o-mini（须已配置路由）" />
        </el-form-item>
        <el-form-item label="告警渠道">
          <el-checkbox-group v-model="form.alert_channels">
            <el-checkbox value="in_site">站内通知</el-checkbox>
            <el-checkbox value="email">邮件（需配置 SMTP）</el-checkbox>
            <el-checkbox value="webhook">Webhook</el-checkbox>
          </el-checkbox-group>
        </el-form-item>
        <el-form-item v-if="form.alert_channels.includes('webhook')" label="Webhook URL">
          <el-input v-model="form.webhook_url" placeholder="https://example.com/hook" />
        </el-form-item>
        <el-form-item label="启用">
          <el-switch v-model="form.enabled" />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="doSave">保存</el-button>
      </template>
    </el-dialog>

    <PlanMembersDialog v-model="membersVisible" :plan="membersPlan" @changed="loadPlans" />
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
.plan-name {
  font-weight: 500;
}
.plan-desc {
  font-size: 12px;
  color: #909399;
  margin-top: 2px;
}
.channel-tag {
  margin-right: 4px;
}
.limit-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.unit-select {
  width: 76px;
}
.hint {
  font-size: 12px;
  color: #909399;
}
</style>
