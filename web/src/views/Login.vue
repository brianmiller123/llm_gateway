<template>
  <div class="login-page">
    <el-card class="login-card">
      <div class="brand">
        <div class="logo">LG</div>
        <h1>LLM Gateway 控制台</h1>
        <p class="sub">企业 LLM API 网关 · 登录后管理 API Key 与用量</p>
      </div>

      <el-form :model="form" size="large" @submit.prevent="onSubmit">
        <el-form-item>
          <el-input
            v-model="form.username"
            placeholder="用户名（LDAP / 本地账号）"
            :prefix-icon="User"
            autofocus
            autocomplete="username"
            @keyup.enter="onSubmit"
          />
        </el-form-item>
        <el-form-item>
          <el-input
            v-model="form.password"
            type="password"
            placeholder="密码"
            :prefix-icon="Lock"
            show-password
            autocomplete="current-password"
            @keyup.enter="onSubmit"
          />
        </el-form-item>
        <el-form-item>
          <el-button type="primary" class="submit" :loading="loading" native-type="submit">
            登 录
          </el-button>
        </el-form-item>
      </el-form>
    </el-card>
    <router-link to="/status" class="status-link">服务状态页 →</router-link>
  </div>
</template>

<script setup lang="ts">
import { reactive, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { User, Lock } from '@element-plus/icons-vue'
import { useAuthStore } from '@/stores/auth'
import { ApiError } from '@/api/client'

const router = useRouter()
const route = useRoute()
const auth = useAuthStore()

const form = reactive({ username: '', password: '' })
const loading = ref(false)

async function onSubmit() {
  if (!form.username.trim() || !form.password) {
    ElMessage.warning('请输入用户名和密码')
    return
  }
  loading.value = true
  try {
    await auth.login(form.username.trim(), form.password)
    ElMessage.success(`欢迎，${auth.user?.display_name ?? form.username}`)
    const redirect = (route.query.redirect as string) || '/dashboard'
    router.push(redirect).catch((e) => console.error('[login] push failed:', e))
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '登录失败，请稍后重试')
  } finally {
    loading.value = false
  }
}
</script>

<style scoped>
.login-page {
  min-height: 100vh;
  display: flex;
  align-items: center;
  justify-content: center;
  background: linear-gradient(135deg, #1f2d3d 0%, #2b3a4d 100%);
}
.login-card {
  width: 400px;
  border-radius: 12px;
}
.brand {
  text-align: center;
  margin-bottom: 24px;
}
.logo {
  width: 56px;
  height: 56px;
  margin: 0 auto 12px;
  border-radius: 14px;
  background: #409eff;
  color: #fff;
  font-size: 22px;
  font-weight: 700;
  display: flex;
  align-items: center;
  justify-content: center;
}
.brand h1 {
  font-size: 20px;
  margin: 0 0 6px;
  color: #303133;
}
.sub {
  font-size: 13px;
  color: #909399;
  margin: 0;
}
.submit {
  width: 100%;
}
.status-link {
  margin-top: 16px;
  font-size: 13px;
  color: #aeb9c7;
  text-decoration: none;
}
.status-link:hover {
  color: #fff;
}
</style>
