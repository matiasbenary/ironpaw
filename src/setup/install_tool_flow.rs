use std::sync::Arc;

use crate::tools::{ExecutionContext, wasm::WasmTool};

use crate::setup::{
    install_common::{config_value, is_yes, secret_value},
    stop_setup_automation,
};

use super::{
    InstallSession, InstallStage, SetupMode, SetupToolTool,
    install_tool_artifacts::{install_package, reconfigure_package, write_account_package},
    install_tool_prompts::{
        collect_account_secrets, rerun_autoconfigure_on_keep, save_secrets, secret_prompt,
    },
    installable_tool::InstallableTool,
};

impl SetupToolTool {
    pub(super) async fn continue_setup(
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
                    self.state.save(session.clone());
                    session.send(&self.setup.tools, &prompt, ctx).await?;
                } else {
                    self.prompt_secret_or_finish(session, index + 1, ctx)
                        .await?;
                }
            }
            InstallStage::Secret { index } => {
                let mut session = session;
                let secret = &session.package.required_secrets()[index];
                let value = secret_value(secret, message)?;
                session.secret_values.insert(secret.name.clone(), value);
                session
                    .delete_current_message(&self.setup.tools, ctx, message_id)
                    .await;
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
                let value = config_value(&session.package.required_config()[index], message)?;
                self.apply_config_value(session, index, value, ctx).await?;
            }
            InstallStage::Autoconfigure(pending) => {
                let Some((secret_name, secret_value)) = pending.finish().await? else {
                    let mut session = session;
                    let prompt = pending.waiting_prompt();
                    session.stage = InstallStage::Autoconfigure(pending);
                    self.state.save(session.clone());
                    session.send(&self.setup.tools, prompt, ctx).await?;
                    return Ok(serde_json::json!({ "ok": true }));
                };
                let mut session = session;
                session.secret_values.insert(secret_name, secret_value);
                if session.package.accounts().is_some() {
                    self.finish_account(session, ctx).await?;
                } else {
                    let config_count = session.package.required_config().len();
                    self.prompt_config_or_finish(session, config_count, ctx)
                        .await?;
                }
            }
            InstallStage::AccountMenu => match message.trim().to_ascii_lowercase().as_str() {
                "add" => self.start_account(session, None, ctx).await?,
                "reconfigure" | "remove" => {
                    let remove = message.trim().eq_ignore_ascii_case("remove");
                    self.prompt_account_choice(session, remove, ctx).await?;
                }
                "finish" => self.complete(session, ctx).await?,
                _ => return Err("choose Add, Reconfigure, Remove, or Finish".to_string()),
            },
            InstallStage::AccountAction { remove } => {
                let account = message.trim().to_string();
                if !session.accounts.contains_key(&account) {
                    return Err(format!("unknown account: {account}"));
                }
                if remove {
                    let mut session = session;
                    session.stage = InstallStage::ConfirmRemove {
                        account: account.clone(),
                    };
                    self.state.save(session.clone());
                    session
                        .request_choice(
                            &self.setup.tools,
                            &format!("Remove {account} and its saved credentials?"),
                            &[("Remove", "yes"), ("Cancel", "no")],
                            ctx,
                        )
                        .await?;
                } else {
                    let current = session.accounts[&account].clone();
                    self.start_account(session, Some(current), ctx).await?;
                }
            }
            InstallStage::ConfirmRemove { account } => {
                let mut session = session;
                if is_yes(message) {
                    if let Some(config) = session.accounts.remove(&account) {
                        collect_account_secrets(
                            &session.package,
                            &config,
                            &mut session.removed_secrets,
                        );
                    }
                    session.grants.retain(|grant| {
                        if grant.account.as_deref() != Some(&account) {
                            return true;
                        }
                        if let Some(oauth) = &grant.oauth {
                            session
                                .removed_secrets
                                .insert(oauth.refresh_token_handle.clone());
                        }
                        false
                    });
                }
                self.prompt_account_menu(session, ctx).await?;
            }
            InstallStage::AddAnother => {
                if is_yes(message) {
                    self.start_account(session, None, ctx).await?;
                } else {
                    self.complete(session, ctx).await?;
                }
            }
        }

        Ok(serde_json::json!({ "ok": true }))
    }

    pub(super) async fn complete(
        &self,
        session: InstallSession,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let configured = matches!(&session.mode, SetupMode::Configure { .. });
        match &session.package {
            InstallableTool::Wasm(package) => {
                let manifest = if package.manifest.accounts.is_some() {
                    write_account_package(
                        &self.setup.layout,
                        package,
                        &session.accounts,
                        &session.grants,
                        configured,
                    )?
                } else if configured {
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
        for secret in &session.removed_secrets {
            self.setup.secrets.remove(secret)?;
        }
        self.finish(session, ctx).await
    }

    pub(super) async fn cancel(
        &self,
        session: InstallSession,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let name = session.package.name().to_string();
        self.state.take();
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
        session
            .send(
                &self.setup.tools,
                if matches!(session.mode, SetupMode::Install) {
                    "Tool installation cancelled. I will answer normally again."
                } else {
                    "Tool configuration cancelled. I will answer normally again."
                },
                ctx,
            )
            .await?;
        Ok(())
    }

    pub(super) async fn fail(
        &self,
        name: &str,
        error: &str,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let session = self
            .state
            .take()
            .ok_or_else(|| "tool setup is not active".to_string())?;
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
        session
            .send(
                &self.setup.tools,
                &format!("The {name} tool could not be configured: {error}"),
                ctx,
            )
            .await
    }

    async fn finish(&self, session: InstallSession, ctx: &ExecutionContext) -> Result<(), String> {
        let name = session.package.name().to_string();
        let configured = matches!(session.mode, SetupMode::Configure { .. });
        self.state.take();
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
        session
            .send(
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
        Ok(())
    }
}
