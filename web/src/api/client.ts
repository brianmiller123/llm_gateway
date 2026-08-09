// 控制台 API 客户端：Bearer 注入、401 自动 refresh 重试一次、统一错误提取。
import type { LoginResp } from './types'

const TOKEN_KEY = 'lg_access_token'
const RT_KEY = 'lg_refresh_token'

export const tokenStorage = {
  get access(): string | null {
    return localStorage.getItem(TOKEN_KEY)
  },
  get refresh(): string | null {
    return localStorage.getItem(RT_KEY)
  },
  set(access: string, refresh: string) {
    localStorage.setItem(TOKEN_KEY, access)
    localStorage.setItem(RT_KEY, refresh)
  },
  clear() {
    localStorage.removeItem(TOKEN_KEY)
    localStorage.removeItem(RT_KEY)
  },
}

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

async function tryRefresh(): Promise<boolean> {
  const rt = tokenStorage.refresh
  if (!rt) return false
  try {
    const res = await fetch('/api/auth/refresh', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ refresh_token: rt }),
    })
    if (!res.ok) return false
    const data = (await res.json()) as LoginResp
    tokenStorage.set(data.access_token, data.refresh_token)
    return true
  } catch {
    return false
  }
}

/** 会话失效（refresh 也失败）→ 清存储回登录页 */
export function forceLogout() {
  tokenStorage.clear()
  if (!window.location.pathname.startsWith('/login')) {
    window.location.href = '/login'
  }
}

export async function request<T>(path: string, opts: RequestInit = {}, retried = false): Promise<T> {
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    ...((opts.headers as Record<string, string>) ?? {}),
  }
  const token = tokenStorage.access
  if (token) headers.Authorization = `Bearer ${token}`

  const res = await fetch(path, { ...opts, headers })

  // access 过期：尝试 refresh 一次后重放
  if (res.status === 401 && !retried && !path.startsWith('/api/auth/')) {
    if (await tryRefresh()) return request<T>(path, opts, true)
    forceLogout()
    throw new ApiError(401, 'unauthorized', '登录已过期，请重新登录')
  }

  if (!res.ok) {
    let code = 'error'
    let message = `HTTP ${res.status}`
    try {
      const body = (await res.json()) as { error?: { code?: string; message?: string } }
      code = body.error?.code ?? code
      message = body.error?.message ?? message
    } catch {
      /* 非 JSON 错误体 */
    }
    throw new ApiError(res.status, code, message)
  }
  return (await res.json()) as T
}

export const api = {
  login: (username: string, password: string) =>
    request<LoginResp>('/api/auth/login', {
      method: 'POST',
      body: JSON.stringify({ username, password }),
    }),
  logout: (refreshToken: string) =>
    request<{ ok: boolean }>('/api/auth/logout', {
      method: 'POST',
      body: JSON.stringify({ refresh_token: refreshToken }),
    }),
}
