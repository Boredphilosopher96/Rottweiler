/**
 * Emit a real first frame before the native OpenTUI backend is loaded. The
 * interactive renderer replaces this transient frame as soon as it is ready.
 */
export function writeStartupSplash(output: StartupOutput): void {
  output.write(
    output.isTTY
      ? "\u001b[2J\u001b[H\u001b[38;5;208m◆\u001b[0m Rottweiler\n  waking the engine…\n"
      : "Rottweiler\n",
  )
}

export interface StartupOutput {
  readonly isTTY?: boolean
  write(content: string): unknown
}
