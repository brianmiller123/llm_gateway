import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  server: {
    port: 5173,
    // 开发代理：全部 API 走本地网关（P1 冒烟地址 https://127.0.0.1:8443）
    proxy: {
      '/api': { target: 'https://127.0.0.1:8443', changeOrigin: true, secure: false },
      '/v1': { target: 'https://127.0.0.1:8443', changeOrigin: true, secure: false },
    },
  },
  build: {
    outDir: 'dist',
    chunkSizeWarningLimit: 1600,
  },
})
