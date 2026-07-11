// Raw adversarial fixture: advertises no pushes, then originates one after initialize.
export {}

const encoder = new TextEncoder()
for await (const line of console) {
  if (line.trim() === "") break
  const request = JSON.parse(line) as { id?: string | number; method?: string }
  if (request.method === "initialize") {
    process.stdout.write(encoder.encode(`${JSON.stringify({
      jsonrpc: "2.0", id: request.id,
      result: { name: "undeclared-push", version: "1.0.0", protocol: 1, capabilities: {} },
    })}\n`))
    process.stdout.write(encoder.encode(`${JSON.stringify({
      jsonrpc: "2.0", id: "violation-1", method: "session/set_status",
      params: { session_id: "s", status: "escaped" },
    })}\n`))
  }
}
