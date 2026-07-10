export interface DesktopNotification {
  readonly title: string
  readonly body: string
  readonly kind: "turn_finished" | "approval_needed" | "question_asked"
}

export interface NotificationAdapter {
  notify(notification: DesktopNotification): void | Promise<void>
}

export interface EditorAdapter {
  compose(initialValue: string): Promise<string | null>
}

export interface ClipboardImage {
  readonly name: string
  readonly mediaType: string
  readonly base64: string
}

export interface ImagePasteAdapter {
  readImage(): Promise<ClipboardImage | null>
}

export const noNotifications: NotificationAdapter = {
  notify() {},
}

export const noExternalEditor: EditorAdapter = {
  async compose() {
    return null
  },
}

export const noImagePaste: ImagePasteAdapter = {
  async readImage() {
    return null
  },
}
