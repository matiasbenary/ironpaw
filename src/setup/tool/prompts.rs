use std::collections::{BTreeMap, BTreeSet};

use crate::{secrets::Secrets, tools::ExecutionContext};

use crate::setup::common::{SetupCondition, SetupConfig, SetupSecret, config_prompt};

use super::{InstallSession, SetupMode, SetupToolTool, installable::InstallableTool};

impl SetupToolTool {
    pub(super) async fn apply_config_value(
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
            let values = crate::setup::autoconfigure::run_setup_action(
                handler,
                &setup_action.action,
                &value,
            )
            .await?;
            session.config_values.extend(values);
        }
        self.prompt_config_or_finish(session, index + 1, ctx).await
    }

    pub(super) async fn prompt_secret_or_finish(
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
                condition_matches(
                    secret.when.as_ref(),
                    &session.config_values,
                    session.account_current.as_ref(),
                    &session.mode,
                )
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
            session.stage = super::InstallStage::ConfirmSecret { index };
            self.state.save(session.clone());
            return session
                .request_choice(
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
        session.stage = super::InstallStage::Secret { index };
        self.state.save(session.clone());
        session.send(&self.setup.tools, &prompt, ctx).await
    }

    pub(super) async fn prompt_config_or_finish(
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
                condition_matches(
                    config.when.as_ref(),
                    &session.config_values,
                    session.account_current.as_ref(),
                    &session.mode,
                ) && !session.config_values.contains_key(&config.path)
            })
        else {
            return if session.package.config_before_secrets() {
                self.prompt_secret_or_finish(session, 0, ctx).await
            } else {
                self.authorize_or_complete(session, ctx).await
            };
        };
        if let SetupMode::Configure { current_config } = &session.mode
            && let Some(value) = saved_config_value(
                session.account_current.as_ref().unwrap_or(current_config),
                &config.path,
            )
        {
            let prompt = format!(
                "{} currently has value {}. Do you want to change it?",
                config_prompt(&config),
                display_config_value(value)
            );
            session.stage = super::InstallStage::ConfirmConfig { index };
            self.state.save(session.clone());
            return session
                .request_choice(
                    &self.setup.tools,
                    &prompt,
                    &[("Change", "yes"), ("Keep", "no")],
                    ctx,
                )
                .await;
        }
        self.prompt_config_value(session, index, ctx).await
    }

    pub(super) async fn prompt_config_value(
        &self,
        mut session: InstallSession,
        index: usize,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let config = session.package.required_config()[index].clone();
        session.stage = super::InstallStage::Config { index };
        self.state.save(session.clone());
        if config.choices.is_empty() {
            session
                .send(&self.setup.tools, config_prompt(&config), ctx)
                .await
        } else {
            let choices = config
                .choices
                .iter()
                .map(|choice| (choice.label.as_str(), choice.value.as_str()))
                .collect::<Vec<_>>();
            session
                .request_choice(&self.setup.tools, config_prompt(&config), &choices, ctx)
                .await
        }
    }
}

pub(super) fn condition_matches(
    condition: Option<&SetupCondition>,
    values: &BTreeMap<String, serde_json::Value>,
    current: Option<&serde_json::Value>,
    mode: &SetupMode,
) -> bool {
    let Some(condition) = condition else {
        return true;
    };
    values
        .get(&condition.path)
        .or_else(|| current.and_then(|config| config.pointer(&condition.path)))
        .or_else(|| match mode {
            SetupMode::Configure { current_config } => current_config.pointer(&condition.path),
            SetupMode::Install => None,
        })
        == Some(&condition.equals)
}

pub(super) fn collect_account_secrets(
    package: &InstallableTool,
    config: &serde_json::Value,
    output: &mut BTreeSet<String>,
) {
    for secret in package.required_secrets() {
        if let Some(handle) = secret.path.as_deref().and_then(|path| config.pointer(path))
            && let Some(handle) = handle.as_str()
        {
            output.insert(handle.to_string());
        }
    }
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

pub(super) fn rerun_autoconfigure_on_keep(config: &SetupConfig) -> bool {
    config.choices.iter().any(|choice| choice.oauth.is_some())
        || config
            .autoconfigure
            .as_ref()
            .is_some_and(|setup| setup.rerun_on_keep)
}

pub(super) fn save_secrets(secrets: &Secrets, session: &InstallSession) -> Result<(), String> {
    for (name, value) in &session.secret_values {
        secrets.set(&session.package.secret_store_name(name), value)?;
    }
    Ok(())
}

pub(super) fn secret_prompt(package: &InstallableTool, secret: &SetupSecret) -> String {
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
