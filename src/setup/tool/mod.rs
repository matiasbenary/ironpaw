use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;

mod accounts;
mod artifacts;
mod flow;
mod installable;
mod package;
mod prompts;

use crate::{
    automations::{Automation, ToolEffect, UserMessageEffect},
    bus::ConversationKey,
    config::RuntimeLayout,
    secrets::Secrets,
    tools::{self, ExecutionContext, InternalTool, InternalTools, ToolResult},
};

use super::{SessionState, SetupServices};

use installable::InstallableTool;
pub use package::install_packaged_tool;
use package::{installed_tool_manifest, normalize_tool_name};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InstallToolParams {
    /// Tool name to install, for example "web_search".
    pub name: String,
    /// Conversation where the setup flow should continue.
    pub conversation: ConversationKey,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ConfigureToolParams {
    /// Installed tool name, for example "web_search".
    pub name: String,
    /// Conversation where the setup flow should continue.
    pub conversation: ConversationKey,
    /// For tools with several accounts or wallets: go straight to adding a new one (it asks
    /// for the name and offers to create the account) instead of opening the account menu.
    #[serde(default)]
    pub add_account: bool,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetupToolParams {
    /// Tool name being configured.
    pub name: String,
    /// Latest setup reply, filled by the temporary tool_auth automation.
    pub message: String,
    /// Provider message id for the follow-up reply, when available.
    #[serde(default)]
    pub message_id: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListToolsParams {}

#[derive(Clone)]
struct InstallSession {
    conversation: ConversationKey,
    package: InstallableTool,
    mode: SetupMode,
    stage: InstallStage,
    config_values: BTreeMap<String, serde_json::Value>,
    secret_values: BTreeMap<String, String>,
    accounts: BTreeMap<String, serde_json::Value>,
    grants: Vec<crate::network::http::NetworkGrantConfig>,
    account_current: Option<serde_json::Value>,
    removed_secrets: BTreeSet<String>,
    /// Set up one new account and finish, without the account menu.
    add_only: bool,
}

#[derive(Clone)]
enum InstallStage {
    ConfirmInstall,
    ConfirmSecret { index: usize },
    Secret { index: usize },
    RegisterSecret { index: usize },
    ConfirmConfig { index: usize },
    Config { index: usize },
    Autoconfigure(crate::setup::autoconfigure::PendingSetup),
    AccountMenu,
    AccountAction { remove: bool },
    ConfirmRemove { account: String },
    AddAnother,
}

#[derive(Clone)]
enum SetupMode {
    Install,
    Configure { current_config: serde_json::Value },
}

impl InstallSession {
    async fn send(
        &self,
        tools: &InternalTools,
        content: &str,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        super::send(tools, &self.conversation, content, ctx).await
    }

    async fn request_choice(
        &self,
        tools: &InternalTools,
        question: &str,
        options: &[(&str, &str)],
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        super::request_choice(tools, &self.conversation, question, options, ctx).await
    }

    async fn delete_current_message(
        &self,
        tools: &InternalTools,
        ctx: &ExecutionContext,
        message_id: Option<&str>,
    ) {
        super::delete_current_message(tools, &self.conversation, ctx, message_id, "install_tool")
            .await;
    }
}

pub struct InstallToolTool {
    setup: Arc<SetupServices>,
    state: Arc<SessionState<InstallSession>>,
}

pub struct SetupToolTool {
    setup: Arc<SetupServices>,
    state: Arc<SessionState<InstallSession>>,
}

pub struct ConfigureToolTool {
    setup: Arc<SetupServices>,
    state: Arc<SessionState<InstallSession>>,
}

pub struct ListToolsTool {
    layout: RuntimeLayout,
    secrets: Secrets,
}

impl ListToolsTool {
    pub fn new(layout: RuntimeLayout, secrets: Secrets) -> Self {
        Self { layout, secrets }
    }
}

impl InstallToolTool {
    pub fn new(setup: Arc<SetupServices>) -> Self {
        Self {
            setup,
            state: Arc::new(SessionState::new()),
        }
    }

    pub fn setup_tool(&self) -> SetupToolTool {
        SetupToolTool {
            setup: Arc::clone(&self.setup),
            state: Arc::clone(&self.state),
        }
    }

    pub fn configure_tool(&self) -> ConfigureToolTool {
        ConfigureToolTool {
            setup: Arc::clone(&self.setup),
            state: Arc::clone(&self.state),
        }
    }

    fn automation(tool_name: &str, conversation: &ConversationKey) -> Automation {
        Automation::OnUserMessageIn {
            name: format!("tool_auth:{tool_name}"),
            conversation: Some(conversation.clone()),
            effect: UserMessageEffect::Tool(ToolEffect {
                tool: "setup_tool".to_string(),
                args: serde_json::json!({
                    "name": tool_name,
                    "message": "{{event.text}}",
                    "message_id": "{{event.message_id}}",
                }),
            }),
        }
    }

    async fn execute(
        &self,
        params: InstallToolParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_tool_name(&params.name)?;
        self.start(&name, params.conversation, ctx).await
    }

    async fn start(
        &self,
        name: &str,
        conversation: ConversationKey,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let package = InstallableTool::locate(&self.setup.layout, name)?;
        let approval_prompt = package.approval_prompt();
        let session = InstallSession {
            conversation: conversation.clone(),
            package,
            mode: SetupMode::Install,
            stage: InstallStage::ConfirmInstall,
            config_values: BTreeMap::new(),
            secret_values: BTreeMap::new(),
            accounts: BTreeMap::new(),
            grants: Vec::new(),
            account_current: None,
            removed_secrets: BTreeSet::new(),
            add_only: false,
        };
        self.state.save(session.clone());
        self.setup
            .automations
            .add_runtime(Self::automation(name, &conversation))?;
        session
            .request_choice(
                &self.setup.tools,
                &approval_prompt,
                &[("Install", "yes"), ("Cancel", "no")],
                ctx,
            )
            .await?;
        Ok(serde_json::json!({
            "ok": true,
            "tool": name,
            "message": "deterministic tool setup prompt sent"
        }))
    }

    async fn configure(
        &self,
        params: ConfigureToolParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_tool_name(&params.name)?;
        let conversation = params.conversation;
        let package = InstallableTool::locate(&self.setup.layout, &name)?;
        let InstallableTool::Wasm(_) = &package else {
            return Err("shell tools do not have configurable non-secret settings".to_string());
        };
        if !self.setup.tools.registry.contains(&name) {
            return Err(format!("tool '{name}' is not installed"));
        }
        let manifest = installed_tool_manifest(&self.setup.layout, &name)?
            .ok_or_else(|| format!("tool '{name}' is not installed"))?;
        let current_config: serde_json::Value =
            serde_json::from_str(&manifest.config_json).map_err(|err| err.to_string())?;
        let accounts = package
            .accounts()
            .and_then(|setup| current_config.pointer(&setup.path))
            .and_then(serde_json::Value::as_object)
            .map(|accounts| {
                accounts
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let grants =
            serde_json::from_value(current_config["http_credentials"].clone()).unwrap_or_default();
        let account_setup = package.accounts().is_some();
        let add_only = account_setup && params.add_account;
        let config_before_secrets = package.config_before_secrets();
        self.state.save(InstallSession {
            conversation: conversation.clone(),
            package,
            mode: SetupMode::Configure { current_config },
            stage: if account_setup {
                InstallStage::AccountMenu
            } else if config_before_secrets {
                InstallStage::ConfirmConfig { index: 0 }
            } else {
                InstallStage::ConfirmSecret { index: 0 }
            },
            config_values: BTreeMap::new(),
            secret_values: BTreeMap::new(),
            accounts,
            grants,
            account_current: None,
            removed_secrets: BTreeSet::new(),
            add_only,
        });
        self.setup
            .automations
            .add_runtime(Self::automation(&name, &conversation))?;
        let session = self.state.get().expect("configure session");
        if add_only {
            self.setup_tool().start_account(session, None, ctx).await?;
        } else if account_setup {
            self.setup_tool().prompt_account_menu(session, ctx).await?;
        } else if session.package.config_before_secrets() {
            self.setup_tool()
                .prompt_config_or_finish(session, 0, ctx)
                .await?;
        } else {
            self.setup_tool()
                .prompt_secret_or_finish(session, 0, ctx)
                .await?;
        }
        Ok(serde_json::json!({
            "tool": name,
            "message": "deterministic tool configuration prompt sent"
        }))
    }
}

#[async_trait]
impl InternalTool for ListToolsTool {
    tools::tool_metadata!(
        "list_tools",
        ListToolsParams,
        "List installed and installable Ironpaw WASM and shell tools, including \
             each tool's setup requirements. Use this before attempting a capability \
             that is not already provided by a callable tool."
    );

    async fn execute(&self, _args: serde_json::Value, _ctx: &ExecutionContext) -> ToolResult {
        ListToolsTool::execute(self)
            .await
            .map_err(crate::tools::ToolError::new)
    }
}

#[async_trait]
impl InternalTool for InstallToolTool {
    tools::tool_metadata!(
        "install_tool",
        InstallToolParams,
        "Install a local WASM or shell tool package for the supplied conversation. \
             This starts a deterministic setup flow that asks the user \
             for confirmation, configuration, and required secrets without exposing those \
             secret replies to the LLM."
    );

    async fn execute(&self, args: serde_json::Value, ctx: &ExecutionContext) -> ToolResult {
        let params = serde_json::from_value(args)
            .map_err(|error| crate::tools::ToolError::new(error.to_string()))?;
        InstallToolTool::execute(self, params, ctx)
            .await
            .map_err(crate::tools::ToolError::new)
    }

    async fn outcome(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
    ) -> tools::ToolOutcome<ToolResult> {
        let result = InternalTool::execute(self, args, ctx).await;
        match &result {
            Ok(value) if value["ok"] == true => tools::ToolOutcome::EndTurn(result),
            _ => tools::ToolOutcome::Result(result),
        }
    }
}

#[async_trait]
impl InternalTool for ConfigureToolTool {
    tools::tool_metadata!(
        "configure_tool",
        ConfigureToolParams,
        "Reconfigure an installed external WASM tool. This starts a deterministic setup flow \
         that asks before replacing each saved secret or non-secret configuration value. For a \
         tool with several accounts or wallets, pass add_account=true when the user wants a new \
         one: it asks for a name and creates it, and the key stays private."
    );

    async fn execute(&self, args: serde_json::Value, ctx: &ExecutionContext) -> ToolResult {
        let params = serde_json::from_value(args)
            .map_err(|error| crate::tools::ToolError::new(error.to_string()))?;
        InstallToolTool {
            setup: Arc::clone(&self.setup),
            state: Arc::clone(&self.state),
        }
        .configure(params, ctx)
        .await
        .map_err(crate::tools::ToolError::new)
    }

    async fn outcome(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
    ) -> tools::ToolOutcome<ToolResult> {
        match InternalTool::execute(self, args, ctx).await {
            result @ Ok(_) => tools::ToolOutcome::EndTurn(result),
            result @ Err(_) => tools::ToolOutcome::Result(result),
        }
    }
}

#[async_trait]
impl InternalTool for SetupToolTool {
    tools::tool_metadata!(
        "setup_tool",
        SetupToolParams,
        "Continue the deterministic setup flow for a tool installation or configuration. \
         This is called by temporary setup automations with inbound user replies."
    );

    async fn execute(&self, args: serde_json::Value, ctx: &ExecutionContext) -> ToolResult {
        let result: Result<serde_json::Value, String> = async {
            let params: SetupToolParams =
                serde_json::from_value(args).map_err(|err| err.to_string())?;
            let name = normalize_tool_name(&params.name)?;
            Ok(
                match self
                    .continue_setup(&name, &params.message, params.message_id.as_deref(), ctx)
                    .await
                {
                    Ok(result) => result,
                    Err(error) => {
                        self.fail(&name, &error, ctx).await?;
                        serde_json::json!({ "error": error })
                    }
                },
            )
        }
        .await;
        result.map_err(crate::tools::ToolError::new)
    }
}
