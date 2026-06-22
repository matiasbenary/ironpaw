use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::{
    automations::{Automation, AutomationEffect, Automations, Trigger},
    bus::Sender,
    message::ToolDefinition,
    tools::{self, InternalTool, InternalTools, ToolContext, ToolOutput, setup::SetupServices},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct OnboardingParams {
    /// The latest user message.
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Greeting,
    NeedNear,
    NeedSearch,
    Complete,
}

pub struct OnboardingTool {
    stage: Mutex<Stage>,
    setup: Arc<SetupServices>,
}

impl OnboardingTool {
    pub fn new(setup: Arc<SetupServices>) -> Self {
        Self {
            stage: Mutex::new(Stage::Greeting),
            setup,
        }
    }

    pub fn automation() -> Automation {
        Automation {
            name: "onboarding".to_string(),
            trigger: Trigger::MessageIn {
                sender: Some(Sender::User),
                channel: None,
            },
            effect: AutomationEffect::Tool {
                tool: "onboarding".to_string(),
                args: serde_json::json!({
                    "message": "{{event.text}}",
                }),
            },
        }
    }

    async fn execute(
        &self,
        params: OnboardingParams,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        let stage = *self.stage.lock().expect("onboarding stage lock");
        match stage {
            Stage::Greeting => {
                *self.stage.lock().expect("onboarding stage lock") = Stage::NeedNear;
                self.send(
                    "Welcome to Warden. Let's set up the two API keys I need. Paste your NEAR API key first.",
                    ctx,
                )
                .await?;
            }
            Stage::NeedNear => {
                let key = clean_key(&params.message)?;
                self.send("Thanks, let me redact that for you.", ctx)
                    .await?;
                self.delete_current_message(ctx).await;
                write_env_secret(self.env_path()?, "NEAR_API_KEY", &key)?;
                reload_env(self.env_path()?)?;
                self.send("Thanks, let me test the key to make sure it works.", ctx)
                    .await?;
                if let Err(err) = self.test_near(ctx).await {
                    remove_env_secret(self.env_path()?, "NEAR_API_KEY")?;
                    reload_env(self.env_path()?)?;
                    *self.stage.lock().expect("onboarding stage lock") = Stage::NeedNear;
                    self.send(
                        &format!(
                            "The NEAR API key did not pass a basic inference test: {err}. Paste your NEAR API key again."
                        ),
                        ctx,
                    )
                    .await?;
                    return Ok(serde_json::json!({ "ok": false, "error": err }));
                }
                *self.stage.lock().expect("onboarding stage lock") = Stage::NeedSearch;
                self.send("NEAR works. Now paste your Brave Search API key.", ctx)
                    .await?;
            }
            Stage::NeedSearch => {
                let key = clean_key(&params.message)?;
                self.send("Thanks, let me redact that for you.", ctx)
                    .await?;
                self.delete_current_message(ctx).await;
                write_env_secret(self.env_path()?, "BRAVE_SEARCH_API", &key)?;
                reload_env(self.env_path()?)?;
                self.send("Thanks, let me test the key to make sure it works.", ctx)
                    .await?;
                self.test_search_and_finish(ctx).await?;
            }
            Stage::Complete => {
                self.send("Warden is already set up.", ctx).await?;
            }
        }
        Ok(serde_json::json!({ "ok": true }))
    }

    async fn send(&self, content: &str, ctx: &ToolContext) -> Result<(), String> {
        let result = tools::run_tool(
            self.tools()?,
            "send_message",
            serde_json::json!({ "content": content }),
            ctx,
        )
        .await;
        if result.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
            return Err(result["error"]
                .as_str()
                .unwrap_or("send failed")
                .to_string());
        }
        Ok(())
    }

    async fn delete_current_message(&self, ctx: &ToolContext) {
        let Ok(tools) = self.tools() else {
            return;
        };
        let result = tools::run_tool(tools, "delete_message", serde_json::json!({}), ctx).await;
        if result.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
            eprintln!(
                "onboarding could not delete inbound setup message: {}",
                result["error"].as_str().unwrap_or("unknown error")
            );
        }
    }

    async fn test_search_and_finish(&self, ctx: &ToolContext) -> Result<(), String> {
        if let Err(err) = self.test_search(ctx).await {
            remove_env_secret(self.env_path()?, "BRAVE_SEARCH_API")?;
            reload_env(self.env_path()?)?;
            *self.stage.lock().expect("onboarding stage lock") = Stage::NeedSearch;
            self.send(
                &format!(
                    "The Brave Search API key did not pass a basic search test: {err}. Paste your Brave Search API key again."
                ),
                ctx,
            )
            .await?;
            return Err(err);
        }
        self.finish(ctx).await
    }

    async fn test_near(&self, ctx: &ToolContext) -> Result<(), String> {
        let result = tools::run_tool(
            self.tools()?,
            "raw_inference",
            serde_json::json!({
                "messages": [
                    { "User": { "text": "Reply with exactly OK.", "attachments": [] } }
                ]
            }),
            ctx,
        )
        .await;
        tool_result_ok(result)
    }

    async fn test_search(&self, ctx: &ToolContext) -> Result<(), String> {
        let result = tools::run_tool(
            self.tools()?,
            "web_search",
            serde_json::json!({
                "query": "Illia Polosukhin Wikipedia",
                "count": 1
            }),
            ctx,
        )
        .await;
        tool_result_ok(result)
    }

    async fn finish(&self, ctx: &ToolContext) -> Result<(), String> {
        *self.stage.lock().expect("onboarding stage lock") = Stage::Complete;
        let automations = self.automations()?;
        automations.delete("onboarding")?;
        automations.add(Automations::handle_conversation())?;
        self.send(
            "Warden is set up. From now on, I will answer your messages normally.",
            ctx,
        )
        .await
    }

    fn tools(&self) -> Result<&InternalTools, String> {
        Ok(Arc::as_ref(&self.setup.tools))
    }

    fn automations(&self) -> Result<&Arc<Automations>, String> {
        Ok(&self.setup.automations)
    }

    fn env_path(&self) -> Result<&PathBuf, String> {
        Ok(&self.setup.env_path)
    }
}

