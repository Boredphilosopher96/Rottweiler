/** Own a test child and its bounded pipes until it has exited, including failures. */
export async function runOwnedProcess(
  command: string[],
  options: { cwd?: string; timeoutMs: number; maxOutputBytes?: number },
): Promise<{ code: number; stdout: string; stderr: string }> {
  const child = Bun.spawn(command, {
    ...(options.cwd === undefined ? {} : { cwd: options.cwd }),
    stdin: "ignore", stdout: "pipe", stderr: "pipe",
  })
  let failure: Error | undefined
  let outputBytes = 0
  const fail = (error: Error) => {
    failure ??= error
    // This test executable owns only worker threads, not descendant processes.
    // A forced stop cannot leave renderer signal handlers running after disposal.
    child.kill("SIGKILL")
  }
  const read = async (stream: ReadableStream<Uint8Array>): Promise<string> => {
    const chunks: Uint8Array[] = []
    try {
      for await (const chunk of stream) {
        outputBytes += chunk.byteLength
        if (outputBytes > (options.maxOutputBytes ?? 1024 * 1024)) {
          fail(new Error(`Test child output exceeded its byte limit: ${command.join(" ")}`))
        }
        if (failure === undefined) chunks.push(chunk)
      }
      return Buffer.concat(chunks).toString("utf8")
    } catch (error) {
      fail(error instanceof Error ? error : new Error(String(error)))
      return ""
    }
  }
  const timer = setTimeout(() => fail(new Error(`Test child exceeded ${options.timeoutMs}ms: ${command.join(" ")}`)), options.timeoutMs)
  try {
    const [code, stdout, stderr] = await Promise.all([child.exited, read(child.stdout), read(child.stderr)])
    if (failure !== undefined) throw failure
    return { code, stdout, stderr }
  } finally {
    clearTimeout(timer)
    if (child.exitCode === null) child.kill("SIGKILL")
    await child.exited
  }
}
