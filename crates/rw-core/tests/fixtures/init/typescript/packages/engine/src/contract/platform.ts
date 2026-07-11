// Platform / OS identity surfaced to the UI for adaptive UX (title-bar style,
// traffic-light insets on macOS, snap hints on Windows, etc). Detected in the
// engine (Electron main), so it reflects the real host OS, not a user agent.

export type OSFamily = 'windows' | 'macos' | 'linux' | 'bsd' | 'unknown'

export interface OSInfo {
  /** coarse family used for most UI branching */
  family: OSFamily
  /** raw process.platform, e.g. 'win32' | 'darwin' | 'linux' | 'freebsd' */
  platform: string
  /** finer-grained id where knowable: 'freebsd'|'openbsd'|'netbsd'|'dragonfly'|'android'|'wsl' | null */
  variant: string | null
  /** process.arch, e.g. 'x64' | 'arm64' | 'ia32' */
  arch: string
  /** os.release() raw string */
  release: string
  /** os.type() raw string, e.g. 'Windows_NT' | 'Darwin' | 'Linux' | 'FreeBSD' */
  kernel: string
  isWindows: boolean
  isMac: boolean
  isLinux: boolean
  isBSD: boolean
}

/** Injectable inputs so detection can be unit-tested for any host. */
export interface OSEnv {
  platform: string
  arch: string
  release: string
  type: string
}
