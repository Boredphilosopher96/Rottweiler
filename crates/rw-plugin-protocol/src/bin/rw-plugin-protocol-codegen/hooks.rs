use std::{
    any::TypeId,
    collections::{BTreeMap, HashSet},
    path::Path,
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
    let declarations = format!(
        "{}\n{}",
        super::GENERATED_MARKER,
        types.declarations.into_values().collect::<String>()
    );
    super::ensure_output(
        &root.join("packages/plugin-sdk/src/generated/hook-contract.ts"),
        &declarations,
        check,
    )?;
    for (name, schema) in [
        ("hook-input", schema::<HookInput>()),
        ("hook-directive", schema::<HookDirective>()),
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
        .arg("packages/plugin-sdk/generate-hook-validators.ts");
    if check {
        validator.arg("--check");
    }
    let status = validator
        .status()
        .map_err(|error| format!("hook validator generator failed: {error}"))?;
    if !status.success() {
        return Err("hook validator generation failed".to_owned());
    }
    Ok(())
}

fn schema<T: schemars::JsonSchema>() -> schemars::Schema {
    let mut settings = schemars::generate::SchemaSettings::draft2020_12();
    settings.contract = schemars::generate::Contract::Serialize;
    settings.into_generator().into_root_schema_for::<T>()
}
