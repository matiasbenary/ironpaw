use crate::tools::ExecutionContext;

use super::{
    InstallSession, InstallStage, SetupMode, SetupToolTool,
    artifacts::build_account,
    installable::InstallableTool,
    package::{account_identity, scoped_secret},
    prompts::collect_account_secrets,
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
                if !super::prompts::condition_matches(
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
        let client_secret = if oauth.broker.is_some() {
            None
        } else {
            oauth
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
                                format!(
                                    "OAuth client secret was not collected during setup: {name}"
                                )
                            })
                    })
                })
                .transpose()?
        };
        let pending =
            crate::setup::autoconfigure::start_oauth(&oauth, &session.config_values, client_secret)
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
        keep_collected_accounts(&mut session.secret_values);
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
        if let Some(previous) = &session.account_current {
            // Same wallet, different secret (e.g. it moved to another network).
            session.removed_secrets.extend(stale_secret_handles(
                &session.package,
                previous,
                &account,
            ));
        }
        session.accounts.insert(identity, account);
        session.account_current = None;
        if session.add_only {
            return self.complete(session, ctx).await;
        }
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

fn wasm_package(package: &InstallableTool) -> &super::package::ToolPackage {
    match package {
        InstallableTool::Wasm(package) => package,
        InstallableTool::Shell(_) => unreachable!("shell tools cannot have accounts"),
    }
}

/// Secret handles the previous version of an account used that the new one no longer does.
fn stale_secret_handles(
    package: &InstallableTool,
    previous: &serde_json::Value,
    current: &serde_json::Value,
) -> Vec<String> {
    let mut before = std::collections::BTreeSet::new();
    let mut after = std::collections::BTreeSet::new();
    collect_account_secrets(package, previous, &mut before);
    collect_account_secrets(package, current, &mut after);
    before.difference(&after).cloned().collect()
}

/// Starting another account drops the unscoped values just typed, but keeps scoped ones
/// (`NAME:ACCOUNT`): those belong to accounts already collected in this session.
fn keep_collected_accounts(secret_values: &mut std::collections::BTreeMap<String, String>) {
    secret_values.retain(|name, _| name.contains(':'));
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{keep_collected_accounts, stale_secret_handles};
    use crate::setup::tool::{installable::InstallableTool, package::ToolPackage};

    #[test]
    fn adding_another_account_keeps_earlier_wallets_keys() {
        let mut values = BTreeMap::from([
            ("OUTLAYER_API_KEY:PRINCIPAL".to_string(), "wk_a".to_string()),
            (
                "OUTLAYER_API_KEY".to_string(),
                "typed-for-the-next".to_string(),
            ),
        ]);

        keep_collected_accounts(&mut values);

        assert_eq!(
            values,
            BTreeMap::from([("OUTLAYER_API_KEY:PRINCIPAL".to_string(), "wk_a".to_string())])
        );
    }

    #[test]
    fn moving_a_wallet_to_another_network_retires_its_old_key() {
        let package = InstallableTool::Wasm(ToolPackage {
            name: "outlayer".to_string(),
            display_name: "OutLayer".to_string(),
            wasm_path: std::path::PathBuf::new(),
            config_template_path: std::path::PathBuf::new(),
            manifest: serde_json::from_str(include_str!(
                "../../../wasm-tools/outlayer/outlayer.install.json"
            ))
            .unwrap(),
        });
        let testnet = serde_json::json!({ "api_key_secret": "OUTLAYER_TESTNET_API_KEY:PRUEBA" });
        let mainnet = serde_json::json!({ "api_key_secret": "OUTLAYER_API_KEY:PRUEBA" });

        assert_eq!(
            stale_secret_handles(&package, &testnet, &mainnet),
            vec!["OUTLAYER_TESTNET_API_KEY:PRUEBA".to_string()]
        );
        assert!(stale_secret_handles(&package, &mainnet, &mainnet).is_empty());
    }
}
