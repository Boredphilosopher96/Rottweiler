import type { SoraApi } from '@shared/ipc'

declare global {
  interface Window {
    sora: SoraApi
  }
}

export {}