fn tool_result_ok(result: serde_json::Value) -> Result<(), String> {
    if result.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
        Err(result["error"]
            .as_str()
            .unwrap_or("tool failed")
            .to_string())
    } else {
        Ok(())
    }
}

fn clean_key(message: &str) -> Result<String, String> {
    let key = message.trim();
    if key.is_empty() {
        return Err("API key must not be empty".to_string());
    }
    Ok(key.to_string())
}

fn write_env_secret(path: &PathBuf, name: &str, value: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let replacement = format!("{name}={}\n", quote_env_value(value));
    let mut found = false;
    let mut output = String::new();
    for line in existing.lines() {
        if line.trim_start().starts_with(&format!("{name}=")) {
            output.push_str(&replacement);
            found = true;
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    if !found {
        output.push_str(&replacement);
    }
    std::fs::write(path, output).map_err(|err| err.to_string())
}

fn remove_env_secret(path: &PathBuf, name: &str) -> Result<(), String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut output = String::new();
    for line in existing.lines() {
        if !line.trim_start().starts_with(&format!("{name}=")) {
            output.push_str(line);
            output.push('\n');
        }
    }
    std::fs::write(path, output).map_err(|err| err.to_string())
}

fn quote_env_value(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn reload_env(path: &PathBuf) -> Result<(), String> {
    dotenvy::from_path_override(path).map_err(|err| err.to_string())
}

#[async_trait]
impl InternalTool for OnboardingTool {
    fn name(&self) -> &str {
        "onboarding"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<OnboardingParams>(
            self.name(),
            "First-run Warden setup. Greets the user, collects NEAR_API_KEY \
             and BRAVE_SEARCH_API, writes them to .env, tests raw_inference \
             and web_search, then enables the normal handle-conversation automation.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params: OnboardingParams| async {
            OnboardingTool::execute(self, params, ctx).await
        })
        .await
        .map(ToolOutput::json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_env_secret_appends_and_replaces_values() {
        let path = std::env::temp_dir().join(format!(
            "warden-onboarding-env-test-{}.env",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, "EXISTING=1\nNEAR_API_KEY=\"old\"\n").unwrap();

        write_env_secret(&path, "NEAR_API_KEY", "near-new").unwrap();
        write_env_secret(&path, "BRAVE_SEARCH_API", "brave-new").unwrap();

        let contents = std::fs::read_to_string(path).unwrap();
        assert!(contents.contains("EXISTING=1\n"));
        assert!(contents.contains("NEAR_API_KEY=\"near-new\"\n"));
        assert!(contents.contains("BRAVE_SEARCH_API=\"brave-new\"\n"));
        assert!(!contents.contains("NEAR_API_KEY=\"old\""));
    }

    #[test]
    fn remove_env_secret_removes_only_named_value() {
        let path = std::env::temp_dir().join(format!(
            "warden-onboarding-env-test-{}.env",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            "EXISTING=1\nNEAR_API_KEY=\"bad\"\nBRAVE_SEARCH_API=\"ok\"\n",
        )
        .unwrap();

        remove_env_secret(&path, "NEAR_API_KEY").unwrap();

        let contents = std::fs::read_to_string(path).unwrap();
        assert!(contents.contains("EXISTING=1\n"));
        assert!(contents.contains("BRAVE_SEARCH_API=\"ok\"\n"));
        assert!(!contents.contains("NEAR_API_KEY="));
    }
}
