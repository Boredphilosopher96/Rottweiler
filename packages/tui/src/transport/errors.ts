export class EngineTransportError extends Error {
  readonly status: number | undefined

  constructor(message: string, status?: number) {
    super(status === undefined ? message : `${message} (HTTP ${status})`)
    this.name = "EngineTransportError"
    this.status = status
  }
}

export class EngineProtocolError extends EngineTransportError {
  constructor(message: string) {
    super(message)
    this.name = "EngineProtocolError"
  }
}

