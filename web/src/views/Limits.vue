<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { request } from '@/api/client'
import type { RateRuleRow, RateRulesResp } from '@/api/types'

/** 格式化 ISO 时间戳为 YYYY-MM-DD HH:mm，空值显示 '-' */
function fmt(iso: string | null): string {
  if (!iso) return '-'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '-'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

// ============ 限流规则（并发上限） ============
const scopeLabels: Record<string, string> = {
  global: '全局',
  user: '用户',
  api_key: 'API Key',
}

const scopeTagTypes: Record<string, 'primary' | 'warning' | 'success'> = {
  global: 'primary',
  user: 'warning',
  api_key: 'success',
}

const rulesLoading = ref(false)
const rules = ref<RateRuleRow[]>([])

async function loadRules() {
  rulesLoading.value = true
  try {
    const resp = await request<RateRulesResp>('/api/admin/rate-limits')
    rules.value = resp.rules
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载限流规则失败')
  } finally {
    rulesLoading.value = false
  }
}

// —— 新建/编辑 ——
const rateVisible = ref(false)
const rateSaving = ref(false)

interface RateForm {
  id: number | null
  scope: string
  scope_id: number | null
  /** 模型限定（'' = 所有模型，提交 null） */
  model: string
  /** 并发上限（0 = 不限） */
  concurrency: number | null
  enabled: boolean
}

const rateForm = reactive<RateForm>({
  id: null,
  scope: 'global',
  scope_id: null,
  model: '',
  concurrency: 0,
  enabled: true,
})

function resetRateForm() {
  rateForm.id = null
  rateForm.scope = 'global'
  rateForm.scope_id = null
  rateForm.model = ''
  rateForm.concurrency = 0
  rateForm.enabled = true
}

function openCreateRate() {
  resetRateForm()
  rateVisible.value = true
}

function openEditRate(row: RateRuleRow) {
  rateForm.id = row.id
  rateForm.scope = row.scope
  rateForm.scope_id = row.scope_id
  rateForm.model = row.model ?? ''
  rateForm.concurrency = row.concurrency
  rateForm.enabled = row.enabled
  rateVisible.value = true
}

/** 切到 global 时清空 scope_id（PATCH 传 null 表示清空绑定） */
function onRateScopeChange() {
  if (rateForm.scope === 'global') rateForm.scope_id = null
}

async function submitRate() {
  if (!rateForm.scope) {
    ElMessage.warning('请选择作用域')
    return
  }
  if (rateForm.scope !== 'global' && rateForm.scope_id === null) {
    ElMessage.warning('非 global 作用域必须填写 scope_id')
    return
  }
  if (rateForm.concurrency === null || rateForm.concurrency < 0 || !Number.isInteger(rateForm.concurrency)) {
    ElMessage.warning('并发上限必须为不小于 0 的整数（0 = 不限）')
    return
  }
  rateSaving.value = true
  try {
    const payload = {
      scope: rateForm.scope,
      // global 无 ID；PATCH 时传 null 表示清空原有绑定
      scope_id: rateForm.scope === 'global' ? null : rateForm.scope_id,
      // 留空 = 所有模型（提交 null）
      model: rateForm.model.trim() || null,
      concurrency: rateForm.concurrency,
      enabled: rateForm.enabled,
    }
    if (rateForm.id === null) {
      await request('/api/admin/rate-limits', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      })
      ElMessage.success('限流规则已创建')
    } else {
      await request(`/api/admin/rate-limits/${rateForm.id}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      })
      ElMessage.success('限流规则已更新')
    }
    rateVisible.value = false
    await loadRules()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '保存失败')
  } finally {
    rateSaving.value = false
  }
}

// —— 删除 ——
async function removeRate(row: RateRuleRow) {
  const scopeText = `${scopeLabels[row.scope] ?? row.scope}${row.scope_id !== null ? ` #${row.scope_id}` : ''}`
  const modelText = row.model ? `（模型 ${row.model}）` : ''
  try {
    await ElMessageBox.confirm(
      `删除后 ${scopeText}${modelText} 将不再受限流保护，确定删除该规则吗？`,
      '删除限流规则',
      { type: 'warning', confirmButtonText: '删除', cancelButtonText: '取消' },
    )
  } catch {
    return // 用户取消
  }
  try {
    await request(`/api/admin/rate-limits/${row.id}`, { method: 'DELETE' })
    ElMessage.success('限流规则已删除')
    await loadRules()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '删除失败')
  }
}

onMounted(loadRules)
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <span class="desc">并发上限：全局 / 用户 / API Key 三层规则同时生效，任一命中桶在途请求数满员即 429 + Retry-After；可选模型限定（按客户端模型名精确匹配，独立计量）</span>
      <el-button type="primary" @click="openCreateRate">
        <el-icon><Plus /></el-icon>
        <span>新建规则</span>
      </el-button>
    </div>

    <el-card shadow="never" body-class="table-card">
      <el-table v-loading="rulesLoading" :data="rules" stripe>
        <el-table-column label="作用域" width="110">
          <template #default="{ row }">
            <el-tag :type="scopeTagTypes[row.scope] ?? 'info'" size="small">
              {{ scopeLabels[row.scope] ?? row.scope }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="scope_id" width="100">
          <template #default="{ row }">{{ row.scope_id ?? '-' }}</template>
        </el-table-column>
        <el-table-column label="模型" min-width="160">
          <template #default="{ row }">
            <span v-if="row.model" class="mono">{{ row.model }}</span>
            <span v-else class="sub">全部模型</span>
          </template>
        </el-table-column>
        <el-table-column label="并发上限" width="110" align="right">
          <template #default="{ row }">
            <span v-if="row.concurrency > 0">{{ row.concurrency }}</span>
            <span v-else class="sub">不限</span>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="80">
          <template #default="{ row }">
            <el-tag :type="row.enabled ? 'success' : 'danger'" size="small">
              {{ row.enabled ? '启用' : '禁用' }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="更新时间" min-width="140">
          <template #default="{ row }">{{ fmt(row.updated_at) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="140" fixed="right">
          <template #default="{ row }">
            <el-button link type="primary" @click="openEditRate(row)">编辑</el-button>
            <el-button link type="danger" @click="removeRate(row)">删除</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty description="暂无限流规则" />
        </template>
      </el-table>
    </el-card>

    <!-- 限流规则：新建/编辑 -->
    <el-dialog
      v-model="rateVisible"
      :title="rateForm.id === null ? '新建限流规则' : '编辑限流规则'"
      width="480px"
      :close-on-click-modal="false"
      @closed="resetRateForm"
    >
      <el-form label-width="130px">
        <el-form-item label="作用域">
          <el-select v-model="rateForm.scope" placeholder="请选择作用域" style="width: 100%" @change="onRateScopeChange">
            <el-option label="全局 global" value="global" />
            <el-option label="用户 user" value="user" />
            <el-option label="API Key api_key" value="api_key" />
          </el-select>
        </el-form-item>
        <el-form-item label="scope_id">
          <el-input-number
            v-model="rateForm.scope_id"
            :min="1"
            :controls="false"
            :disabled="rateForm.scope === 'global'"
            placeholder="用户或 API Key 的 ID"
            style="width: 100%"
          />
          <div v-if="rateForm.scope !== 'global'" class="form-tip">
            必填；清空保存表示解除绑定（传 null）
          </div>
        </el-form-item>
        <el-form-item label="模型">
          <el-input
            v-model="rateForm.model"
            placeholder="留空 = 所有模型"
            maxlength="128"
            clearable
            style="width: 100%"
          />
          <div class="form-tip">模型限定：按客户端请求的模型名精确匹配，独立计量</div>
        </el-form-item>
        <el-form-item label="并发上限">
          <el-input-number
            v-model="rateForm.concurrency"
            :min="0"
            :step="1"
            :controls="false"
            placeholder="0 = 不限并发"
            style="width: 100%"
          />
          <div class="form-tip">同时在途的请求数上限：从进入管线起计数，响应流式传输结束（或客户端断开）后释放；0 = 不限。多条规则叠加时须全部未满。</div>
        </el-form-item>
        <el-form-item label="启用">
          <el-switch v-model="rateForm.enabled" />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="rateVisible = false">取消</el-button>
        <el-button type="primary" :loading="rateSaving" @click="submitRate">保存</el-button>
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
  gap: 12px;
}

.desc {
  font-size: 14px;
  color: #606266;
}

.form-tip {
  font-size: 12px;
  line-height: 1.4;
  color: #909399;
  margin-top: 4px;
}

.mono {
  font-family: var(--el-font-family-mono);
}

.sub {
  color: #909399;
}
</style>
