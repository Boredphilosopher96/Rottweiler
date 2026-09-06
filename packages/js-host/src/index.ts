import { JS_HOST_ROLES } from "../generated/release-contract"

/** Each process selects one role before importing its application graph. */
export async function runJavaScriptHost(argv: readonly string[]): Promise<void> {
  const [role, ...arguments_] = argv
  switch (role) {
    case JS_HOST_ROLES.tui: {
      if (arguments_.length !== 0) throw new Error("the tui role takes no arguments")
      const { runTui } = await import("../../tui/src/index")
      await runTui()
      return
    }
    case JS_HOST_ROLES.source_plugin: {
      const { main } = await import("../../plugin-host/src/index")
      await main(arguments_)
      return
    }
    default: throw new Error("usage: rottweiler-js-host tui|source-plugin COMMAND")
  }
}

if (import.meta.main) {
  void runJavaScriptHost(process.argv.slice(2)).catch((error: unknown) => {
    const causes: readonly unknown[] = error instanceof AggregateError ? error.errors : []
    const message = [error, ...causes.slice(0, 8)]
      .map(cause => typeof cause === "object" && cause !== null && "message" in cause && typeof cause.message === "string"
        ? cause.message : "JavaScript host failed")
      .join("\n")
    process.stderr.write(`${message.slice(0, 4096)}\n`)
    process.exitCode = 1
  })
}
