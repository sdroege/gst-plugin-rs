import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import fs from 'fs'

export default defineConfig({
  plugins: [react()],
  server: {
    https: {
      key: fs.readFileSync('./certificate.key'),
      cert: fs.readFileSync('./certificate.pem'),
    },
    port: 3001,
    host: true
  }
})
