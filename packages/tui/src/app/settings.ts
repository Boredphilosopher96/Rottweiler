import {
  FuzzyPickerRenderable,
  ListDetailRenderable
} from "../components"
import { PickerController } from "../picker-controller"
import {
  ProjectionRequestBroker,
  type ProjectionKind,
} from "../projection-requests"
import {
  createSettingsBrowserModel,
  type SettingsBrowserAction,
  type SettingsCatalog,
} from "../settings-browser"
import {
  type RottweilerState
} from "../state"

type BudgetSettingKey =
  | "budget.session_cost_cap_micros_usd"
  | "budget.daily_cost_cap_micros_usd"
  | "budget.session_token_cap"
  | "budget.daily_token_cap"
  | "budget.token_rate_alarm_per_minute"
  | "budget.warn_at_percent"
interface SettingsUiHost {
  readonly state: RottweilerState
  readonly picker: FuzzyPickerRenderable<unknown>
  readonly browser: ListDetailRenderable<SettingsBrowserAction>
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly projectionErrors: Readonly<Partial<Record<ProjectionKind, string>>>
  readonly terminalWidth: number
  readonly terminalHeight: number
  readonly statusHeight: number
  readonly composerDockHeight: number
  readonly vim: boolean
  closePicker(): void
  openThemePicker(): void
  modalOpened(): void
}
export class SettingsUiController {
  readonly #host: SettingsUiHost
  #budgetSettingKey: BudgetSettingKey | null = null
  #settingChoiceKey: string | null = null
  constructor(host: SettingsUiHost) { this.#host = host }
  pickerClosed(): void { this.#budgetSettingKey = null; this.#settingChoiceKey = null }
  openSettingsPicker(): void {
    this.#host.pickerController.begin("settings")
    this.resize(
      this.#host.terminalWidth,
      this.#host.terminalHeight,
    )
    this.#host.requests.command({ type: "list_settings" })
    this.#host.pickerController.refresh()
    this.#host.browser.input.focus()
  }

  #activateSettingsAction(action: SettingsBrowserAction): void {
    switch (action.kind) {
      case "choose":
        this.#settingChoiceKey = action.key
        this.#host.browser.visible = false
        this.#host.browser.input.blur()
        this.#host.pickerController.kind = "settingChoices"
        this.#host.pickerController.refresh()
        return
      case "openThemes":
        this.#host.browser.close()
        this.#host.openThemePicker()
        return
      case "openBudgets":
        this.#host.browser.close()
        this.openBudgetPicker()
        return
      case "inspect":
        return
    }
  }

  openBudgetPicker(): void {
    this.#budgetSettingKey = null
    this.#settingChoiceKey = null
    this.#host.pickerController.begin("budgets")
    this.#host.requests.command({ type: "list_settings" })
    this.#host.pickerController.refresh()
  }

  #openBudgetPresetPicker(key: BudgetSettingKey): void {
    this.#budgetSettingKey = key
    this.#host.pickerController.kind = "budgetPresets"
    this.#host.pickerController.refresh()
  }

