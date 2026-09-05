import type { EngineEvent } from "../protocol"
import {
  FuzzyPickerRenderable,
  type PickerItem
} from "../components"
import { PickerController } from "../picker-controller"
import {
  type ExternalUrlAdapter,
  type TextClipboardAdapter
} from "../platform"
import {
  ProjectionRequestBroker,
  type ProjectionKind,
} from "../projection-requests"
import {
  type RottweilerState
} from "../state"

import {
  modelAliasDescription,
  modelAvailabilityLabel,
  providerConnectionStatus,
  providerDisplayName,
  providerName,
  providerStatusDetail
} from "../ui-presentation"
import type { RottweilerAppOptions } from "./options"
type ModelPickerChoice =
  | { readonly kind: "alias"; readonly alias: RottweilerState["modelAliases"][number] }
  | { readonly kind: "model"; readonly model: RottweilerState["models"][number] }

type ProviderAuthPickerAction =
  | { readonly kind: "open_url"; readonly value: string }
  | { readonly kind: "copy_url"; readonly value: string }
  | { readonly kind: "copy_code"; readonly value: string }
  | { readonly kind: "cancel" }

interface ProviderUiHost {
  readonly state: RottweilerState
  readonly activeSubagentId: string | null
  readonly draft: string
  readonly picker: FuzzyPickerRenderable<unknown>
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly projectionErrors: Readonly<Partial<Record<ProjectionKind, string>>>
  readonly options: Pick<RottweilerAppOptions, "onProviderApiKey" | "onProviderActivate"> & { readonly externalUrl: ExternalUrlAdapter; readonly textClipboard: TextClipboardAdapter }
  closePicker(): void
  clearProjectionError(kind: ProjectionKind): void
  projectError(code: string, message: string, retryable?: boolean): void
}

export class ProviderUiController {
  readonly #host: ProviderUiHost
  #disposed = false
  #modelsRequested = false
  #providerOnboardingOffered = false
  #providerOnboardingModelsResponseReceived = false
  #providerOnboardingSessionsResponseReceived = false
  #providerPickerOnboarding = false
  #modelProviderFilter: string | null = null
  #providerApiKeyProvider: string | null = null
  #providerRecoveryProvider: RottweilerState["providers"][number] | null = null
  #providerAuthAction: { readonly provider: string; readonly attemptId: string } | null = null
  #providerAuthActionNotice: string | null = null
  #providerAuthCompletionAttempts = new Set<string>()
  #storedProviderKeys = new Set<string>()
  #credentialAction: { readonly provider: string } | null = null

