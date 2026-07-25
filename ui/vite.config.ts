import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    proxy: {
      '/api': 'http://localhost:9090',
      '/healthz': 'http://localhost:9090',
      '/readyz': 'http://localhost:9090',
      '/metrics': 'http://localhost:9090',
    },
  },
})