  #openBudgetTextPrompt(key: BudgetSettingKey): void {
    this.#budgetSettingKey = key
    this.#host.pickerController.kind = "budgetInput"
    const prompt = key === "budget.session_cost_cap_micros_usd"
      ? "Session limit in USD, e.g. 12.50"
      : key === "budget.daily_cost_cap_micros_usd"
        ? "Daily limit in USD, e.g. 12.50"
        : key === "budget.session_token_cap"
          ? "Session token limit, e.g. 250000"
          : key === "budget.daily_token_cap"
            ? "Daily token limit, e.g. 1000000"
            : key === "budget.token_rate_alarm_per_minute"
              ? "Token rate alarm per minute, e.g. 100000"
              : "Warning threshold as a percent, e.g. 70"
    const placeholder = key === "budget.warn_at_percent"
      ? "70"
      : key.includes("token")
        ? "250000"
        : "12.50"
    const scope = this.#host.pickerController.interaction
    this.#host.picker.openTextPrompt({
      title: prompt, placeholder: placeholder, onSubmit: (value) => {
        if (!scope?.active) return
        const selectedKey = key
        this.#host.closePicker()
        if (selectedKey !== null) {
          this.#host.requests.command({ type: "set_setting", key: selectedKey, value })
        }
      }, maxBytes: 32, empty: "reject"
    })
  }

  resize(width: number, height: number): void {
    const primaryHeight = Math.max(
      6,
      height - this.#host.statusHeight - this.#host.composerDockHeight,
    )
    this.#host.browser.resizeForTerminal(width, height, primaryHeight)
  }
  render(kind: "budgets" | "budgetPresets" | "settings" | "settingChoices"): void {
    switch (kind) {
      case "budgets": {
        const rows = [
          {
            key: "budget.session_cost_cap_micros_usd",
            label: "Session limit",
            description: "Maximum spend for this session",
          },
          {
            key: "budget.daily_cost_cap_micros_usd",
            label: "Daily limit",
            description: "Maximum spend per UTC day",
          },
          {
            key: "budget.session_token_cap",
            label: "Session tokens",
            description: "Maximum subscription tokens for this session",
          },
          {
            key: "budget.daily_token_cap",
            label: "Daily tokens",
            description: "Maximum subscription tokens per UTC day",
          },
          {
            key: "budget.token_rate_alarm_per_minute",
            label: "Token rate alarm",
            description: "Alert when one minute of subscription usage reaches this value",
          },
          {
            key: "budget.warn_at_percent",
            label: "Warn at",
            description: "Warn when a configured cap reaches this percentage",
          },
        ] as const
        const settings = rows.map((row) => ({
          ...row,
          setting: this.#host.state.settings.find((setting) => setting.key === row.key),
        }))
        if (settings.some(({ setting }) => setting === undefined)) {
          if (this.#host.requests.current("settings_pending") !== null) {
            this.#host.pickerController.showLoading("Budget limits", "Loading budget limits")
          } else {
            this.#host.pickerController.showStatus(
              "Budget limits",
              "Budget limits could not be loaded",
              "Close and reopen this panel to retry.",
            )
          }
          break
        }
        this.#host.pickerController.show(
          "Budget limits",
          settings.map(({ key, label, description, setting }) => ({
            id: `budget.setting.${key}`,
            label: `${label} · ${setting?.value}`,
            description: `${description} · ${setting?.provenance}${setting?.appliesImmediately ? " · live" : " · next session"}`,
            value: key,
          })),
          (item) => this.#openBudgetPresetPicker(item.value),
        )
        break
      }
      case "budgetPresets": {
        const key = this.#budgetSettingKey
        if (key === null) {
          this.openBudgetPicker()
          break
        }
        const isWarning = key === "budget.warn_at_percent"
        const isToken = key.includes("token")
        const title = key === "budget.session_cost_cap_micros_usd"
          ? "Session limit"
          : key === "budget.daily_cost_cap_micros_usd"
            ? "Daily limit"
            : key === "budget.session_token_cap"
              ? "Session tokens"
              : key === "budget.daily_token_cap"
                ? "Daily tokens"
                : key === "budget.token_rate_alarm_per_minute"
                  ? "Token rate alarm"
                  : "Warn at"
        const presets = isWarning
          ? [
            { label: "50%", value: "50" },
            { label: "75%", value: "75" },
            { label: "80%", value: "80" },
            { label: "90%", value: "90" },
            { label: "Custom…", value: null },
          ]
          : isToken
            ? [
              { label: "50k tokens", value: "50000" },
              { label: "100k tokens", value: "100000" },
              { label: "250k tokens", value: "250000" },
              { label: "1m tokens", value: "1000000" },
              { label: "Unlimited", value: "unlimited" },
              { label: "Custom amount…", value: null },
            ]
            : [
              { label: "$5", value: "5" },
              { label: "$10", value: "10" },
              { label: "$20", value: "20" },
              { label: "$50", value: "50" },
              { label: "$100", value: "100" },
              { label: "Unlimited", value: "unlimited" },
              { label: "Custom amount…", value: null },
            ]
        this.#host.pickerController.show(
          title,
          presets.map((preset) => ({
            id: `budget.preset.${key}.${preset.value ?? "custom"}`,
            label: preset.label,
            description: preset.value === null
              ? isWarning
                ? "Enter a custom warning percentage"
                : isToken
                  ? "Enter a positive whole-token amount"
                  : "Enter a USD amount with up to two decimals"
              : isWarning
                ? `Warn at ${preset.label} of every configured cap`
                : preset.value === "unlimited"
                  ? `Remove the ${title.toLowerCase()} cap`
                  : `Set the ${title.toLowerCase()} to ${preset.label}`,
            value: preset.value,
          })),
          (item) => {
            if (item.value === null) {
              this.#openBudgetTextPrompt(key)
              return
            }
            this.#host.closePicker()
            this.#host.requests.command({ type: "set_setting", key, value: item.value })
          },
        )
        break
      }
      case "settings": {
        this.resize(
          this.#host.terminalWidth,
          this.#host.terminalHeight,
        )
        const catalog: SettingsCatalog = this.#host.projectionErrors.settings === undefined
          ? this.#host.state.settings.length === 0 && this.#host.requests.current("settings_pending") !== null
            ? { kind: "loading" }
            : { kind: "ready", settings: this.#host.state.settings }
          : {
            kind: "error",
            message: this.#host.projectionErrors.settings,
            stale: this.#host.state.settings,
          }
        const query = this.#host.browser.visible
          ? this.#host.browser.input.value
          : this.#host.pickerController.query
        const preserveSelection = query === this.#host.pickerController.query
        this.#host.pickerController.query = query
        const model = createSettingsBrowserModel({
          catalog,
          query,
          selectedId: this.#host.browser.visible && preserveSelection
            ? this.#host.browser.selectedId
            : null,
        })
        const presentation = this.#host.vim && model.status.includes("Esc close")
          ? { ...model, status: model.status.replace("Esc close", "Esc×2 close") }
          : model
        if (this.#host.browser.visible) {
          this.#host.browser.refresh(presentation)
        } else {
          this.#host.browser.open(presentation, (action) => {
            this.#activateSettingsAction(action)
          }, {
            onQuery: () => this.#host.pickerController.refresh(),
            onSelection: () => this.#host.pickerController.refresh(),
            onRetry: () => {
              this.#host.requests.command({ type: "list_settings" })
              this.#host.pickerController.refresh()
            },
          })
          this.#host.modalOpened()
        }
        break
      }
      case "settingChoices": {
        const setting = this.#host.state.settings.find((candidate) => candidate.key === this.#settingChoiceKey)
        if (setting === undefined || setting.choices.length === 0) {
          this.#host.pickerController.showStatus(
            "Setting choices",
            "No choices available",
            "Close this panel and refresh settings.",
          )
          break
        }
        this.#host.pickerController.show(
          setting.label,
          setting.choices.map((value) => ({
            id: value,
            label: value,
            description: value === setting.value
              ? `current · ${setting.provenance}`
              : setting.provenance,
            value,
          })),
          (item) => {
            const key = this.#settingChoiceKey
            if (key === null) return
            this.#host.picker.close()
            this.#settingChoiceKey = null
            this.#host.pickerController.kind = "settings"
            this.#host.browser.visible = true
            this.#host.browser.input.focus()
            this.#host.requests.command({
              type: "set_setting",
              key,
              value: item.value,
            })
          },
        )
        break
      }
    }
  }
}
