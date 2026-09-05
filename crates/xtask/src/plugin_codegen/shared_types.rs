use std::{
    any::TypeId,
    collections::{BTreeMap, HashSet},
    path::Path,
};

use rw_types::extension_contract::{
    ExtensionSessionSnapshot, ExtensionStateCommitOutcome, ExtensionStateSnapshot,
    ExtensionStateTransaction,
};
use rw_types::extension_events::{
    ExtensionEventChunk, ExtensionEventKind, ExtensionEventNotice, ExtensionEventOutcome,
    ExtensionEventRead,
};
use rw_types::hook_contract::{HookClass, HookDirective, HookEvent, HookFailurePolicy, HookInput};
use ts_rs::{TS, TypeVisitor};

struct Types {
    seen: HashSet<TypeId>,
    declarations: BTreeMap<String, String>,
}

impl TypeVisitor for Types {
    fn visit<T: TS + 'static + ?Sized>(&mut self) {
        if !self.seen.insert(TypeId::of::<T>()) {
            return;
        }
        if T::output_path().is_some() {
            let config = ts_rs::Config::default();
            self.declarations
                .insert(T::ident(&config), format!("export {}\n", T::decl(&config)));
        }
        T::visit_dependencies(self);
    }
}

pub(super) fn generate(root: &Path, check: bool) -> Result<(), String> {
    let mut types = Types {
        seen: HashSet::new(),
        declarations: BTreeMap::new(),
    };
    types.visit::<HookInput>();
    types.visit::<HookDirective>();
    types.visit::<HookClass>();
    types.visit::<HookEvent>();
    types.visit::<HookFailurePolicy>();
    write_types(root, "hook-contract", types, check)?;
    let mut ir = Types {
        seen: HashSet::new(),
        declarations: BTreeMap::new(),
    };
    ir.visit::<rw_providers::ProviderRequest>();
    ir.visit::<rw_providers::ProviderEvent>();
    write_types(root, "provider-contract", ir, check)?;
    let mut extension = Types {
        seen: HashSet::new(),
        declarations: BTreeMap::new(),
    };
    extension.visit::<ExtensionStateTransaction>();
    extension.visit::<ExtensionStateSnapshot>();
    extension.visit::<ExtensionStateCommitOutcome>();
    extension.visit::<ExtensionSessionSnapshot>();
    extension.visit::<ExtensionEventKind>();
    extension.visit::<ExtensionEventNotice>();
    extension.visit::<ExtensionEventOutcome>();
    extension.visit::<ExtensionEventRead>();
    extension.visit::<ExtensionEventChunk>();
    write_types(root, "extension-contract", extension, check)?;
    let mut ui = Types {
        seen: HashSet::new(),
        declarations: BTreeMap::new(),
    };
    ui.visit::<rw_types::extension_ui::UiContribution>();
    write_types(root, "ui-contract", ui, check)?;
    for (name, schema) in [
        (
            "ui-contribution",
            schema::<rw_types::extension_ui::UiContribution>(),
        ),
        ("extension-event-kind", schema::<ExtensionEventKind>()),
        ("extension-event-notice", schema::<ExtensionEventNotice>()),
        ("extension-event-outcome", schema::<ExtensionEventOutcome>()),
        ("extension-event-chunk", schema::<ExtensionEventChunk>()),
        (
            "extension-state-transaction",
            schema::<ExtensionStateTransaction>(),
        ),
        (
            "extension-state-snapshot",
            schema::<ExtensionStateSnapshot>(),
        ),
        (
            "extension-state-outcome",
            schema::<ExtensionStateCommitOutcome>(),
        ),
        (
            "extension-session-snapshot",
            schema::<ExtensionSessionSnapshot>(),
        ),
        ("hook-input", schema::<HookInput>()),
        ("hook-directive", schema::<HookDirective>()),
        (
            "provider-request",
            schema::<rw_providers::ProviderRequest>(),
        ),
        ("provider-event", schema::<rw_providers::ProviderEvent>()),
    ] {
        let mut schema = schema;
        schema.insert(
            "$comment".to_owned(),
            serde_json::json!(super::GENERATED_MARKER),
        );
        let text = serde_json::to_string_pretty(&schema).map_err(|error| error.to_string())? + "\n";
        super::ensure_output(
            &root.join(format!(
                "packages/plugin-sdk/fixtures/wire/{name}.schema.json"
            )),
            &text,
            check,
        )?;
    }
    let mut validator = std::process::Command::new("bun");
    validator
        .current_dir(root)
        .arg("packages/plugin-sdk/generate-contract-validators.ts");
    if check {
        validator.arg("--check");
    }
    let status = validator
        .status()
        .map_err(|error| format!("plugin validator generator failed: {error}"))?;
    if !status.success() {
        return Err("plugin validator generation failed".to_owned());
    }
    Ok(())
}

fn schema<T: schemars::JsonSchema>() -> schemars::Schema {
    schemars::generate::SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<T>()
}

fn write_types(root: &Path, name: &str, types: Types, check: bool) -> Result<(), String> {
    let declarations = format!(
        "{}\n{}",
        super::GENERATED_MARKER,
        types.declarations.into_values().collect::<String>()
    );
    super::ensure_output(
        &root.join(format!("packages/plugin-sdk/src/generated/{name}.ts")),
        &declarations,
        check,
    )
}
