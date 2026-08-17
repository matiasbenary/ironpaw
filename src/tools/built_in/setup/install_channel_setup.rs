use std::sync::Arc;

use crate::{
    automations::Automations,
    bus::{ConversationKey, SenderKind},
    config::RuntimeLayout,
    tools::{
        ExecutionContext, InternalTools,
        setup::{
            delete_current_message,
            install_common::{
                config_prompt, config_value, is_yes, secret_value, stored_secret_name,
            },
            publish_tool_result, request_choice, send, stop_setup_automation,
        },
    },
};

use super::{
    ChannelPackage, InstallSession, InstallStage, SetupChannelTool,
    install_channel_package::{
        configure_generated_secrets, remove_installed_package, required_config, secret_prompt,
        set_channel_owner_id, verification_code, write_package,
    },
    is_setup_conversation,
};

impl SetupChannelTool {
    pub(super) async fn continue_setup(
        &self,
        name: &str,
        channel: Option<&str>,
        channel_user_id: Option<&str>,
        conversation: &ConversationKey,
        message: &str,
        message_id: Option<&str>,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let tools = Arc::as_ref(&self.setup.tools);
        let automations = &self.setup.automations;
        let layout = &self.setup.layout;
        let session = self.state.get().ok_or_else(|| {
            "channel setup is not active; ask to install the channel again".to_string()
        })?;
        if session.package.name != name {
            return Err(format!(
                "channel setup is active for '{}', not '{name}'",
                session.package.name
            ));
        }

        match session.stage {
            InstallStage::Confirm => {
                if !is_yes(message) {
                    self.cancel(session, tools, automations, conversation, ctx)
                        .await?;
                    return Ok(serde_json::json!({ "ok": true, "cancelled": true }));
                }
                let mut session = session;
                configure_generated_secrets(&mut session.secrets, &self.setup.secrets)?;
                if session.secrets.is_empty() {
                    self.prompt_config_or_finish(session, layout, tools, conversation, ctx)
                        .await?;
                    return Ok(serde_json::json!({ "ok": true }));
                }
                session.stage = InstallStage::Secret { index: 0 };
                self.state.save(session.clone());
                send(
                    tools,
                    conversation,
                    &secret_prompt(&session.package, &session.secrets[0]),
                    ctx,
                )
                .await?;
            }
            InstallStage::Secret { index } => {
                let mut session = session;
                let secret = &session.secrets[index];
                let value = secret_value(secret, message)?;
                self.setup
                    .secrets
                    .set(&stored_secret_name(&secret.name), &value)?;
                delete_current_message(tools, conversation, ctx, message_id, "install_channel")
                    .await;

                let next = index + 1;
                if let Some(secret) = session.secrets.get(next) {
                    session.stage = InstallStage::Secret { index: next };
                    self.state.save(session.clone());
                    send(
                        tools,
                        conversation,
                        &secret_prompt(&session.package, secret),
                        ctx,
                    )
                    .await?;
                } else {
                    self.prompt_config_or_finish(session, layout, tools, conversation, ctx)
                        .await?;
                }
            }
            InstallStage::Config { index } => {
                let mut session = session;
                let configs = required_config(&session.package);
                let config = &configs[index];
                session
                    .config_values
                    .insert(config.path.clone(), config_value(config, message)?);

                let next = index + 1;
                if let Some(config) = configs.get(next) {
                    session.stage = InstallStage::Config { index: next };
                    self.state.save(session.clone());
                    send(tools, conversation, config_prompt(config), ctx).await?;
                } else {
                    self.complete(session, layout, tools, conversation, ctx)
                        .await?;
                }
            }
            InstallStage::Verify {
                activated,
                ref code,
            } => {
                if activated
                    && channel == Some(session.package.name.as_str())
                    && message.trim().eq_ignore_ascii_case(code)
                {
                    let Some(channel_user_id) = channel_user_id else {
                        send(
                            tools,
                            conversation,
                            "I received the verification code, but this channel did not provide a stable user id. Continue setup from the original conversation.",
                            ctx,
                        )
                        .await?;
                        return Ok(serde_json::json!({ "verified": false }));
                    };
                    set_channel_owner_id(layout, &session.package, channel_user_id)?;
                    self.activate_package(&session.package).await?;
                    self.finish(session, tools, automations, conversation, ctx, true)
                        .await?;
                    return Ok(serde_json::json!({ "ok": true, "verified": true }));
                }

                let is_setup_conversation = is_setup_conversation(&session, conversation);
                match message.trim().to_ascii_lowercase().as_str() {
                    _ if !is_setup_conversation
                        && channel == Some(session.package.name.as_str()) =>
                    {
                        send(
                            tools,
                            conversation,
                            "Please enter the verification code.",
                            ctx,
                        )
                        .await?;
                    }
                    _ if !is_setup_conversation => {}
                    "retry" => {
                        self.retry(session, tools, conversation, ctx).await?;
                    }
                    "continue" if activated => {
                        self.finish(session, tools, automations, conversation, ctx, false)
                            .await?;
                    }
                    "continue" => {
                        request_verification_choice(
                            tools,
                            conversation,
                            "The channel has not started successfully yet.",
                            false,
                            ctx,
                        )
                        .await?;
                    }
                    "rollback" | "cancel" => {
                        self.rollback(session, tools, automations, conversation, ctx)
                            .await?;
                    }
                    _ if activated => {
                        request_verification_choice(
                            tools,
                            conversation,
                            &format!(
                                "I am waiting for a message from the {} channel containing the verification code I sent above.",
                                session.package.name
                            ),
                            true,
                            ctx,
                        )
                        .await?;
                    }
                    _ => {
                        request_verification_choice(
                            tools,
                            conversation,
                            "The channel is not ready yet.",
                            false,
                            ctx,
                        )
                        .await?;
                    }
                }
            }
        }

        Ok(serde_json::json!({ "ok": true }))
    }

