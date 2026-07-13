import { MacOSScrollAccel, type ScrollAcceleration } from "@opentui/core"

export interface ScrollConfig {
  readonly scroll_acceleration?: { readonly enabled?: boolean }
  readonly scroll_speed?: number
}

export class CustomSpeedScroll implements ScrollAcceleration {
  constructor(private readonly speed: number) {}

  tick(_now?: number): number {
    return this.speed
  }

  reset(): void {}
}

/** Match OpenCode's scroll policy while making native macOS momentum the default on macOS. */
export function getScrollAcceleration(config?: ScrollConfig): ScrollAcceleration {
  if (config?.scroll_acceleration?.enabled) return new MacOSScrollAccel()
  if (config?.scroll_speed !== undefined) return new CustomSpeedScroll(config.scroll_speed)
  return process.platform === "darwin" ? new MacOSScrollAccel() : new CustomSpeedScroll(3)
}
