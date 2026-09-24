import type { ClientAllocationOwner } from "../client-allocation"
import type { EngineEvent, CompatibleProviderSetup } from "../protocol"
import type { PickerItem } from "../components"
import { PickerController } from "../picker-controller"
import { type ExternalUrlAdapter, type TextClipboardAdapter } from "../platform"
import { ProjectionRequestBroker, type ProjectionKind } from "../projection-requests"
import { formatTokenCount } from "../render"
import { type RottweilerState } from "../state"

import {
  modelAliasDescription,
  modelAvailabilityLabel,
  providerConnectionStatus,
  providerDisplayName,
  providerName,
  providerStatusDetail,
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
  readonly allocations: ClientAllocationOwner
  readonly state: RottweilerState
  readonly activeSubagentId: string | null
  readonly draft: string
  readonly submissionPending: boolean
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
  #liveOnboardingCatalogRequested = false
  #automaticOnboarding: "models" | "providers" | null = null
  #providerOnboardingModelsResponseReceived = false
  #providerOnboardingSessionsResponseReceived = false
  #providerPickerOnboarding = false
  #modelProviderFilter: string | null = null
  #providerBack = false
  #activationProvider: string | null = null
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
  catalogSettled(): void {
    this.#modelsRequested = false
    if (!this.#liveOnboardingCatalogRequested || this.#providerOnboardingOffered) return
    // Success and failure both settle the catalog projection; a failure records
    // its error after this call, so decide once the current dispatch finishes.
    queueMicrotask(() => {
      if (this.#disposed || this.#providerOnboardingOffered) return
      if (this.#host.projectionErrors.models !== undefined) this.#offerFromCachedCatalog(this.#host.state)
    })
  }
  suppressOnboarding(): void { this.#providerOnboardingOffered = true }
  get hasPendingAction(): boolean { return this.#providerAuthAction !== null || this.#credentialAction !== null }
  get modelProviderFilter(): string | null { return this.#modelProviderFilter }
  get onboarding(): boolean { return this.#providerPickerOnboarding }
  pickerClosed(): void {
    this.#automaticOnboarding = null
    this.#providerApiKeyProvider = null
    this.#providerRecoveryProvider = null
  }
  resetAuthentication(): void {
    this.#providerAuthAction = null
    this.#providerAuthActionNotice = null
  }
  resetSession(): void {
    this.#credentialAction = null
    this.#activationProvider = null
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
      !this.#providerOnboardingSessionsResponseReceived ||
      this.#providerOnboardingOffered ||
      selectionResolved(state) ||
      state.replay.active ||
      this.#host.activeSubagentId !== null ||
      this.#host.draft.length > 0 ||
      this.#host.submissionPending ||
      state.hasActivity ||
      this.#host.pickerController.kind !== null
    ) return
    const configured = state.providers.some((provider) => provider.configured)
    // A cached catalog cannot prove current availability. Decide from the live
    // catalog so a connected provider is never mistaken for missing setup.
    if (configured && state.modelCatalogCached) {
      if (!this.#liveOnboardingCatalogRequested) {
        this.#liveOnboardingCatalogRequested = true
        this.requestModels(true)
      }
      return
    }
    this.#providerOnboardingOffered = true
    // A fresh session selects the first available tool-capable model in
    // provider catalog order; the engine persists it as the first default.
    const model = firstUsableModel(state)
    if (model !== undefined) {
      this.#host.requests.command({ type: "switch_model", model: model.id, provider: model.provider })
      return
    }
    if (configured && state.models.some(model => model.available !== false)) {
      this.openModelPicker()
      this.#automaticOnboarding = "models"
    } else {
      this.openProviderPicker(true)
      this.#automaticOnboarding = "providers"
    }
  }


  /** The live refresh failed: use the cached catalog if it still offers a model. */
  #offerFromCachedCatalog(state: RottweilerState): void {
    if (
      selectionResolved(state) ||
      state.replay.active ||
      this.#host.activeSubagentId !== null ||
      this.#host.draft.length > 0 ||
      this.#host.submissionPending ||
      state.hasActivity ||
      this.#host.pickerController.kind !== null
    ) return
    this.#providerOnboardingOffered = true
    const model = firstUsableModel(state)
    if (model !== undefined) {
      this.#host.requests.command({ type: "switch_model", model: model.id, provider: model.provider })
      return
    }
    // Keep the recorded catalog error visible with its retry row instead of
    // silently starting another refresh.
    this.#modelsRequested = true
    this.openModelPicker()
    this.#automaticOnboarding = "models"
  }

  openCompatibleProviderSetup(): void {
    if (this.#host.state.replay.active || this.#host.activeSubagentId !== null) return
    this.#host.pickerController.begin("providerSetup")
    const prompt = (title: string, placeholder: string, maxBytes: number, onSubmit: (value: string) => void, empty: "allow" | "reject" = "reject", detail?: string) => {
      this.#host.pickerController.openTextPrompt({ title, placeholder, maxBytes, onSubmit, empty, ...(detail === undefined ? {} : { detail }) })
    }
    const model = (configuration: Omit<CompatibleProviderSetup, "initial_model">) => {
      prompt("CONNECT › Initial model (optional)", "exact model ID, or leave empty", 256, (value) => {
        const setup: CompatibleProviderSetup = { ...configuration, initial_model: value.trim() || null }
        this.#host.pickerController.show("CONNECT › Review endpoint", [{
          id: "provider-setup.save", label: "Save provider and connect",
          description: `${setup.provider} · ${setup.adapter} · ${setup.endpoint} · ${setup.auth === "api_key" ? "API key next" : "no authentication"}${setup.initial_model === null ? "" : ` · ${setup.initial_model}`}`,
          value: setup,
        }], item => {
          this.#host.closePicker()
          this.#host.requests.command({ type: "configure_compatible_provider", configuration: item.value })
        })
      }, "allow", "A local server without a /models listing needs its exact model ID. Leave empty to discover models from the endpoint.")
    }
    const endpoint = (provider: string, adapter: CompatibleProviderSetup["adapter"]) => {
      prompt("CONNECT › Endpoint URL", adapter === "chat" ? "https://gateway.example/v1/chat/completions" : "https://gateway.example/v1/responses", 2048, value => {
        const endpoint = value.trim()
        this.#host.pickerController.show<CompatibleProviderSetup["auth"]>("CONNECT › Authentication", [
          { id: "provider-setup.api-key", label: "API key", description: "Stored through the secure credential channel", value: "api_key" },
          { id: "provider-setup.no-auth", label: "No authentication", description: "Allowed only for a local loopback endpoint", value: "none" },
        ], item => model({ provider, adapter, endpoint, auth: item.value }))
      })
    }
    prompt("CONNECT › Provider name", "my-gateway", 128, value => {
      const provider = value.trim()
      this.#host.pickerController.show<CompatibleProviderSetup["adapter"]>("CONNECT › API format", [
        { id: "provider-setup.chat", label: "Chat completions", description: "OpenAI-compatible chat API", value: "chat" },
        { id: "provider-setup.responses", label: "Responses", description: "OpenAI-compatible Responses API", value: "responses" },
      ], item => endpoint(provider, item.value))
    })
  }

  openModelPicker(provider: string | null = null): void {
    this.#automaticOnboarding = null
    this.#modelProviderFilter = provider
    this.#host.pickerController.begin("models")
    if (!this.#modelsRequested) {
      this.requestModels(true)
    }
    this.#host.requests.command({ type: "list_settings" })
    this.#host.pickerController.refresh()
  }

  /** `fromModels` makes Esc return to the model list instead of closing. */
  openProviderPicker(onboarding = false, fromModels = false): void {
    this.#automaticOnboarding = null
    this.#modelProviderFilter = null
    this.#providerPickerOnboarding = onboarding
    this.#providerBack = fromModels
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
    this.#host.pickerController.openSecret(`${providerName(provider)} · API key`, (apiKey) => {
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
    const allocation = this.#host.allocations.reserve("decoding", 0)
    try {
      const result = await this.#host.options.onProviderApiKey?.(provider, apiKey, { admit: bytes => allocation.resize(bytes) })
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
      if (result.activated) {
        this.#activationProvider = provider
        this.#host.requests.markProviderActivationModels()
        this.openModelPicker(provider)
      } else this.openProviderPicker()
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
      allocation.release()
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
      this.#activationProvider = provider
      this.#host.requests.markProviderActivationModels()
      this.openModelPicker(provider)
    } catch {
      if (!this.#currentCredential(operation)) return
      this.#host.projectError(
        "provider_activation_failed",
        "credential remains stored securely, but activation failed; retry from /model",
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
      if (next.model !== null && next.models.some(model => model.available !== false && (model.id === next.model || model.aliases.includes(next.model!)))
        && this.#automaticOnboarding !== null && this.#host.pickerController.kind === this.#automaticOnboarding) this.#host.closePicker()
      const activationCatalog = this.#host.requests.consumeProviderActivationModels(
        commandRequestId,
      )
      if (
        activationCatalog &&
        !next.replay.active &&
        this.#host.activeSubagentId === null
      ) {
        const availableModels = next.models.filter((model) =>
          model.available !== false && model.provider === this.#activationProvider)
        this.#activationProvider = null
        // Catalog order belongs to the provider. Prefer a tool-capable model for
        // a fresh coding session; connecting another provider keeps the selection.
        const model = availableModels.find(model => model.toolCalling) ?? availableModels[0]
        if (next.model === null && model !== undefined) {
          this.#host.requests.command({
            type: "switch_model",
            model: model.id,
            provider: model.provider,
          })
          this.#host.closePicker()
        }
      }
      if (next.connection.phase === "connected") {
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
      } else if (eventRecord.auth_kind === "none") {
        void this.#retryProviderActivation(provider)
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
        this.#activationProvider = typeof eventRecord.provider === "string" ? eventRecord.provider : null
        this.requestModels(true)
        this.#host.requests.markProviderActivationModels()
        this.openModelPicker(this.#activationProvider)
      } else {
        this.#host.projectError("provider_activation_failed", message, true)
        this.openProviderPicker()
      }
    }
  }
  /**
   * Models: every model grouped by provider, with capabilities and context
   * window, the current choice marked, and unavailable models explained in
   * place. Connecting another provider is a chord, not a model row.
   */
  #renderModels(): void {
    const state = this.#host.state
    const filter = this.#modelProviderFilter
    const models = state.models.filter(model => filter === null || model.provider === filter)
    const concreteModelIds = new Set(models.map(model => model.id))
    const aliases = filter === null
      ? state.modelAliases.filter(alias => alias.candidates.length !== 1 || alias.alias !== alias.candidates[0]
        || !concreteModelIds.has(alias.candidates[0]!))
      : []
    const modelError = this.#host.projectionErrors.models
    if (modelError === undefined && this.#modelsRequested && models.length === 0 && aliases.length === 0) {
      this.#host.pickerController.showLoading("MODELS   /model", "Loading available models")
      return
    }
    const providers = [...new Set(models.map(model => model.provider))]
    const items: PickerItem<ModelPickerChoice | null>[] = [
      ...(modelError === undefined ? [] : [{
        id: "models.error", label: "Retry loading models", tone: "error" as const, primary: "retry",
        description: modelError, detail: `${modelError}\n\nSelect to ask every provider for its model list again.`, value: null,
      }]),
      ...(aliases.length === 0 ? [] : [{ id: "models.section.failover-chains", label: "Failover chains", description: "",
        sectionHeader: true, value: null }]),
      ...aliases.map(alias => ({
        id: `model-alias:${alias.alias}`,
        label: alias.alias,
        ...(alias.current ? { marker: "●" } : {}),
        hint: `${alias.candidates.length} ${alias.candidates.length === 1 ? "route" : "routes"}`,
        description: modelAliasDescription(alias, models),
        detail: `Tries each route in order until one is available.\n\n${alias.candidates.map((candidate, index) =>
          `${index + 1}. ${models.find(model => model.id === candidate)?.displayName ?? candidate}`).join("\n")}`,
        value: { kind: "alias" as const, alias },
      })),
      ...providers.flatMap(provider => {
        const members = models.filter(model => model.provider === provider)
        // Unavailable models stay visible with their reason and sink within their provider.
        const ordered = [...members.filter(model => model.available !== false), ...members.filter(model => model.available === false)]
        return [
          { id: `models.section.${provider}`, label: providerName(provider), description: "", sectionHeader: true, value: null },
          ...ordered.map(model => this.#modelItem(model)),
        ]
      }),
    ]
    const cached = state.modelCatalogCached
    this.#host.pickerController.show(
      `${filter === null ? "MODELS" : `MODELS › ${providerName(filter)}`}${cached ? "   cached catalog" : ""}   /model`,
      items,
      (item) => {
        const selection = item.value
        if (selection === null) {
          if (item.id === "models.error") this.requestModels()
          return
        }
        if (selection.kind === "alias") {
          this.#host.requests.command({ type: "switch_model", model: selection.alias.alias })
        } else {
          if (selection.model.available === false) return
          this.#host.requests.command({ type: "switch_model", model: selection.model.id, provider: selection.model.provider })
        }
        this.#host.closePicker()
      },
      {
        primary: "use",
        selectedId: models.find(model => model.current)?.id ?? aliases.find(alias => alias.current)?.alias ?? null,
        emptyCopy: filter === null ? "No models yet\nctrl+n connects a provider." : `${providerName(filter)} has no models yet`,
        keys: [{ stroke: "ctrl+n", label: "connect provider", available: () => !state.replay.active,
          run: () => this.openProviderPicker(false, true) }],
        ...(filter === null ? {} : { back: () => this.openModelPicker() }),
      },
    )
  }

  #modelItem(model: RottweilerState["models"][number]): PickerItem<ModelPickerChoice> {
    const capabilities = [model.toolCalling ? "tools" : null, model.vision ? "vision" : null, model.thinking ? "thinking" : null]
      .filter((part): part is string => part !== null)
    const window = model.contextTokens === null ? null : `${formatTokenCount(model.contextTokens)} ctx`
    const unavailable = model.available === false
    return {
      id: model.id,
      label: model.displayName,
      ...(model.current ? { marker: "●" } : {}),
      ...(unavailable ? { tone: "muted" as const, primary: null } : {}),
      hint: unavailable ? modelAvailabilityLabel(model) : [window, ...capabilities].filter(Boolean).join(" · "),
      description: [modelAvailabilityLabel(model), ...capabilities].join(" · "),
      detail: [
        `${providerName(model.provider)}${model.current ? " · current model" : ""}`,
        "",
        `context   ${model.contextTokens === null ? "not published by the provider" : `${formatTokenCount(model.contextTokens)} tokens`}`,
        `tools     ${model.toolCalling ? "yes" : "no"}`,
        `vision    ${model.vision ? "yes" : "no"}`,
        `thinking  ${model.thinking ? "yes" : "no"}`,
        ...(model.aliases.length === 0 ? [] : [`roles     ${model.aliases.join(", ")}`]),
        ...(unavailable ? ["", `Unavailable · ${model.status ?? modelAvailabilityLabel(model)}`] : []),
      ].join("\n"),
      searchText: `${model.displayName} ${providerName(model.provider)} ${model.aliases.join(" ")}`,
      value: { kind: "model", model },
    }
  }

  /** Providers: connection state and the one next step each needs. */
  #renderProviders(): void {
    const providerError = this.#host.projectionErrors.models
    const choices = this.#host.state.providers.slice().sort((left, right) => left.name.localeCompare(right.name))
    const title = this.#providerPickerOnboarding ? "WELCOME   connect a provider to start" : "CONNECT A PROVIDER"
    if (providerError === undefined && this.#modelsRequested && choices.length === 0) {
      this.#host.pickerController.showLoading(title, "Loading provider connections")
      return
    }
    const items: PickerItem<RottweilerState["providers"][number] | null>[] = [
      ...(providerError === undefined ? [] : [{
        id: "providers.error", label: "Retry loading providers", tone: "error" as const, primary: "retry",
        description: providerError, value: null,
      }]),
      ...(choices.length === 0 ? [] : [{ id: "providers.section.builtin", label: "Providers", description: "", sectionHeader: true, value: null }]),
      ...choices.map(provider => {
        const connected = provider.authenticated && provider.reachable
        const stored = this.#storedProviderKeys.has(provider.name)
        return {
          id: provider.name,
          label: providerDisplayName(provider),
          ...(connected ? { marker: "●", tone: "success" as const } : {}),
          hint: connected ? `${provider.modelCount} ${provider.modelCount === 1 ? "model" : "models"}` : providerConnectionStatus(provider),
          description: [providerConnectionStatus(provider), stored ? "credential stored" : "", providerStatusDetail(provider)].filter(Boolean).join(" · "),
          detail: [
            providerConnectionStatus(provider),
            `${provider.modelCount} ${provider.modelCount === 1 ? "model" : "models"}`,
            ...(stored ? ["A credential is stored; select to activate it without re-entering."] : []),
            ...(providerStatusDetail(provider).length === 0 ? [] : ["", providerStatusDetail(provider)]),
          ].join("\n"),
          primary: connected ? "models" : provider.authenticated ? "reconnect" : "connect",
          value: provider,
        }
      }),
      { id: "providers.section.custom", label: "Custom", description: "", sectionHeader: true, value: null },
      { id: "providers.compatible", label: "Compatible endpoint…", description: "An OpenAI-compatible gateway or a local model server",
        detail: "Connect any OpenAI-compatible chat or Responses endpoint, including a local model server.", primary: "set up", value: null },
    ]
    this.#host.pickerController.show(title, items, (item) => {
      const provider = item.value
      if (provider === null) {
        if (item.id === "providers.compatible") this.openCompatibleProviderSetup()
        else if (item.id === "providers.error") this.requestModels()
        return
      }
      this.#connect(provider)
    }, {
      selectedId: choices.find(provider => !provider.authenticated)?.name ?? null,
      ...(this.#providerBack ? { back: () => this.openModelPicker() } : {}),
    })
  }

  #connect(provider: RottweilerState["providers"][number]): void {
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
        if (this.#storedProviderKeys.has(provider.name)) void this.#retryProviderActivation(provider.name)
        else this.openProviderApiKeyPrompt(provider.name)
        break
      case "configure":
        this.#host.requests.command({ type: "configure_builtin_provider", provider: provider.name })
        break
      case "none":
        this.#host.projectError("provider_auth_unavailable", provider.status ?? `${provider.name} has no safe authentication action`, true)
        break
    }
  }

  render(kind: "models" | "providers" | "providerRecovery" | "providerAuth" | "providerApiKey"): void {
    switch (kind) {
      case "models":
        this.#renderModels()
        break
      case "providers":
        this.#renderProviders()
        break
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
        this.#host.pickerController.show(`CONNECT › ${providerName(provider.name)}`, items, (item) => {
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
          `SIGN IN › ${providerDisplayName({
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

function selectionResolved(state: Pick<RottweilerState, "model" | "models">): boolean {
  const selected = state.model
  return selected !== null && state.models.some(model =>
    model.available !== false && (model.id === selected || model.aliases.includes(selected)))
}

/** First available model of a connected provider, preferring tool calling, in catalog order. */
export function firstUsableModel(
  state: Pick<RottweilerState, "models" | "providers">,
): RottweilerState["models"][number] | undefined {
  const connected = new Set(state.providers
    .filter(provider => provider.configured && provider.authenticated)
    .map(provider => provider.name))
  const usable = state.models.filter(model => model.available !== false && connected.has(model.provider))
  return usable.find(model => model.toolCalling) ?? usable[0]
}

export function modelSupportsVision(state: RottweilerState): boolean {
  const selected = state.models.find((model) => model.current && model.available !== false)
    ?? state.models.find(
      (model) => model.available !== false &&
        (model.id === state.model || model.aliases.includes(state.model ?? "")),
    )
  return selected?.vision === true
}
