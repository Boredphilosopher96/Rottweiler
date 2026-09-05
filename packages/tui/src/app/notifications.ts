import type { NotificationAdapter } from "../platform"
import type { RottweilerState } from "../state"

export function notifyTransition(notifications: NotificationAdapter, previous: RottweilerState, next: RottweilerState): void {
    const finished = Object.values(next.turns).find(
      (turn) => turn.status !== "running" && previous.turns[turn.turnId]?.status === "running",
    )
    const approval = Object.values(next.tools).find(
      (tool) =>
        tool.status === "awaiting_approval" &&
        previous.tools[tool.invocationId]?.status !== "awaiting_approval",
    )
    const question = Object.values(next.questions).find(
      (candidate) => previous.questions[candidate.questionId] === undefined,
    )
    const pluginNotification =
      next.pluginNotifications.at(-1) !== previous.pluginNotifications.at(-1)
        ? next.pluginNotifications.at(-1)
        : undefined
    if (pluginNotification !== undefined) {
      void notifications.notify({
        kind: "plugin",
        title: pluginNotification.title,
        body: pluginNotification.message,
      })
    } else if (approval !== undefined) {
      void notifications.notify({
        kind: "approval_needed",
        title: "Rottweiler needs approval",
        body: approval.name,
      })
    } else if (question !== undefined) {
      void notifications.notify({
        kind: "question_asked",
        title: "Rottweiler has a question",
        body: question.questions[0]?.prompt ?? "Input required",
      })
    } else if (finished !== undefined) {
      void notifications.notify({
        kind: "turn_finished",
        title: "Rottweiler finished",
        body: `Turn ${finished.turnId} · ${finished.status}`,
      })
    }
}
