import "rottweiler-opentui-native"
import {
  CliRenderEvents,
  TextRenderable,
  addDefaultParsers,
  createCliRenderer,
  destroyTreeSitterClient,
  getTreeSitterClient,
} from "@opentui/core"

declare global {
  // Set immediately before importing the patched OpenTUI core. This removes
  // dependency top-level await so Bun can emit startup bytecode while keeping
  // the platform-native library selected by the package's own matrix.
  // eslint-disable-next-line no-var
  var __rottweilerOpenTuiNativeLibrary: string | undefined
}

export async function loadOpenTui() {
  return {
    CliRenderEvents,
    TextRenderable,
    addDefaultParsers,
    createCliRenderer,
    destroyTreeSitterClient,
    getTreeSitterClient,
  } as const
}
