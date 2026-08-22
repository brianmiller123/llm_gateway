<template>
  <el-container class="layout">
    <el-aside width="210px" class="aside">
      <div class="aside-brand">
        <span class="logo">LG</span>
        <span class="title">LLM Gateway</span>
      </div>
      <el-menu
        :default-active="$route.path"
        router
        background-color="#1f2d3d"
        text-color="#aeb9c7"
        active-text-color="#ffffff"
        class="menu"
      >
        <el-menu-item index="/dashboard">
          <el-icon><Odometer /></el-icon><span>仪表盘</span>
        </el-menu-item>
        <el-menu-item index="/keys">
          <el-icon><Key /></el-icon><span>API Keys</span>
        </el-menu-item>
        <el-menu-item index="/usage">
          <el-icon><DataLine /></el-icon><span>用量统计</span>
        </el-menu-item>
        <template v-if="auth.isAdmin">
          <el-menu-item index="/users">
            <el-icon><UserFilled /></el-icon><span>用户管理</span>
          </el-menu-item>
          <el-menu-item index="/audit">
            <el-icon><Document /></el-icon><span>审计日志</span>
          </el-menu-item>
          <el-menu-item index="/monitor">
            <el-icon><Monitor /></el-icon><span>实时监控</span>
          </el-menu-item>
          <el-sub-menu index="config">
            <template #title>
              <el-icon><Setting /></el-icon><span>系统配置</span>
            </template>
            <el-menu-item index="/providers">
              <el-icon><Connection /></el-icon><span>供应商配置</span>
            </el-menu-item>
            <el-menu-item index="/models">
              <el-icon><Grid /></el-icon><span>模型库</span>
            </el-menu-item>
            <el-menu-item index="/routes">
              <el-icon><Share /></el-icon><span>路由规则</span>
            </el-menu-item>
            <el-menu-item index="/advanced-request">
              <el-icon><MagicStick /></el-icon><span>高级请求配置</span>
            </el-menu-item>
            <el-menu-item index="/limits">
              <el-icon><Timer /></el-icon><span>限流与配额</span>
            </el-menu-item>
            <el-menu-item index="/api-endpoints">
              <el-icon><Link /></el-icon><span>API 端点</span>
            </el-menu-item>
          </el-sub-menu>
        </template>
        <el-menu-item index="/status">
          <el-icon><CircleCheck /></el-icon><span>服务状态</span>
        </el-menu-item>
      </el-menu>
    </el-aside>

    <el-container>
      <el-header class="header">
        <div class="page-title">{{ $route.meta.title ?? '' }}</div>
        <el-dropdown @command="onCommand">
          <span class="user-chip">
            <el-avatar :size="30" class="avatar">{{ initial }}</el-avatar>
            <span class="name">{{ auth.user?.display_name ?? auth.user?.username }}</span>
            <el-tag size="small" :type="auth.user?.source === 'ldap' ? 'warning' : 'success'" effect="plain">
              {{ auth.user?.source === 'ldap' ? 'LDAP' : '本地' }}
            </el-tag>
            <el-icon class="caret"><ArrowDown /></el-icon>
          </span>
          <template #dropdown>
            <el-dropdown-menu>
              <el-dropdown-item disabled>
                {{ auth.user?.username }}<template v-if="auth.isAdmin">（管理员）</template>
              </el-dropdown-item>
              <el-dropdown-item divided command="status">服务状态页</el-dropdown-item>
              <el-dropdown-item divided command="logout">退出登录</el-dropdown-item>
            </el-dropdown-menu>
          </template>
        </el-dropdown>
      </el-header>

      <el-main class="main">
        <router-view />
      </el-main>
    </el-container>
  </el-container>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessageBox } from 'element-plus'
import { useAuthStore } from '@/stores/auth'

const auth = useAuthStore()
const router = useRouter()

const initial = computed(() => (auth.user?.display_name ?? auth.user?.username ?? '?').charAt(0).toUpperCase())

async function onCommand(cmd: string) {
  if (cmd === 'status') {
    router.push('/status')
    return
  }
  if (cmd !== 'logout') return
  await ElMessageBox.confirm('确定退出登录吗？', '提示', { type: 'warning' })
  await auth.logout()
  router.push('/login')
}
</script>

<style scoped>
.layout {
  min-height: 100vh;
}
.aside {
  background: #1f2d3d;
  display: flex;
  flex-direction: column;
}
.aside-brand {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 18px 16px;
  color: #fff;
}
.aside-brand .logo {
  width: 30px;
  height: 30px;
  border-radius: 8px;
  background: #409eff;
  color: #fff;
  font-size: 13px;
  font-weight: 700;
  display: flex;
  align-items: center;
  justify-content: center;
}
.aside-brand .title {
  font-size: 15px;
  font-weight: 600;
}
.menu {
  border-right: none;
  flex: 1;
}
.menu :deep(.el-menu-item.is-active) {
  background: #2b4b6d;
}
.header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  border-bottom: 1px solid #e4e7ed;
  background: #fff;
}
.page-title {
  font-size: 16px;
  font-weight: 600;
  color: #303133;
}
.user-chip {
  display: flex;
  align-items: center;
  gap: 8px;
  cursor: pointer;
  outline: none;
}
.avatar {
  background: #409eff;
}
.name {
  font-size: 14px;
  color: #303133;
}
.caret {
  color: #909399;
}
.main {
  background: #f5f7fa;
}
</style>
