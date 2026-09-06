// @generated TypeScript projection by scripts/release_contract.py; do not edit.
export const JS_HOST_EXECUTABLE_NAME = "rottweiler-js-host" as const
export const OPENTUI_LICENSES_NAME = "opentui-licenses.txt" as const
export const JS_HOST_ROLES = {"source_plugin": "source-plugin", "tui": "tui"} as const

export interface ReleaseProductBudgets {
  readonly engineLessThanBytes: number
  readonly wasmHostLessThanBytes: number
  readonly jsBundleLessThanBytes: number
}

export interface ReleasePlatform {
  readonly id: string
  readonly nodePlatform: string
  readonly nodeArch: string
  readonly nativeLibrary: string
  readonly productBudgets: ReleaseProductBudgets
}

export const RELEASE_PLATFORMS = [
  {
    id: "darwin-arm64",
    nodePlatform: "darwin",
    nodeArch: "arm64",
    nativeLibrary: "libopentui.dylib",
    productBudgets: {
      engineLessThanBytes: 40000000,
      wasmHostLessThanBytes: 30000000,
      jsBundleLessThanBytes: 100000000,
    },
  },
  {
    id: "darwin-x86_64",
    nodePlatform: "darwin",
    nodeArch: "x64",
    nativeLibrary: "libopentui.dylib",
    productBudgets: {
      engineLessThanBytes: 40000000,
      wasmHostLessThanBytes: 30000000,
      jsBundleLessThanBytes: 100000000,
    },
  },
  {
    id: "linux-aarch64",
    nodePlatform: "linux",
    nodeArch: "arm64",
    nativeLibrary: "libopentui.so",
    productBudgets: {
      engineLessThanBytes: 28000000,
      wasmHostLessThanBytes: 30000000,
      jsBundleLessThanBytes: 150000000,
    },
  },
  {
    id: "linux-x86_64",
    nodePlatform: "linux",
    nodeArch: "x64",
    nativeLibrary: "libopentui.so",
    productBudgets: {
      engineLessThanBytes: 28000000,
      wasmHostLessThanBytes: 30000000,
      jsBundleLessThanBytes: 150000000,
    },
  },
] as const satisfies readonly ReleasePlatform[]

export function releasePlatformForNodeTarget(
  nodePlatform: string,
  nodeArch: string,
): ReleasePlatform | undefined {
  return RELEASE_PLATFORMS.find(
    (platform) =>
      platform.nodePlatform === nodePlatform && platform.nodeArch === nodeArch,
  )
}
