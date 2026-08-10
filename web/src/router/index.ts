import { createRouter, createWebHistory } from 'vue-router'
import { useAuthStore } from '@/stores/auth'

const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/login', name: 'login', component: () => import('@/views/Login.vue'), meta: { public: true } },
    {
      path: '/',
      component: () => import('@/views/Layout.vue'),
      children: [
        { path: '', redirect: '/dashboard' },
        { path: 'dashboard', name: 'dashboard', component: () => import('@/views/Dashboard.vue'), meta: { title: '仪表盘' } },
        { path: 'keys', name: 'keys', component: () => import('@/views/Keys.vue'), meta: { title: 'API Keys' } },
        { path: 'usage', name: 'usage', component: () => import('@/views/Usage.vue'), meta: { title: '用量统计' } },
        { path: 'users', name: 'users', component: () => import('@/views/Users.vue'), meta: { title: '用户管理', admin: true } },
        { path: 'audit', name: 'audit', component: () => import('@/views/Audit.vue'), meta: { title: '审计日志', admin: true } },
        {
          path: 'providers',
          name: 'providers',
          component: () => import('@/views/Providers.vue'),
          meta: { title: '供应商配置', admin: true },
        },
        {
          path: 'models',
          name: 'models',
          component: () => import('@/views/Models.vue'),
          meta: { title: '模型库', admin: true },
        },
        {
          path: 'routes',
          name: 'routes',
          component: () => import('@/views/Routes.vue'),
          meta: { title: '路由规则', admin: true },
        },
        {
          path: 'limits',
          name: 'limits',
          component: () => import('@/views/Limits.vue'),
          meta: { title: '限流与配额', admin: true },
        },
        {
          path: 'prices',
          name: 'prices',
          component: () => import('@/views/Prices.vue'),
          meta: { title: '模型价格', admin: true },
        },
        {
          path: 'settings',
          name: 'settings',
          component: () => import('@/views/Settings.vue'),
          meta: { title: '系统设置', admin: true },
        },
      ],
    },
    { path: '/:pathMatch(.*)*', redirect: '/dashboard' },
  ],
})

router.beforeEach(async (to) => {
  const auth = useAuthStore()

  if (to.meta.public) {
    if (auth.isAuthenticated && to.path === '/login') return { path: '/dashboard' }
    return true
  }

  if (!auth.isAuthenticated) return { path: '/login', query: { redirect: to.fullPath } }

  // 已登录但用户信息未加载（如刷新页面）→ 先取 /api/me
  if (!auth.loaded && !auth.user) {
    try {
      await auth.loadMe()
    } catch {
      return { path: '/login' }
    }
  }

  if (to.meta.admin && !auth.isAdmin) return { path: '/dashboard' }
  return true
})

export default router
