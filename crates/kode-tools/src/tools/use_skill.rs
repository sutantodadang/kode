use std::sync::Arc;

use serde::Deserialize;

use crate::error::{Result, ToolError};
use crate::skills::SkillCatalog;
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    name: String,
    path: Option<String>,
}

pub struct UseSkill {
    catalog: Arc<SkillCatalog>,
}

impl UseSkill {
    pub fn new(catalog: Arc<SkillCatalog>) -> Self {
        Self { catalog }
    }
}

#[async_trait::async_trait]
impl Tool for UseSkill {
    fn name(&self) -> &str {
        "use_skill"
    }

    fn description(&self) -> &str {
        "Read an available skill's SKILL.md or a relative resource from that skill. Call this before following a named or relevant skill."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name, with or without a leading $"
                },
                "path": {
                    "type": "string",
                    "description": "Optional resource path relative to the skill root; defaults to SKILL.md"
                }
            },
            "required": ["name"]
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::ReadOnly
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: Args = serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: error.to_string(),
        })?;
        let content = self.catalog.read(&args.name, args.path.as_deref())?;
        Ok(ToolOutput { content })
    }
}
