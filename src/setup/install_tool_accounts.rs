use crate::tools::ExecutionContext;

use super::{
    InstallSession, InstallStage, SetupMode, SetupToolTool,
    install_tool_artifacts::build_account,
    install_tool_package::{account_identity, scoped_secret},
    install_tool_prompts::collect_account_secrets,
    installable_tool::InstallableTool,
};

impl SetupToolTool {
    pub(super) async fn authorize_or_complete(
        &self,
        mut session: InstallSession,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        if let SetupMode::Configure { current_config } = &session.mode {
            for config in session.package.required_config() {
                if let Some(value) = session
                    .account_current
                    .as_ref()
                    .unwrap_or(current_config)
                    .pointer(&config.path)
                {
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
                if !super::install_tool_prompts::condition_matches(
                    config.when.as_ref(),
                    &session.config_values,
                    session.account_current.as_ref(),
                    &session.mode,
                ) {
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
        let Some(mut oauth) = oauth else {
            return if session.package.accounts().is_some() {
                self.finish_account(session, ctx).await
            } else {
                self.complete(session, ctx).await
            };
        };
        if session.package.accounts().is_some() {
            let account = account_identity(wasm_package(&session.package), &session.config_values)?;
            oauth.refresh_token_handle = scoped_secret(&account, &oauth.refresh_token_handle);
        }
        if session
            .secret_values
            .contains_key(&oauth.refresh_token_handle)
        {
            return if session.package.accounts().is_some() {
                self.finish_account(session, ctx).await
            } else {
                self.complete(session, ctx).await
            };
        }
        let client_secret = oauth
            .client_secret
            .clone()
            .map(Ok)
            .or_else(|| {
                oauth.client_secret_handle.as_ref().map(|name| {
                    let name = if session.package.accounts().is_some() {
                        scoped_secret(
                            &account_identity(
                                wasm_package(&session.package),
                                &session.config_values,
                            )?,
                            name,
                        )
                    } else {
                        name.clone()
                    };
                    session
                        .secret_values
                        .get(&name)
                        .cloned()
                        .or_else(|| match &session.mode {
                            SetupMode::Configure { .. } => self
                                .setup
                                .secrets
                                .get(&session.package.secret_store_name(&name)),
                            SetupMode::Install => None,
                        })
                        .ok_or_else(|| {
                            format!("OAuth client secret was not collected during setup: {name}")
                        })
                })
            })
            .transpose()?;
        let pending = crate::setup::package_support::start_oauth(
            &oauth,
            &session.config_values,
            client_secret,
        )
        .await?;
        let prompt = pending.prompt();
        session.stage = InstallStage::Autoconfigure(pending);
        self.state.save(session.clone());
        session.send(&self.setup.tools, &prompt, ctx).await
    }

    pub(super) async fn start_account(
        &self,
        mut session: InstallSession,
        current: Option<serde_json::Value>,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        session.config_values.clear();
        session.secret_values.clear();
        session.account_current = current;
        self.prompt_config_or_finish(session, 0, ctx).await
    }

    pub(super) async fn finish_account(
        &self,
        mut session: InstallSession,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let package = wasm_package(&session.package);
        let (identity, account, grants) = build_account(package, &session.config_values)?;
        if let Some(previous) = session
            .account_current
            .as_ref()
            .and_then(|config| config.pointer(&package.manifest.accounts.as_ref()?.identity_path))
            .and_then(serde_json::Value::as_str)
            .filter(|previous| *previous != identity)
            .map(str::to_string)
        {
            if let Some(config) = session.accounts.remove(&previous) {
                collect_account_secrets(&session.package, &config, &mut session.removed_secrets);
            }
            session.grants.retain(|grant| {
                if grant.account.as_deref() != Some(&previous) {
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
        session
            .grants
            .retain(|grant| grant.account.as_deref() != Some(&identity));
        session.grants.extend(grants);
        for secret in session.package.required_secrets() {
            if let Some(value) = session.secret_values.remove(&secret.name) {
                session
                    .secret_values
                    .insert(scoped_secret(&identity, &secret.name), value);
            }
        }
        session.accounts.insert(identity, account);
        session.account_current = None;
        if matches!(session.mode, SetupMode::Install) {
            session.stage = InstallStage::AddAnother;
            self.state.save(session.clone());
            session
                .request_choice(
                    &self.setup.tools,
                    "Add another account?",
                    &[("Add another", "yes"), ("Finish", "no")],
                    ctx,
                )
                .await
        } else {
            self.prompt_account_menu(session, ctx).await
        }
    }

    pub(super) async fn prompt_account_menu(
        &self,
        mut session: InstallSession,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        session.stage = InstallStage::AccountMenu;
        self.state.save(session.clone());
        session
            .request_choice(
                &self.setup.tools,
                "Manage accounts",
                &[
                    ("Add", "add"),
                    ("Reconfigure", "reconfigure"),
                    ("Remove", "remove"),
                    ("Finish", "finish"),
                ],
                ctx,
            )
            .await
    }

    pub(super) async fn prompt_account_choice(
        &self,
        mut session: InstallSession,
        remove: bool,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        if session.accounts.is_empty() {
            return self.prompt_account_menu(session, ctx).await;
        }
        session.stage = InstallStage::AccountAction { remove };
        let names = session.accounts.keys().cloned().collect::<Vec<_>>();
        self.state.save(session.clone());
        let choices = names
            .iter()
            .map(|name| (name.as_str(), name.as_str()))
            .collect::<Vec<_>>();
        session
            .request_choice(&self.setup.tools, "Which account?", &choices, ctx)
            .await
    }
}

fn wasm_package(package: &InstallableTool) -> &super::install_tool_package::ToolPackage {
    match package {
        InstallableTool::Wasm(package) => package,
        InstallableTool::Shell(_) => unreachable!("shell tools cannot have accounts"),
    }
}
