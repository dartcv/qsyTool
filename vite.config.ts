import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:8080',
      '/downloads': 'http://127.0.0.1:8080',
      '/healthz': 'http://127.0.0.1:8080',
    },
  },
})
