pub mod ai_inference;

use crate::{
    http::{CredentialInjection, HttpError, HttpTool},
    secrets::ToolSecrets,
};

use self::ai_inference::AiInferenceTool;

#[derive(Clone)]
pub struct ToolsRuntime {
    pub(crate) ai_inference: AiInferenceTool,
}

impl ToolsRuntime {
    pub fn new(secrets: ToolSecrets) -> Result<Self, HttpError> {
        let http = HttpTool::new(
            secrets,
            vec![CredentialInjection::bearer(
                "cloud-api.near.ai",
                "NEAR_API_KEY",
            )],
        )?;

        Ok(Self {
            ai_inference: AiInferenceTool::new(http),
        })
    }
}
