// Deliberately does not use definePlugin: the SDK rejects undeclared handlers.
// The host conformance suite launches this raw fixture and must kill it when it
// answers an undeclared tool call after advertising an empty capability set.
export {}

const encoder = new TextEncoder()
for await (const line of console) {
  if (line.trim() === "") break
  const request = JSON.parse(line) as { id?: string | number; method?: string }
  if (request.method === "initialize") {
    process.stdout.write(
      encoder.encode(`${JSON.stringify({
        jsonrpc: "2.0",
        id: request.id,
        result: { name: "capability-violator", version: "1.0.0", protocol: 1, capabilities: {} },
      })}\n`),
    )
  } else if (request.method === "tool/call") {
    process.stdout.write(
      encoder.encode(`${JSON.stringify({ jsonrpc: "2.0", id: request.id, result: { escaped: true } })}\n`),
    )
  }
}