    async fn prompt_config_or_finish(
        &self,
        mut session: InstallSession,
        layout: &RuntimeLayout,
        tools: &InternalTools,
        conversation: &ConversationKey,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let configs = required_config(&session.package);
        let Some(config) = configs.first() else {
            return self
                .complete(session, layout, tools, conversation, ctx)
                .await;
        };
        session.stage = InstallStage::Config { index: 0 };
        self.state.save(session);
        send(tools, conversation, config_prompt(config), ctx).await
    }

    async fn complete(
        &self,
        session: InstallSession,
        layout: &RuntimeLayout,
        tools: &InternalTools,
        conversation: &ConversationKey,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        write_package(layout, &session.package, &session.config_values)?;
        match self.activate_package(&session.package).await {
            Ok(()) => {
                let code = verification_code();
                let mut session = session;
                session.stage = InstallStage::Verify {
                    activated: true,
                    code: code.clone(),
                };
                self.state.save(session.clone());
                self.setup
                    .automations
                    .add_runtime(super::InstallChannelTool::automation(
                        &session.package.name,
                        SenderKind::Guest,
                        None,
                    ))?;
                send(
                    tools,
                    conversation,
                    &format!(
                        "The {} channel is installed and started. Send me this code from that channel to confirm it works: {}.",
                        session.package.name,
                        code
                    ),
                    ctx,
                )
                .await?;
                request_verification_choice(
                    tools,
                    conversation,
                    "Or choose how to continue setup.",
                    true,
                    ctx,
                )
                .await
            }
            Err(err) => {
                let mut session = session;
                session.stage = InstallStage::Verify {
                    activated: false,
                    code: verification_code(),
                };
                self.state.save(session);
                request_verification_choice(
                    tools,
                    conversation,
                    &format!("I installed the channel files, but the channel did not start: {err}"),
                    false,
                    ctx,
                )
                .await
            }
        }
    }

