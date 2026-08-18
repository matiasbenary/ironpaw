use std::collections::BTreeMap;

use crate::{
    network::oauth::{self, OauthFlow, ProviderSettings},
    setup::common::SetupOAuth,
};

#[derive(Clone)]
pub(crate) struct PendingSetup {
    flow: OauthFlow,
}

impl PendingSetup {
    pub(crate) fn prompt(&self) -> String {
        format!(
            "[Connect your account]({}). The WebUI will finish setup automatically; in other clients, reply done after authorization.",
            self.flow.authorization_url
        )
    }

    pub(crate) async fn finish(&self) -> Result<Option<(String, String)>, String> {
        Ok(oauth::finish(&self.flow)
            .await?
            .map(|token| (self.flow.refresh_token_handle().to_string(), token)))
    }

    pub(crate) fn waiting_prompt(&self) -> &'static str {
        "Authorization has not reached Ironpaw yet. Complete it in the browser, then reply done again."
    }
}

pub(crate) async fn start_oauth(
    config: &SetupOAuth,
    values: &BTreeMap<String, serde_json::Value>,
    client_secret: Option<String>,
) -> Result<PendingSetup, String> {
    let login_hint = values
        .get(&config.login_hint_path)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("OAuth login hint is missing: {}", config.login_hint_path))?;
    let flow = oauth::start(
        ProviderSettings {
            authorize_url: config.authorize_url.clone(),
            token_url: config.token_url.clone(),
            client_id: config.client_id.resolve(values)?,
            client_secret,
            redirect_host: config.redirect_host.clone(),
            refresh_token_handle: config.refresh_token_handle.clone(),
            scopes: config.scopes.clone(),
            extra_authorize_params: config
                .extra_authorize_params
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        },
        login_hint,
    )
    .await?;
    Ok(PendingSetup { flow })
}

pub(crate) async fn run_setup_action(
    handler: &str,
    action: &str,
    value: &serde_json::Value,
) -> Result<BTreeMap<String, serde_json::Value>, String> {
    match handler {
        "imap_discovery" => crate::network::email::run_setup_action(action, value).await,
        _ => Err(format!("unknown package setup handler: {handler}")),
    }
}
