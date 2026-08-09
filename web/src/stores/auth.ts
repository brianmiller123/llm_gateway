import { defineStore } from 'pinia'
import { api, tokenStorage, forceLogout } from '@/api/client'
import type { UserInfo } from '@/api/types'
import { request } from '@/api/client'

interface AuthState {
  user: UserInfo | null
  loaded: boolean
  /** 响应式 token（持久化在 localStorage；getter 依赖它而非直接读 localStorage，避免 pinia 缓存陷阱） */
  token: string | null
}

export const useAuthStore = defineStore('auth', {
  state: (): AuthState => ({
    user: null,
    loaded: false,
    token: tokenStorage.access,
  }),

  getters: {
    isAuthenticated: (s) => !!s.token,
    isAdmin: (s) => s.user?.is_admin ?? false,
  },

  actions: {
    async login(username: string, password: string) {
      const resp = await api.login(username, password)
      tokenStorage.set(resp.access_token, resp.refresh_token)
      this.token = resp.access_token
      this.user = resp.user
      this.loaded = true
    },

    async loadMe() {
      if (!this.token) return
      const resp = await request<{ user: UserInfo }>('/api/me')
      this.user = resp.user
      this.loaded = true
    },

    async logout() {
      const rt = tokenStorage.refresh
      if (rt) {
        try {
          await api.logout(rt)
        } catch {
          /* 服务端已失效也无妨，本地必清 */
        }
      }
      this.token = null
      this.user = null
      this.loaded = false
      forceLogout()
    },
  },
})