    async fn retry(
        &self,
        mut session: InstallSession,
        tools: &InternalTools,
        conversation: &ConversationKey,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        remove_guest_setup_automation(&self.setup.automations, &session.package.name)?;
        session.config_values.clear();
        if session.secrets.is_empty() {
            let configs = required_config(&session.package);
            if let Some(config) = configs.first() {
                session.stage = InstallStage::Config { index: 0 };
                self.state.save(session);
                return send(tools, conversation, config_prompt(config), ctx).await;
            }
            return self
                .complete(session, &self.setup.layout, tools, conversation, ctx)
                .await;
        }
        session.stage = InstallStage::Secret { index: 0 };
        self.state.save(session.clone());
        send(
            tools,
            conversation,
            &secret_prompt(&session.package, &session.secrets[0]),
            ctx,
        )
        .await
    }

    async fn rollback(
        &self,
        session: InstallSession,
        tools: &InternalTools,
        automations: &Arc<Automations>,
        conversation: &ConversationKey,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let continuation = session.continuation.clone();
        self.setup
            .channel_runtime()?
            .remove_channel(&session.package.name)
            .await;
        remove_installed_package(&self.setup.layout, &session.package);
        self.state.take();
        stop_setup_automations(automations, &session.package.name)?;
        send(
            tools,
            conversation,
            &format!(
                "I rolled back the {} channel setup. I will answer normally again.",
                session.package.name
            ),
            ctx,
        )
        .await?;
        publish_tool_result(
            continuation.as_ref(),
            Ok(serde_json::json!({ "channel": session.package.name, "rolled_back": true })),
        );
        Ok(())
    }

    async fn cancel(
        &self,
        session: InstallSession,
        tools: &InternalTools,
        automations: &Arc<Automations>,
        conversation: &ConversationKey,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let name = session.package.name.clone();
        let continuation = session.continuation.clone();
        self.state.take();
        stop_setup_automations(automations, &name)?;
        send(
            tools,
            conversation,
            "Channel installation cancelled. I will answer normally again.",
            ctx,
        )
        .await?;
        publish_tool_result(
            continuation.as_ref(),
            Ok(serde_json::json!({ "channel": name, "cancelled": true })),
        );
        Ok(())
    }

    async fn finish(
        &self,
        session: InstallSession,
        tools: &InternalTools,
        automations: &Arc<Automations>,
        conversation: &ConversationKey,
        ctx: &ExecutionContext,
        verified: bool,
    ) -> Result<(), String> {
        let name = session.package.name.clone();
        let continuation = session.continuation.clone();
        self.state.take();
        stop_setup_automations(automations, &name)?;
        send(
            tools,
            conversation,
            &format!("The {name} channel is installed, configured, and ready to use."),
            ctx,
        )
        .await?;
        publish_tool_result(
            continuation.as_ref(),
            Ok(serde_json::json!({
                "channel": name,
                "installed": true,
                "configured": true,
                "verification": if verified { "confirmed" } else { "skipped" },
                "status": "ready",
                "message": format!("The {name} channel is installed, configured, and ready to use.")
            })),
        );
        Ok(())
    }

    async fn activate_package(&self, package: &ChannelPackage) -> Result<(), String> {
        let runtime = self.setup.channel_runtime()?;
        runtime
            .activate_installed_channel(&package.name, package.manifest.external)
            .await
    }
}

async fn request_verification_choice(
    tools: &InternalTools,
    conversation: &ConversationKey,
    question: &str,
    activated: bool,
    ctx: &ExecutionContext,
) -> Result<(), String> {
    let options = if activated {
        &[
            ("Retry setup", "retry"),
            ("Continue", "continue"),
            ("Rollback", "rollback"),
        ][..]
    } else {
        &[("Retry setup", "retry"), ("Rollback", "rollback")][..]
    };
    request_choice(tools, conversation, question, options, ctx).await
}

fn stop_setup_automations(automations: &Arc<Automations>, name: &str) -> Result<(), String> {
    for sender in ["user", "guest"] {
        let automation = format!("channel_auth:{name}:{sender}");
        if automations.contains(&automation) {
            stop_setup_automation(automations, &automation)?;
        }
    }
    Ok(())
}

fn remove_guest_setup_automation(automations: &Arc<Automations>, name: &str) -> Result<(), String> {
    let automation = format!("channel_auth:{name}:guest");
    if automations.contains(&automation) {
        stop_setup_automation(automations, &automation)?;
    }
    Ok(())
}
