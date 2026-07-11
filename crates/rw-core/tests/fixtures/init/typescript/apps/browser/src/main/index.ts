import { app } from 'electron'
import { join } from 'node:path'
import { is, electronApp } from '@electron-toolkit/utils'
import { Browser, registerIpc, type BrowserConfig } from '@sora/engine'
import { runSmoke } from './smoke'

if (process.env['SORA_SMOKE']) {
  app.commandLine.appendSwitch('no-sandbox')
  app.commandLine.appendSwitch('disable-gpu')
  app.commandLine.appendSwitch('disable-dev-shm-usage')
}

let browser: Browser | null = null

app.whenReady().then(() => {
  electronApp.setAppUserModelId('com.milkmanabi.sora')

  const preloadPath = join(__dirname, '../preload/index.js')
  const chrome =
    is.dev && process.env['ELECTRON_RENDERER_URL']
      ? { preloadPath, devUrl: process.env['ELECTRON_RENDERER_URL'] }
      : { preloadPath, fileEntry: join(__dirname, '../renderer/index.html') }

  const config: BrowserConfig = { chrome }
  browser = new Browser(config)
  registerIpc(browser)
  if (process.env['SORA_SMOKE']) runSmoke(browser)
})

app.on('before-quit', () => {
  browser?.saveSession()
})

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit()
})
