use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;

#[path = "install_tool_package.rs"]
mod install_tool_package;
#[path = "installable_tool.rs"]
mod installable_tool;

use crate::{
    automations::{Automation, AutomationEffect, Trigger},
    bus::{ConversationKey, InvocationScope, SenderKind},
    config::RuntimeLayout,
    secrets::Secrets,
    tools::{
        self, ExecutionContext, InternalTool, ToolResponder,
        setup::{
            SessionState, SetupServices, delete_current_message,
            install_common::{SetupSecret, config_prompt, config_value, is_yes, secret_value},
            pause_conversation, publish_tool_result, request_choice, send, stop_setup_automation,
        },
        wasm::WasmTool,
    },
};

pub use install_tool_package::install_packaged_tool;
use install_tool_package::{
    install_package, installed_tool_manifest, normalize_tool_name, reconfigure_package,
};
use installable_tool::InstallableTool;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InstallToolParams {
    /// Tool name to install, for example "web_search".
    pub name: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ConfigureToolParams {
    /// Installed tool name, for example "web_search".
    pub name: String,
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
    package: InstallableTool,
    mode: SetupMode,
    stage: InstallStage,
    config_values: BTreeMap<String, serde_json::Value>,
    secret_values: BTreeMap<String, String>,
    continuation: Option<ToolResponder>,
}

#[derive(Clone)]
enum InstallStage {
    ConfirmInstall,
    ConfirmSecret { index: usize },
    Secret { index: usize },
    ConfirmConfig { index: usize },
    Config { index: usize },
    Autoconfigure(crate::tools::package_support::PendingSetup),
}

#[derive(Clone)]
enum SetupMode {
    Install,
    Configure { current_config: serde_json::Value },
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
        Automation {
            name: format!("tool_auth:{tool_name}"),
            trigger: Trigger::MessageIn {
                sender: SenderKind::User,
                conversation: Some(conversation.clone()),
            },
            effect: AutomationEffect::Tool {
                tool: "setup_tool".to_string(),
                args: serde_json::json!({
                    "name": tool_name,
                    "message": "{{event.text}}",
                    "message_id": "{{event.message_id}}",
                }),
            },
        }
    }

    async fn execute(
        &self,
        params: InstallToolParams,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_tool_name(&params.name)?;
        self.start(&name, ctx, responder).await
    }

    async fn start(
        &self,
        name: &str,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let conversation = match &ctx.invoked_by {
            InvocationScope::Conversation(conversation) => conversation,
            InvocationScope::ScheduledAutomation { automation_id } => {
                return Err(format!(
                    "install_tool cannot start from scheduled automation '{automation_id}'"
                ));
            }
        };
        let package = InstallableTool::locate(&self.setup.layout, name)?;
        let approval_prompt = package.approval_prompt();
        self.state.save(InstallSession {
            package,
            mode: SetupMode::Install,
            stage: InstallStage::ConfirmInstall,
            config_values: BTreeMap::new(),
            secret_values: BTreeMap::new(),
            continuation: Some(responder),
        });
        pause_conversation(&self.setup.automations)?;
        self.setup
            .automations
            .add(Self::automation(name, conversation))?;
        request_choice(
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
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_tool_name(&params.name)?;
        let conversation = match &ctx.invoked_by {
            InvocationScope::Conversation(conversation) => conversation,
            InvocationScope::ScheduledAutomation { automation_id } => {
                return Err(format!(
                    "configure_tool cannot start from scheduled automation '{automation_id}'"
                ));
            }
        };
        let package = InstallableTool::locate(&self.setup.layout, &name)?;
        let InstallableTool::Wasm(_) = &package else {
            return Err("shell tools do not have configurable non-secret settings".to_string());
        };
        if !self.setup.tools.registry.contains(&name) {
            return Err(format!("tool '{name}' is not installed"));
        }
        let manifest = installed_tool_manifest(&self.setup.layout, &name)?
            .ok_or_else(|| format!("tool '{name}' is not installed"))?;
        let current_config =
            serde_json::from_str(&manifest.config_json).map_err(|err| err.to_string())?;
        let config_before_secrets = package.config_before_secrets();
        self.state.save(InstallSession {
            package,
            mode: SetupMode::Configure { current_config },
            stage: if config_before_secrets {
                InstallStage::ConfirmConfig { index: 0 }
            } else {
                InstallStage::ConfirmSecret { index: 0 }
            },
            config_values: BTreeMap::new(),
            secret_values: BTreeMap::new(),
            continuation: Some(responder),
        });
        pause_conversation(&self.setup.automations)?;
        self.setup
            .automations
            .add(Self::automation(&name, conversation))?;
        let session = self.state.get().expect("configure session");
        if session.package.config_before_secrets() {
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

impl SetupToolTool {
    async fn continue_setup(
        &self,
        name: &str,
        message: &str,
        message_id: Option<&str>,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let session = self
            .state
            .get()
            .ok_or_else(|| "tool setup is not active; ask to install the tool again".to_string())?;
        if session.package.name() != name {
            return Err(format!(
                "tool setup is active for '{}', not '{name}'",
                session.package.name()
            ));
        }

        match session.stage.clone() {
            InstallStage::ConfirmInstall => {
                if !is_yes(message) {
                    self.cancel(session, ctx).await?;
                    return Ok(serde_json::json!({ "ok": true, "cancelled": true }));
                }
                if session.package.config_before_secrets() {
                    self.prompt_config_or_finish(session, 0, ctx).await?;
                } else {
                    self.prompt_secret_or_finish(session, 0, ctx).await?;
                }
            }
            InstallStage::ConfirmSecret { index } => {
                if is_yes(message) {
                    let prompt =
                        secret_prompt(&session.package, &session.package.required_secrets()[index]);
                    let mut session = session;
                    session.stage = InstallStage::Secret { index };
                    self.state.save(session);
                    send(&self.setup.tools, &prompt, ctx).await?;
                } else {
                    self.prompt_secret_or_finish(session, index + 1, ctx)
                        .await?;
                }
            }
            InstallStage::Secret { index } => {
                let mut session = session;
                let secrets = session.package.required_secrets();
                let secret = &secrets[index];
                let value = secret_value(secret, message)?;
                session.secret_values.insert(secret.name.clone(), value);
                delete_current_message(&self.setup.tools, ctx, message_id, "install_tool").await;
                self.prompt_secret_or_finish(session, index + 1, ctx)
                    .await?;
            }
            InstallStage::ConfirmConfig { index } => {
                if is_yes(message) {
                    self.prompt_config_value(session, index, ctx).await?;
                } else if rerun_autoconfigure_on_keep(&session.package.required_config()[index]) {
                    let value = match &session.mode {
                        SetupMode::Configure { current_config } => current_config
                            .pointer(&session.package.required_config()[index].path)
                            .cloned()
                            .ok_or_else(|| "saved configuration value is missing".to_string())?,
                        SetupMode::Install => {
                            return Err("cannot keep config during installation".to_string());
                        }
                    };
                    self.apply_config_value(session, index, value, ctx).await?;
                } else {
                    self.prompt_config_or_finish(session, index + 1, ctx)
                        .await?;
                }
            }
            InstallStage::Config { index } => {
                let session = session;
                let configs = session.package.required_config();
                let config = &configs[index];
                let value = config_value(config, message)?;
                self.apply_config_value(session, index, value, ctx).await?;
            }
            InstallStage::Autoconfigure(pending) => {
                let Some((secret_name, secret_value)) = pending.finish().await? else {
                    let mut session = session;
                    let prompt = pending.waiting_prompt();
                    session.stage = InstallStage::Autoconfigure(pending);
                    self.state.save(session);
                    send(&self.setup.tools, prompt, ctx).await?;
                    return Ok(serde_json::json!({ "ok": true }));
                };
                let mut session = session;
                session.secret_values.insert(secret_name, secret_value);
                let config_count = session.package.required_config().len();
                self.prompt_config_or_finish(session, config_count, ctx)
                    .await?;
            }
        }

        Ok(serde_json::json!({ "ok": true }))
    }

    async fn apply_config_value(
        &self,
        mut session: InstallSession,
        index: usize,
        value: serde_json::Value,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let config = session.package.required_config()[index].clone();
        session
            .config_values
            .insert(config.path.clone(), value.clone());
        if let Some(setup_action) = config.autoconfigure {
            let handler = session.package.setup_handler().ok_or_else(|| {
                format!(
                    "tool '{}' declares setup action '{}' without a setup handler",
                    session.package.name(),
                    setup_action.action
                )
            })?;
            let values = crate::tools::package_support::run_setup_action(
                handler,
                &setup_action.action,
                &value,
            )
            .await?;
            session.config_values.extend(values);
        }
        self.prompt_config_or_finish(session, index + 1, ctx).await
    }

    async fn authorize_or_complete(
        &self,
        mut session: InstallSession,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        if let SetupMode::Configure { current_config } = &session.mode {
            for config in session.package.required_config() {
                if let Some(value) = current_config.pointer(&config.path) {
                    session
                        .config_values
                        .entry(config.path)
                        .or_insert_with(|| value.clone());
                }
            }
        }
        let oauth = session
            .package
            .required_config()
            .into_iter()
            .filter_map(|config| {
                if !condition_matches(config.when.as_ref(), &session.config_values, &session.mode) {
                    return None;
                }
                let selected = session.config_values.get(&config.path)?.as_str()?;
                config
                    .choices
                    .into_iter()
                    .find(|choice| choice.value == selected)?
                    .oauth
            })
            .next();
        let Some(oauth) = oauth else {
            return self.complete(session, ctx).await;
        };
        if session
            .secret_values
            .contains_key(&oauth.refresh_token_handle)
        {
            return self.complete(session, ctx).await;
        }
        let client_secret = oauth
            .client_secret
            .clone()
            .map(Ok)
            .or_else(|| {
                oauth.client_secret_handle.as_ref().map(|name| {
                    session
                        .secret_values
                        .get(name)
                        .cloned()
                        .or_else(|| match &session.mode {
                            SetupMode::Configure { .. } => self
                                .setup
                                .secrets
                                .get(&session.package.secret_store_name(name)),
                            SetupMode::Install => None,
                        })
                        .ok_or_else(|| {
                            format!("OAuth client secret was not collected during setup: {name}")
                        })
                })
            })
            .transpose()?;
        let pending = crate::tools::package_support::start_oauth(
            &oauth,
            &session.config_values,
            client_secret,
        )
        .await?;
        let prompt = pending.prompt();
        session.stage = InstallStage::Autoconfigure(pending);
        self.state.save(session);
        send(&self.setup.tools, &prompt, ctx).await
    }

    async fn prompt_secret_or_finish(
        &self,
        mut session: InstallSession,
        start: usize,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let Some((index, secret)) = session
            .package
            .required_secrets()
            .into_iter()
            .enumerate()
            .skip(start)
            .find(|(_, secret)| {
                condition_matches(secret.when.as_ref(), &session.config_values, &session.mode)
            })
        else {
            return if session.package.config_before_secrets() {
                self.authorize_or_complete(session, ctx).await
            } else {
                Box::pin(self.prompt_config_or_finish(session, 0, ctx)).await
            };
        };
        if matches!(session.mode, SetupMode::Configure { .. })
            && self
                .setup
                .secrets
                .exists(&session.package.secret_store_name(&secret.name))
        {
            session.stage = InstallStage::ConfirmSecret { index };
            self.state.save(session);
            return request_choice(
                &self.setup.tools,
                &format!(
                    "{}: a value is already saved for {}. Do you want to change it?",
                    secret.name, secret.name
                ),
                &[("Change", "yes"), ("Keep", "no")],
                ctx,
            )
            .await;
        }
        let prompt = secret_prompt(&session.package, &secret);
        session.stage = InstallStage::Secret { index };
        self.state.save(session);
        send(&self.setup.tools, &prompt, ctx).await
    }

    async fn prompt_config_or_finish(
        &self,
        mut session: InstallSession,
        start: usize,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let Some((index, config)) = session
            .package
            .required_config()
            .into_iter()
            .enumerate()
            .skip(start)
            .find(|(_, config)| {
                condition_matches(config.when.as_ref(), &session.config_values, &session.mode)
                    && !session.config_values.contains_key(&config.path)
            })
        else {
            return if session.package.config_before_secrets() {
                self.prompt_secret_or_finish(session, 0, ctx).await
            } else {
                self.authorize_or_complete(session, ctx).await
            };
        };
        if let SetupMode::Configure { current_config } = &session.mode
            && let Some(value) = saved_config_value(current_config, &config.path)
        {
            let prompt = format!(
                "{} currently has value {}. Do you want to change it?",
                config_prompt(&config),
                display_config_value(value)
            );
            session.stage = InstallStage::ConfirmConfig { index };
            self.state.save(session);
            return request_choice(
                &self.setup.tools,
                &prompt,
                &[("Change", "yes"), ("Keep", "no")],
                ctx,
            )
            .await;
        }
        self.prompt_config_value(session, index, ctx).await
    }

    async fn prompt_config_value(
        &self,
        mut session: InstallSession,
        index: usize,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let config = session.package.required_config()[index].clone();
        session.stage = InstallStage::Config { index };
        self.state.save(session);
        if config.choices.is_empty() {
            send(&self.setup.tools, config_prompt(&config), ctx).await
        } else {
            let choices = config
                .choices
                .iter()
                .map(|choice| (choice.label.as_str(), choice.value.as_str()))
                .collect::<Vec<_>>();
            request_choice(&self.setup.tools, config_prompt(&config), &choices, ctx).await
        }
    }

    async fn complete(
        &self,
        session: InstallSession,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let configured = matches!(&session.mode, SetupMode::Configure { .. });
        match &session.package {
            InstallableTool::Wasm(package) => {
                let manifest = if configured {
                    reconfigure_package(&self.setup.layout, package, &session.config_values)?
                } else {
                    install_package(&self.setup.layout, package, &session.config_values)?
                };
                save_secrets(&self.setup.secrets, &session)?;
                let tool = WasmTool::from_manifest(manifest, self.setup.secrets.tools.clone())?;
                if configured {
                    self.setup.tools.registry.replace(Arc::new(tool))?;
                } else {
                    self.setup
                        .tools
                        .registry
                        .add(Arc::new(tool), crate::tools::ToolVisibility::Agent)?;
                }
            }
            InstallableTool::Shell(package) => {
                if configured {
                    return Err("shell tools cannot be reconfigured".to_string());
                }
                self.setup
                    .tools
                    .shell_executor
                    .install(
                        Arc::new(package.manifest.clone()),
                        package.source_dir.clone(),
                    )
                    .await?;
                save_secrets(&self.setup.secrets, &session)?;
            }
        }
        self.setup.tool_state.enable(session.package.name())?;
        self.finish(session, ctx).await
    }

    async fn cancel(&self, session: InstallSession, ctx: &ExecutionContext) -> Result<(), String> {
        let name = session.package.name().to_string();
        let continuation = session.continuation.clone();
        self.state.take();
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
        send(
            &self.setup.tools,
            if matches!(session.mode, SetupMode::Install) {
                "Tool installation cancelled. I will answer normally again."
            } else {
                "Tool configuration cancelled. I will answer normally again."
            },
            ctx,
        )
        .await?;
        publish_tool_result(
            continuation.as_ref(),
            Ok(serde_json::json!({ "tool": name, "cancelled": true })),
        );
        Ok(())
    }

    async fn fail(&self, name: &str, error: &str, ctx: &ExecutionContext) -> Result<(), String> {
        let session = self.state.take();
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
        publish_tool_result(
            session
                .as_ref()
                .and_then(|session| session.continuation.as_ref()),
            Err(crate::tools::ToolError::new(error)),
        );
        send(
            &self.setup.tools,
            &format!("The {name} tool could not be configured: {error}"),
            ctx,
        )
        .await
    }

    async fn finish(&self, session: InstallSession, ctx: &ExecutionContext) -> Result<(), String> {
        let name = session.package.name().to_string();
        let configured = matches!(session.mode, SetupMode::Configure { .. });
        let continuation = session.continuation.clone();
        self.state.take();
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
        send(
            &self.setup.tools,
            &format!(
                "The {name} tool is {}.",
                if configured {
                    "configured"
                } else {
                    "installed"
                }
            ),
            ctx,
        )
        .await?;
        let result = if configured {
            serde_json::json!({ "tool": name, "configured": true })
        } else {
            serde_json::json!({ "tool": name, "installed": true })
        };
        publish_tool_result(continuation.as_ref(), Ok(result));
        Ok(())
    }
}

fn condition_matches(
    condition: Option<&crate::tools::setup::install_common::SetupCondition>,
    values: &BTreeMap<String, serde_json::Value>,
    mode: &SetupMode,
) -> bool {
    let Some(condition) = condition else {
        return true;
    };
    values.get(&condition.path).or_else(|| match mode {
        SetupMode::Configure { current_config } => current_config.pointer(&condition.path),
        SetupMode::Install => None,
    }) == Some(&condition.equals)
}

fn saved_config_value<'a>(
    config: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    config.pointer(path).filter(|value| match value {
        serde_json::Value::Null => false,
        serde_json::Value::String(value) => !value.trim().is_empty(),
        _ => true,
    })
}

fn display_config_value(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn rerun_autoconfigure_on_keep(config: &crate::tools::setup::install_common::SetupConfig) -> bool {
    config.choices.iter().any(|choice| choice.oauth.is_some())
        || config
            .autoconfigure
            .as_ref()
            .is_some_and(|setup| setup.rerun_on_keep)
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

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(
            ListToolsTool::execute(self)
                .await
                .map_err(crate::tools::ToolError::new),
        );
    }
}

#[async_trait]
impl InternalTool for InstallToolTool {
    tools::tool_metadata!(
        "install_tool",
        InstallToolParams,
        "Install a local WASM or shell tool package. When called with only \
             `name`, this starts a deterministic setup flow that asks the user \
             for confirmation, configuration, and required secrets without exposing those \
             secret replies to the LLM."
    );

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = async {
            let params = serde_json::from_value(args).map_err(|err| err.to_string())?;
            InstallToolTool::execute(self, params, ctx, responder.clone()).await
        }
        .await;
        match result {
            Ok(result) if result["ok"] == true => {}
            Ok(result) => responder.respond(Ok(result)),
            Err(error) => responder.respond(Err(crate::tools::ToolError::new(error))),
        }
    }
}

#[async_trait]
impl InternalTool for ConfigureToolTool {
    tools::tool_metadata!(
        "configure_tool",
        ConfigureToolParams,
        "Reconfigure an installed external WASM tool. This starts a deterministic setup flow \
         that asks before replacing each saved secret or non-secret configuration value."
    );

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = async {
            let params = serde_json::from_value(args).map_err(|err| err.to_string())?;
            InstallToolTool {
                setup: Arc::clone(&self.setup),
                state: Arc::clone(&self.state),
            }
            .configure(params, ctx, responder.clone())
            .await
        }
        .await;
        match result {
            Ok(_) => {}
            Err(error) => responder.respond(Err(crate::tools::ToolError::new(error))),
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

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
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
        responder.respond(result.map_err(crate::tools::ToolError::new));
    }
}

fn save_secrets(secrets: &Secrets, session: &InstallSession) -> Result<(), String> {
    for (name, value) in &session.secret_values {
        secrets.set(&session.package.secret_store_name(name), value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{display_config_value, saved_config_value};

    #[test]
    fn empty_template_values_are_not_treated_as_saved_configuration() {
        let config = serde_json::json!({
            "email": { "provider": "", "username": "person@example.com" }
        });

        assert!(saved_config_value(&config, "/email/provider").is_none());
        assert_eq!(
            saved_config_value(&config, "/email/username"),
            Some(&serde_json::json!("person@example.com"))
        );
    }

    #[test]
    fn displays_string_values_without_json_quotes() {
        assert_eq!(
            display_config_value(&serde_json::json!("readonly")),
            "readonly"
        );
    }
}

fn secret_prompt(package: &InstallableTool, secret: &SetupSecret) -> String {
    let mut prompt = format!(
        "{}: {}",
        package.display_name(),
        secret.prompt.as_deref().unwrap_or(&secret.name)
    );
    if let Some(setup_url) = package.setup_url() {
        prompt.push_str(&format!(" Setup: {setup_url}"));
    }
    prompt
}