  constructor(host: ProviderUiHost) {
    this.#host = host
  }
  catalogSettled(): void { this.#modelsRequested = false }
  suppressOnboarding(): void { this.#providerOnboardingOffered = true }
  get hasPendingAction(): boolean { return this.#providerAuthAction !== null || this.#credentialAction !== null }
  get modelProviderFilter(): string | null { return this.#modelProviderFilter }
  get onboarding(): boolean { return this.#providerPickerOnboarding }
  pickerClosed(): void {
    this.#providerApiKeyProvider = null
    this.#providerRecoveryProvider = null
  }
  resetAuthentication(): void {
    this.#providerAuthAction = null
    this.#providerAuthActionNotice = null
  }
  resetSession(): void {
    this.#credentialAction = null
    this.catalogSettled()
    this.pickerClosed()
    this.resetAuthentication()
    this.#storedProviderKeys.clear()
    this.#providerAuthCompletionAttempts.clear()
  }
  dispose(): void {
    this.#disposed = true
    this.resetSession()
  }


  #maybeOfferProviderOnboarding(state: RottweilerState): void {
    if (
      !this.#providerOnboardingModelsResponseReceived ||
      !this.#providerOnboardingSessionsResponseReceived
    ) return
    // A cold catalog has not checked credentials or reachability. Only offer
    // setup automatically when no provider is configured; explicit discovery
    // and inference still report missing credentials and unavailable models.
    const configured = state.providers.some((provider) => provider.configured)
    if (
      !configured &&
      state.model === null &&
      !this.#providerOnboardingOffered &&
      !state.replay.active &&
      this.#host.activeSubagentId === null &&
      this.#host.draft.length === 0 &&
      this.#host.pickerController.kind === null
    ) {
      this.#providerOnboardingOffered = true
      this.openProviderPicker(true)
    }
  }

  openModelPicker(provider: string | null = null): void {
    this.#modelProviderFilter = provider
    this.#host.pickerController.begin("models")
    if (!this.#modelsRequested) {
      this.requestModels(true)
    }
    this.#host.requests.command({ type: "list_settings" })
    this.#host.pickerController.refresh()
  }

  openProviderPicker(onboarding = false): void {
    this.#modelProviderFilter = null
    this.#providerPickerOnboarding = onboarding
    this.#host.pickerController.begin("providers")
    if (!this.#modelsRequested) {
      this.requestModels(true)
    }
    this.#host.pickerController.refresh()
  }

  openProviderAuthPicker(): void {
    this.#host.pickerController.begin("providerAuth")
    this.#host.pickerController.refresh()
  }

  openProviderRecoveryPicker(provider: RottweilerState["providers"][number]): void {
    this.#providerRecoveryProvider = provider
    this.#host.pickerController.begin("providerRecovery")
    this.#host.pickerController.refresh()
  }

  openProviderApiKeyPrompt(provider: string): void {
    if (this.#host.state.replay.active || provider.length === 0) return
    this.#host.pickerController.begin("providerApiKey")
    this.#providerApiKeyProvider = provider
    this.#host.picker.openSecret(`Enter ${providerName(provider)} API key`, (apiKey) => {
      const selectedProvider = this.#providerApiKeyProvider
      this.#host.closePicker()
      if (selectedProvider !== null)
        void this.#submitProviderApiKey(selectedProvider, apiKey)
    })
  }

  requestModels(refresh = false): void {
    this.#modelsRequested = true
    this.#host.clearProjectionError("models")
    this.#host.requests.command({ type: "list_models", refresh })
  }

  #currentCredential(operation: { readonly provider: string }): boolean {
    return !this.#disposed && this.#credentialAction === operation
  }

  #currentAuthentication(operation: { readonly provider: string; readonly attemptId: string }): boolean {
    const current = this.#host.state.providerAuth.pending
    return !this.#disposed && this.#providerAuthAction === operation &&
      current?.provider === operation.provider && current.attemptId === operation.attemptId
  }

  async #submitProviderApiKey(provider: string, apiKey: string): Promise<void> {
    if (this.#disposed || this.#credentialAction !== null) return
    const operation = { provider }
    this.#credentialAction = operation
    this.#host.pickerController.kind = "providerApiKey"
    this.#host.pickerController.refresh()
    try {
      const result = await this.#host.options.onProviderApiKey?.(provider, apiKey)
      if (!this.#currentCredential(operation)) return
      if (result === undefined)
        throw new Error("credential transport unavailable")
      this.requestModels(true)
      if (result.activated) {
        this.#storedProviderKeys.delete(provider)
      } else {
        if (this.#storedProviderKeys.size >= 32) {
          const oldest = this.#storedProviderKeys.values().next().value
          if (oldest !== undefined) this.#storedProviderKeys.delete(oldest)
        }
        this.#storedProviderKeys.add(provider)
        this.#host.projectError(
          "provider_activation_pending",
          "credential stored securely, but activation is pending; select the provider again to refresh without re-entering the key",
          true
        )
      }
      this.openProviderPicker()
      for (const warning of result.warnings.slice(0, 16)) {
        this.#host.projectError("provider_credential_warning", warning)
      }
    } catch {
      if (!this.#currentCredential(operation)) return
      this.#host.projectError(
        "provider_credential_failed",
        "provider credential submission failed; verify the key and try again",
        true
      )
      this.openProviderPicker()
    } finally {
      if (this.#currentCredential(operation)) this.#credentialAction = null
    }
  }

  async #retryProviderActivation(provider: string): Promise<void> {
    if (this.#disposed || this.#credentialAction !== null) return
    const operation = { provider }
    this.#credentialAction = operation
    this.#host.pickerController.kind = "providerApiKey"
    this.#host.pickerController.refresh()
    try {
      if (this.#host.options.onProviderActivate === undefined) throw new Error("activation unavailable")
      await this.#host.options.onProviderActivate(provider)
      if (!this.#currentCredential(operation)) return
      this.#storedProviderKeys.delete(provider)
      this.requestModels(true)
      this.openProviderPicker()
    } catch {
      if (!this.#currentCredential(operation)) return
      this.#host.projectError(
        "provider_activation_failed",
        "credential remains stored securely, but activation failed; retry from /providers",
        true,
      )
      this.openProviderPicker()
    } finally {
      if (this.#currentCredential(operation)) this.#credentialAction = null
    }
  }

  async #runProviderAuthAction(
    provider: string,
    attemptId: string,
    action: ProviderAuthPickerAction,
  ): Promise<void> {
    if (this.#disposed || this.#providerAuthAction !== null) return
    const pending = this.#host.state.providerAuth.pending
    if (
      pending === null ||
      pending.provider !== provider ||
      pending.attemptId !== attemptId
    )
      return
    const operation = { provider, attemptId }
    this.#providerAuthAction = operation
    let failureCode = "provider_auth_action_failed"
    let failureMessage =
      "provider authentication action failed; copy the URL manually"
    try {
      switch (action.kind) {
        case "open_url":
          failureCode = "provider_auth_browser_failed"
          failureMessage =
            "couldn't open a browser; use Copy URL and open it manually"
          await this.#host.options.externalUrl.open(action.value)
          if (!this.#currentAuthentication(operation)) return
          this.#providerAuthActionNotice =
            "Browser opened · waiting for authentication"
          break
        case "copy_code":
          failureCode = "provider_auth_copy_failed"
          failureMessage =
            "couldn't copy the device code; enter the displayed code manually"
          await this.#host.options.textClipboard.writeText(action.value)
          if (!this.#currentAuthentication(operation)) return
          this.#providerAuthActionNotice =
            "Code copied · waiting for authentication"
          break
        case "copy_url":
          failureCode = "provider_auth_copy_failed"
          failureMessage =
            "couldn't copy the URL; open the displayed URL manually"
          await this.#host.options.textClipboard.writeText(action.value)
          if (!this.#currentAuthentication(operation)) return
          this.#providerAuthActionNotice =
            "URL copied · waiting for authentication"
          break
        case "cancel":
          return
      }
    } catch {
      if (!this.#currentAuthentication(operation)) return
      this.#providerAuthActionNotice = null
      this.#host.projectError(failureCode, failureMessage, true)
    } finally {
      if (this.#currentAuthentication(operation)) {
        this.#providerAuthAction = null
        if (this.#host.pickerController.kind === "providerAuth") this.#host.pickerController.refresh()
      }
    }
  }
  afterEvent(event: EngineEvent, eventRecord: Readonly<Record<string, unknown>>, commandRequestId: string | null, next: RottweilerState): void {
    if (event.type === "models_listed") {
      const activationCatalog = this.#host.requests.consumeProviderActivationModels(
        commandRequestId,
      )
      if (
        activationCatalog &&
        !next.replay.active &&
        this.#host.activeSubagentId === null
      ) {
        const availableModels = next.models.filter((model) => model.available !== false)
        if (availableModels.length === 1) {
          const model = availableModels[0]!
          this.#host.requests.command({
            type: "switch_model",
            model: model.id,
            provider: model.provider,
          })
          this.#host.closePicker()
        }
      }
      if (
        !this.#providerOnboardingModelsResponseReceived &&
        next.connection.phase === "connected"
      ) {
        this.#providerOnboardingModelsResponseReceived = true
        this.#maybeOfferProviderOnboarding(next)
      }
    }
    if (
      event.type === "sessions_listed" &&
      !this.#providerOnboardingSessionsResponseReceived &&
      next.connection.phase === "connected"
    ) {
      this.#providerOnboardingSessionsResponseReceived = true
      this.#maybeOfferProviderOnboarding(next)
    }
    if (event.type === "provider_auth_started") {
      const provider = typeof eventRecord.provider === "string" ? eventRecord.provider : null
      const attemptId = typeof eventRecord.attempt_id === "string" ? eventRecord.attempt_id : null
      if (provider === null || attemptId === null) return
      this.#providerAuthAction = null
      this.#providerAuthActionNotice = null
      const firstDelivery = !this.#providerAuthCompletionAttempts.has(attemptId)
      if (firstDelivery) {
        if (this.#providerAuthCompletionAttempts.size >= 64) {
          const oldest = this.#providerAuthCompletionAttempts.values().next().value
          if (oldest !== undefined) this.#providerAuthCompletionAttempts.delete(oldest)
        }
        this.#providerAuthCompletionAttempts.add(attemptId)
        this.#host.requests.command({
          type: "complete_provider_auth",
          provider,
          attemptId,
        })
      }
      this.openProviderAuthPicker()
      if (firstDelivery) {
        const challenge = next.providerAuth.pending?.challenge
        const url = challenge?.kind === "oauth"
          ? challenge.authorization_url
          : challenge?.verification_uri
        if (url !== undefined) {
          void this.#runProviderAuthAction(provider, attemptId, { kind: "open_url", value: url })
        }
      }
    }
    if (event.type === "provider_configured") {
      const provider = typeof eventRecord.provider === "string" ? eventRecord.provider : null
      if (provider === null) return
      if (eventRecord.auth_kind === "oauth" || eventRecord.auth_kind === "device_flow") {
        this.#host.requests.command({ type: "begin_provider_auth", provider })
      } else if (eventRecord.auth_kind === "api_key") {
        this.openProviderApiKeyPrompt(provider)
      }
    }
    if (event.type === "provider_auth_finished") {
      this.#providerAuthAction = null
      if (eventRecord.success === true) {
        this.#providerAuthActionNotice = "Signed in. Connecting provider and loading models…"
      } else {
        this.#providerAuthActionNotice = null
        this.#host.projectError(
          "provider_auth_failed",
          typeof eventRecord.message === "string" ? eventRecord.message : "provider authentication failed",
          true,
        )
      }
    }
    if (event.type === "provider_activation_finished") {
      this.#providerAuthActionNotice = null
      const message = typeof eventRecord.message === "string"
        ? eventRecord.message
        : "provider connection did not become ready"
      if (eventRecord.success === true) {
        this.requestModels(true)
        this.#host.requests.markProviderActivationModels()
      } else {
        this.#host.projectError("provider_activation_failed", message, true)
      }
      this.openProviderPicker()
    }
  }
  render(kind: "models" | "providers" | "providerRecovery" | "providerAuth" | "providerApiKey"): void {
    switch (kind) {
      case "models":
        const models = this.#host.state.models.filter(
          (model) =>
            this.#modelProviderFilter === null ||
            model.provider === this.#modelProviderFilter,
        )
        const concreteModelIds = new Set(models.map((model) => model.id))
        const aliases = this.#modelProviderFilter === null
          ? this.#host.state.modelAliases.filter(
            (alias) =>
              alias.candidates.length !== 1 ||
              alias.alias !== alias.candidates[0] ||
              !concreteModelIds.has(alias.candidates[0]!),
          )
          : []
        const modelItems: PickerItem<ModelPickerChoice | null>[] = [
          ...(aliases.length === 0
            ? []
            : [{
              id: "models.section.failover-chains",
              label: "Failover chains",
              description: "",
              value: null,
              selectable: false,
              sectionHeader: true,
            }]),
          ...aliases.map((alias) => ({
            id: `model-alias:${alias.alias}`,
            label: `${alias.current ? "● " : ""}${alias.alias}`,
            description: modelAliasDescription(alias, models),
            value: { kind: "alias" as const, alias },
          })),
          ...(models.length === 0
            ? []
            : [{
              id: "models.section.models",
              label: "Models",
              description: "",
              value: null,
              selectable: false,
              sectionHeader: true,
            }]),
          ...models.map((model) => ({
            id: model.id,
            label: `${model.current ? "● " : ""}${model.displayName}`,
            description: [
              model.provider,
              modelAvailabilityLabel(model),
              model.toolCalling ? "tools" : "",
              model.vision ? "vision" : "",
              model.thinking ? "thinking" : "",
              "pinned route",
            ]
              .filter(Boolean)
              .join(" · "),
            value: { kind: "model" as const, model },
          })),
        ]
        const modelError = this.#host.projectionErrors.models
        if (modelError === undefined && this.#modelsRequested && modelItems.length === 0) {
          this.#host.pickerController.showLoading("Models", "Loading available models")
          break
        }
        if (modelError !== undefined) {
          modelItems.unshift({
            id: "models.error",
            label: "Couldn't load models",
            description: `${modelError} · select to retry`,
            value: null,
          })
        }
        if (modelItems.length === 0) {
          this.#host.pickerController.showStatus(
            "Models",
            "No models are available",
            "Connect a provider, then reopen this panel.",
          )
          break
        }
        this.#host.pickerController.show(
          this.#modelProviderFilter === null
            ? "Models"
            : `Models · ${this.#modelProviderFilter}`,
          modelItems,
          (item) => {
            const selection = item.value as ModelPickerChoice | null
            if (selection === null) {
              if (item.id === "models.error") {
                this.requestModels()
                return
              }
              this.#host.projectError(
                "models_unavailable",
                "no configured model routes are available; configure a provider and model alias",
              )
              this.#host.closePicker()
              return
            }
            if (selection.kind === "alias") {
              this.#host.requests.command({
                type: "switch_model",
                model: selection.alias.alias,
              })
            } else {
              const model = selection.model
              if (model.available === false) {
                this.#host.projectError(
                  "model_unavailable",
                  model.status ?? `${model.displayName} is unavailable`,
                  true,
                )
                return
              }
              this.#host.requests.command({
                type: "switch_model",
                model: model.id,
                provider: model.provider,
              })
            }
            this.#host.closePicker()
          },
        )
        break
      case "providers": {
        const providerChoices = this.#host.state.providers
        const providerItems: PickerItem<RottweilerState["providers"][number] | null>[] =
          providerChoices
            .slice()
            .sort((left, right) => left.name.localeCompare(right.name))
            .map((provider) => ({
              id: provider.name,
              label: providerDisplayName(provider),
              description: [
                providerConnectionStatus(provider),
                `${provider.modelCount} model${provider.modelCount === 1 ? "" : "s"}`,
                this.#storedProviderKeys.has(
                  provider.name)
                  ? "credential stored"
                  : "",
                providerStatusDetail(provider),
              ].filter(Boolean).join(" · "),
              value: provider,
            }))
        const providerError = this.#host.projectionErrors.models
        if (providerError === undefined && this.#modelsRequested && providerItems.length === 0) {
          this.#host.pickerController.showLoading("Providers", "Loading provider connections")
          break
        }
        if (providerError !== undefined) {
          providerItems.unshift({
            id: "providers.error",
            label: "Couldn't load providers",
            description: `${providerError} · select to retry`,
            value: null,
          })
        }
        if (providerItems.length === 0) {
          this.#host.pickerController.showStatus(
            this.#providerPickerOnboarding
              ? "Welcome to Rottweiler · connect a provider to start"
              : "Providers",
            "No providers are connected",
            "Connect a provider, then reopen this panel.",
          )
          break
        }
        this.#host.pickerController.show(
          this.#providerPickerOnboarding
            ? "Welcome to Rottweiler · connect a provider to start"
            : "Providers",
          providerItems,
          (item) => {
            const provider = item.value as RottweilerState["providers"][number] | null
            if (provider === null) {
              if (item.id === "providers.error") {
                this.requestModels()
                return
              }
              this.#host.projectError(
                "providers_unavailable",
                "no configured provider routes are available; authenticate and configure a provider",
              )
              this.#host.closePicker()
              return
            }
            if (provider.authenticated && !provider.reachable) {
              this.openProviderRecoveryPicker(provider)
              return
            }
            switch (provider.nextAction) {
              case "select_models":
                this.openModelPicker(provider.name)
                break
              case "authenticate":
                this.#host.requests.command({ type: "begin_provider_auth", provider: provider.name })
                break
              case "api_key_cli":
                if (this.#storedProviderKeys.has(provider.name)) {
                  void this.#retryProviderActivation(provider.name)
                } else {
                  this.openProviderApiKeyPrompt(provider.name)
                }
                break
              case "configure":
                this.#host.requests.command({ type: "configure_builtin_provider", provider: provider.name })
                break
              case "none":
                this.#host.projectError(
                  "provider_auth_unavailable",
                  provider.status ?? `${provider.name} has no safe authentication action`,
                  true,
                )
                break
            }
          }
        )
        break
      }
      case "providerRecovery": {
        const provider = this.#providerRecoveryProvider
        if (provider === null) {
          this.openProviderPicker()
          break
        }
        const items: PickerItem<"activate" | "reauthenticate">[] = [
          {
            id: "provider-recovery.activate",
            label: "Refresh models",
            description: "Retry this provider's live model catalog with the saved sign-in",
            value: "activate",
          },
        ]
        if (provider.authKind !== "none") {
          items.push({
            id: "provider-recovery.reauthenticate",
            label: provider.authKind === "api_key" ? "Replace API key" : "Re-authenticate",
            description: "Replace the stored credential for this provider",
            value: "reauthenticate",
          })
        }
        this.#host.pickerController.show(`Reconnect ${providerName(provider.name)}`, items, (item) => {
          if (item.value === "activate") {
            void this.#retryProviderActivation(provider.name)
          } else if (provider.authKind === "api_key") {
            this.openProviderApiKeyPrompt(provider.name)
          } else {
            this.#host.closePicker()
            this.#host.requests.command({ type: "begin_provider_auth", provider: provider.name })
          }
        })
        break
      }
      case "providerAuth": {
        const pending = this.#host.state.providerAuth.pending
        if (pending === null) {
          this.openProviderPicker()
          break
        }
        const authUrl =
          pending.challenge.kind === "oauth"
            ? pending.challenge.authorization_url
            : pending.challenge.verification_uri
        const prompt =
          pending.challenge.kind === "oauth"
            ? "Finish signing in in your browser; Rottweiler will continue automatically"
            : `Enter code ${pending.challenge.user_code} on GitHub; Rottweiler will continue automatically`
        const items: PickerItem<ProviderAuthPickerAction>[] = [
          {
            id: "provider-auth.open",
            label: pending.challenge.kind === "oauth" ? "Continue in browser" : "Open GitHub",
            description: this.#providerAuthActionNotice ?? prompt,
            searchText: `open browser ${prompt}`,
            value: { kind: "open_url", value: authUrl },
          },
        ]
        if (pending.challenge.kind === "device_flow") {
          items.push({
            id: "provider-auth.copy-code",
            label: `Copy code ${pending.challenge.user_code}`,
            description: "Copy the one-time GitHub device code",
            searchText: `copy code ${pending.challenge.user_code}`,
            value: { kind: "copy_code", value: pending.challenge.user_code },
          })
        }
        items.push(
          {
            id: "provider-auth.copy-url",
            label: "Copy sign-in link",
            description: "Copy the browser link to the clipboard",
            searchText: `copy url ${authUrl}`,
            value: { kind: "copy_url", value: authUrl },
          },
          {
            id: "provider-auth.cancel",
            label: "Cancel sign-in",
            description: pending.warnings.join(" · ") || "Stop this sign-in attempt",
            value: { kind: "cancel" },
          },
        )
        this.#host.pickerController.show(
          `Sign in · ${providerDisplayName({
            name: pending.provider,
            authKind: pending.challenge.kind === "oauth" ? "oauth" : "device_flow",
          })}`,
          items,
          (item) => {
            if (item.value.kind === "cancel") {
              this.#providerAuthActionNotice = null
              this.#host.requests.command({
                type: "cancel_provider_auth",
                provider: pending.provider,
                attemptId: pending.attemptId,
              })
            } else {
              void this.#runProviderAuthAction(
                pending.provider,
                pending.attemptId,
                item.value,
              )
            }
          },
        )
        break
      }
      case "providerApiKey":
        if (this.#credentialAction !== null) {
          this.#host.pickerController.showLoading(
            `Provider credential · ${providerName(this.#credentialAction.provider)}`,
            "Storing and activating credential",
          )
        }
        break
    }
  }
}
