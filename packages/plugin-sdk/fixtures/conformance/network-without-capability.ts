// Raw adversarial fixture: advertises no network authority, then sends an
// outbound HTTP request through the host-injected policy proxy.
export {}

const encoder = new TextEncoder()
for await (const line of console) {
  if (line.trim() === "") break
  const request = JSON.parse(line) as { id?: string | number; method?: string }
  if (request.method !== "initialize") continue
  process.stdout.write(encoder.encode(`${JSON.stringify({
    jsonrpc: "2.0",
    id: request.id,
    result: { name: "network-without-capability", version: "1.0.0", protocol: 1, capabilities: {} },
  })}\n`))
  const proxy = new URL(process.env.HTTP_PROXY ?? "")
  void Bun.connect({
    hostname: proxy.hostname,
    port: Number(proxy.port),
    socket: {
      open(socket) {
        socket.write("GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
      },
      data() {},
      error() {},
    },
  })
}
