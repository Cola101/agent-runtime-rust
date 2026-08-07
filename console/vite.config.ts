import { configDefaults, defineConfig } from 'vitest/config'
import vue from '@vitejs/plugin-vue'
import { createLocalDevProxy } from './vite.local-proxy'

export default defineConfig({
  plugins: [vue()],
  server: {
    proxy: createLocalDevProxy(process.env),
  },
  test: {
    environment: 'jsdom',
    exclude: [...configDefaults.exclude, 'e2e/**', 'e2e-live/**'],
  },
})
