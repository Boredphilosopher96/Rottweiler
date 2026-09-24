//! Serves discovered skills to the built-in `skill` tool.

use rw_ext::ExtensionCatalog;
use rw_tools::{SkillDocument, SkillLibrary, SkillTool, ToolError, ToolRegistry};
use std::sync::Arc;

/// Skill source backed by one discovery generation.
pub(crate) struct CatalogSkillLibrary {
    catalog: Arc<ExtensionCatalog>,
}

impl CatalogSkillLibrary {
    pub(crate) fn new(catalog: Arc<ExtensionCatalog>) -> Self {
        Self { catalog }
    }
}

impl SkillLibrary for CatalogSkillLibrary {
    fn load(&self, name: &str, path: Option<&str>) -> Result<SkillDocument, String> {
        let skill = self.catalog.skill(name).ok_or_else(|| {
            let available = self.catalog.skills().len();
            format!("unknown skill `{name}`; {available} skills are available in this session")
        })?;
        let content = match path {
            None => skill.render_invocation(""),
            Some(path) => skill.read_bundled_file(path),
        }
        .map_err(|error| format!("skill `{name}` could not be loaded: {error}"))?;
        Ok(SkillDocument {
            name: skill.name().to_owned(),
            path: path.map(str::to_owned),
            content,
        })
    }
}

/// Registers the `skill` tool when this generation discovered any skill.
///
/// # Errors
///
/// Fails only when a tool named `skill` is already registered.
pub(crate) fn register_skill_tool(
    tools: &mut ToolRegistry,
    catalog: &Arc<ExtensionCatalog>,
) -> Result<(), ToolError> {
    if catalog.skills().len() == 0 {
        return Ok(());
    }
    tools.register(Arc::new(SkillTool::new(Arc::new(
        CatalogSkillLibrary::new(Arc::clone(catalog)),
    ))))
}
