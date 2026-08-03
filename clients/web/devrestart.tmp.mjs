import { chromium } from '@playwright/test'
import { execSync } from 'node:child_process'
const sh = (c) => execSync(c, { shell: '/bin/bash', stdio: 'pipe' }).toString().trim()

const browser = await chromium.launch()
const page = await browser.newPage()
await page.addInitScript(() => localStorage.setItem('axon.token', 'dev-probe'))
await page.goto('http://localhost:5199/', { waitUntil: 'networkidle' })
await page.evaluate(() => { window.__mark = true })
console.log('tab running :', await page.evaluate(async () => (await import('/src/build-info.ts')).BUILD_INFO.version))

try { sh('fuser -k 5199/tcp') } catch {}
await new Promise((r) => setTimeout(r, 2000))
sh('cd /opt/adam/matrix-axon-pr114/clients/web && VITE_AXON_WEB_VERSION=after-restart setsid nohup pnpm dev --port 5199 --strictPort > /tmp/devrestart.log 2>&1 < /dev/null &')
await new Promise((r) => setTimeout(r, 9000))
console.log('server serves:', JSON.parse(sh('curl -s http://localhost:5199/version.json')).version)

await page.evaluate(() => {
  const realNow = Date.now.bind(Date); let skew = 0
  Date.now = () => realNow() + skew
  let hidden = true
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden })
  document.dispatchEvent(new Event('visibilitychange'))
  skew = 120000; hidden = false
  document.dispatchEvent(new Event('visibilitychange'))
})
await new Promise((r) => setTimeout(r, 6000))
console.log('tab reloaded:', await page.evaluate(() => window.__mark === undefined).catch(() => 'page gone'))
await browser.close()
