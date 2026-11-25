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

test('About advanced section shows a friendly read-only connection address', () => {
  const desktopSource = readFileSync('src/main.ts', 'utf8')
  const aboutStart = desktopSource.indexOf('<p class="eyebrow">ABOUT</p>')
  assert.notEqual(aboutStart, -1)
  const aboutEnd = desktopSource.indexOf('</section>', aboutStart)
  const aboutSection = desktopSource.slice(aboutStart, aboutEnd)

  assert.match(aboutSection, /<summary>Advanced<\/summary>/)
  assert.match(aboutSection, /Where DeLore connects/)
  assert.match(aboutSection, /id="coordinator-address"[^>]*readonly/)
  assert.doesNotMatch(aboutSection, /coordinator address|WebSocket|ws:\/\//i)
})
