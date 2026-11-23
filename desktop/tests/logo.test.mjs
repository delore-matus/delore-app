import assert from 'node:assert/strict'
import { readFileSync, statSync } from 'node:fs'
import test from 'node:test'

test('official logo asset renders in desktop and website branding', () => {
  const desktopLogo = 'public/logo.png'
  const websiteLogo = '../website/logo.png'
  assert.ok(statSync(desktopLogo).size > 1000)
  assert.ok(statSync(websiteLogo).size > 1000)

  const desktopSource = readFileSync('src/main.ts', 'utf8')
  const desktopCss = readFileSync('src/style.css', 'utf8')
  const websiteSource = readFileSync('../website/index.html', 'utf8')

  assert.match(desktopSource, /<img src="\/logo\.png" alt="DeLore"/)
  assert.match(desktopCss, /\.wordmark img/)
  assert.match(websiteSource, /src="\/logo\.png" alt="DeLore"/)
})
