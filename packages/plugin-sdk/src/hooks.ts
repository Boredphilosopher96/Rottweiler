import type { HandlerContext } from "./server"
import type { HookDirective, HookEvent, HookInput, HookTransform } from "./generated/hook-contract"

export type HookResult<Event extends HookEvent> =
  | Extract<HookDirective, { decision: "continue" | "block" }>
  | (Event extends "permission_check" ? Extract<HookDirective, { decision: "permission" }> : never)
  | { decision: "transform"; change: Extract<HookTransform, { hook: Event }> }

export type HookHandler<Event extends HookEvent = HookEvent> = (
  params: Extract<HookInput, { hook: Event }>, context: HandlerContext,
) => HookResult<Event> | Promise<HookResult<Event>>

export type HookHandlers = { readonly [Event in HookEvent]?: HookHandler<Event> }

export function invokeHook(handlers: HookHandlers, input: HookInput, context: HandlerContext): HookDirective | Promise<HookDirective> {
  switch (input.hook) {
    case "session_start": {
      const handler = handlers.session_start
      if (handler === undefined) throw new Error("session_start hook handler is missing")
      return handler(input, context)
    }
    case "session_end": {
      const handler = handlers.session_end
      if (handler === undefined) throw new Error("session_end hook handler is missing")
      return handler(input, context)
    }
    case "user_prompt_submit": {
      const handler = handlers.user_prompt_submit
      if (handler === undefined) throw new Error("user_prompt_submit hook handler is missing")
      return handler(input, context)
    }
    case "pre_tool": {
      const handler = handlers.pre_tool
      if (handler === undefined) throw new Error("pre_tool hook handler is missing")
      return handler(input, context)
    }
    case "post_tool": {
      const handler = handlers.post_tool
      if (handler === undefined) throw new Error("post_tool hook handler is missing")
      return handler(input, context)
    }
    case "pre_compact": {
      const handler = handlers.pre_compact
      if (handler === undefined) throw new Error("pre_compact hook handler is missing")
      return handler(input, context)
    }
    case "turn_end": {
      const handler = handlers.turn_end
      if (handler === undefined) throw new Error("turn_end hook handler is missing")
      return handler(input, context)
    }
    case "permission_check": {
      const handler = handlers.permission_check
      if (handler === undefined) throw new Error("permission_check hook handler is missing")
      return handler(input, context)
    }
  }
  const exhaustive: never = input
  throw new Error(`invalid hook input: ${exhaustive}`)
}
