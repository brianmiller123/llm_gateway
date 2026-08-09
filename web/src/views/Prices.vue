<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { request } from '@/api/client'
import type { ModelRow, ModelsResp, PriceRow, PricesResp } from '@/api/types'

const loading = ref(false)
const prices = ref<PriceRow[]>([])
const models = ref<ModelRow[]>([])

/** 模型库去重后的模型 id（下拉选项；仍可手输自定义） */
const modelOptions = computed(() => [...new Set(models.value.map((m) => m.model_id))].sort())

async function loadPrices() {
  loading.value = true
  try {
    const [pricesResp, modelsResp] = await Promise.all([
      request<PricesResp>('/api/admin/prices'),
      request<ModelsResp>('/api/admin/models'),
    ])
    prices.value = pricesResp.prices
    models.value = modelsResp.models
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '加载价格列表失败')
  } finally {
    loading.value = false
  }
}

// —— 新建 ——
const dialogVisible = ref(false)
const saving = ref(false)

interface PriceForm {
  model: string
  input_price_per_m: number | null
  output_price_per_m: number | null
  currency: string
  effective_from: string
}

/** 本地日期 YYYY-MM-DD */
function todayStr(): string {
  const d = new Date()
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`
}

const form = reactive<PriceForm>({
  model: '',
  input_price_per_m: null,
  output_price_per_m: null,
  currency: 'CNY',
  effective_from: todayStr(),
})

function resetForm() {
  form.model = ''
  form.input_price_per_m = null
  form.output_price_per_m = null
  form.currency = 'CNY'
  form.effective_from = todayStr()
}

function openCreate() {
  resetForm()
  dialogVisible.value = true
}

async function doCreate() {
  const model = form.model.trim()
  if (!model) {
    ElMessage.warning('请输入模型名称')
    return
  }
  if (model.length > 128) {
    ElMessage.warning('模型名称不能超过 128 个字符')
    return
  }
  if (!form.effective_from) {
    ElMessage.warning('请选择生效日期')
    return
  }
  saving.value = true
  try {
    await request('/api/admin/prices', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        model,
        // 空价格 = 免费 = null（不是 0）
        input_price_per_m: form.input_price_per_m ?? null,
        output_price_per_m: form.output_price_per_m ?? null,
        currency: form.currency,
        effective_from: form.effective_from,
      }),
    })
    ElMessage.success('价格已创建')
    dialogVisible.value = false
    await loadPrices()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '创建失败')
  } finally {
    saving.value = false
  }
}

// —— 删除 ——
async function removePrice(row: PriceRow) {
  try {
    await ElMessageBox.confirm(
      `删除后该模型的成本按无价（0）计算，确定删除 ${row.model} 的价格吗？`,
      '删除价格',
      { type: 'warning', confirmButtonText: '删除', cancelButtonText: '取消' },
    )
  } catch {
    return // 用户取消
  }
  try {
    await request(`/api/admin/prices/${row.id}`, { method: 'DELETE' })
    ElMessage.success('价格已删除')
    await loadPrices()
  } catch (e) {
    ElMessage.error(e instanceof Error ? e.message : '删除失败')
  }
}

onMounted(loadPrices)
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <span class="desc">每百万 Token 单价，用于成本计量；同模型多条按生效日期取最新</span>
      <el-button type="primary" @click="openCreate">
        <el-icon><Plus /></el-icon>
        <span>新建</span>
      </el-button>
    </div>

    <el-card shadow="never" body-class="table-card">
      <el-table v-loading="loading" :data="prices" stripe>
        <el-table-column label="模型" min-width="180" show-overflow-tooltip>
          <template #default="{ row }"><span class="mono">{{ row.model }}</span></template>
        </el-table-column>
        <el-table-column label="输入价（元/百万tokens）" min-width="160" align="right">
          <template #default="{ row }">{{ row.input_price_per_m ?? '-' }}</template>
        </el-table-column>
        <el-table-column label="输出价（元/百万tokens）" min-width="160" align="right">
          <template #default="{ row }">{{ row.output_price_per_m ?? '-' }}</template>
        </el-table-column>
        <el-table-column prop="currency" label="币种" width="80" />
        <el-table-column label="生效日期" width="130">
          <template #default="{ row }">{{ row.effective_from || '-' }}</template>
        </el-table-column>
        <el-table-column label="操作" width="80" fixed="right">
          <template #default="{ row }">
            <el-button link type="danger" @click="removePrice(row)">删除</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty description="暂无价格配置" />
        </template>
      </el-table>
    </el-card>

    <el-dialog
      v-model="dialogVisible"
      title="新建价格"
      width="480px"
      :close-on-click-modal="false"
      @closed="resetForm"
    >
      <el-form label-width="130px">
        <el-form-item label="模型" required>
          <el-select
            v-model="form.model"
            filterable
            allow-create
            default-first-option
            clearable
            placeholder="选择模型库中的模型，或输入自定义"
            style="width: 100%"
          >
            <el-option v-for="m in modelOptions" :key="m" :label="m" :value="m" />
          </el-select>
        </el-form-item>
        <el-form-item label="输入价（元/百万）">
          <el-input-number
            v-model="form.input_price_per_m"
            :min="0"
            :precision="4"
            :controls="false"
            placeholder="留空表示免费"
            style="width: 100%"
          />
        </el-form-item>
        <el-form-item label="输出价（元/百万）">
          <el-input-number
            v-model="form.output_price_per_m"
            :min="0"
            :precision="4"
            :controls="false"
            placeholder="留空表示免费"
            style="width: 100%"
          />
        </el-form-item>
        <el-form-item label="币种">
          <el-select v-model="form.currency" style="width: 100%">
            <el-option label="CNY（人民币）" value="CNY" />
            <el-option label="USD（美元）" value="USD" />
          </el-select>
        </el-form-item>
        <el-form-item label="生效日期" required>
          <el-date-picker
            v-model="form.effective_from"
            type="date"
            value-format="YYYY-MM-DD"
            placeholder="选择生效日期"
            style="width: 100%"
          />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="doCreate">确定</el-button>
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

.mono {
  font-family: 'JetBrains Mono', Consolas, 'Courier New', monospace;
  font-size: 13px;
}
</style>
