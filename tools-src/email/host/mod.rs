mod discovery;

use std::collections::BTreeMap;

use crate::oauth::{self, OauthFlow, ProviderSettings};

#[derive(Clone, Copy)]
pub(crate) enum Provider {
    Google,
    Microsoft,
}

pub(crate) enum SetupOutcome {
    Complete(BTreeMap<String, serde_json::Value>),
    Pending(PendingSetup),
}

#[derive(Clone)]
pub(crate) struct PendingSetup {
    flow: OauthFlow,
}

impl PendingSetup {
    pub(crate) fn prompt(&self) -> String {
        format!(
            "[Connect your email account]({}). The WebUI will finish setup automatically; in other clients, reply done after authorization.",
            self.flow.authorization_url
        )
    }

    pub(crate) async fn finish(&self) -> Result<Option<(String, String)>, String> {
        Ok(oauth::finish(&self.flow)
            .await?
            .map(|token| (self.flow.refresh_token_secret().to_string(), token)))
    }

    pub(crate) fn waiting_prompt(&self) -> &'static str {
        "Authorization has not reached Ironpaw yet. Complete it in the browser, then reply done again."
    }
}

pub(crate) fn is_email_address(value: &str) -> bool {
    discovery::is_email_address(value)
}

pub(crate) async fn run_setup_action(
    action: &str,
    values: &BTreeMap<String, serde_json::Value>,
    value: &serde_json::Value,
) -> Result<SetupOutcome, String> {
    match action {
        "discover" => {
            let discovered = match value.as_str() {
                Some(address) => discovery::discover(address).await.unwrap_or_default(),
                None => BTreeMap::new(),
            };
            Ok(SetupOutcome::Complete(discovered))
        }
        "oauth" => {
            let address = string_value(values, "/email/username", "email address")?;
            let access = value
                .as_str()
                .ok_or_else(|| "email access mode must be a string".to_string())?;
            let provider = match string_value(values, "/email/provider", "email provider")? {
                "gmail" => Provider::Google,
                "outlook" => Provider::Microsoft,
                value => return Err(format!("unsupported OAuth email provider: {value}")),
            };
            let flow = oauth::start(oauth_settings(provider, access)?, address).await?;
            Ok(SetupOutcome::Pending(PendingSetup { flow }))
        }
        _ => Err(format!("unknown email setup action: {action}")),
    }
}

pub(crate) fn oauth_settings(provider: Provider, access: &str) -> Result<ProviderSettings, String> {
    let (authorize_url, token_url, client_id, redirect_host, refresh_token_secret, scopes) =
        match (provider, access) {
            (Provider::Google, "readonly") => (
                "https://accounts.google.com/o/oauth2/v2/auth",
                "https://oauth2.googleapis.com/token",
                option_env!("IRONPAW_GOOGLE_CLIENT_ID"),
                "127.0.0.1",
                "EMAIL_GOOGLE_REFRESH_TOKEN",
                vec!["https://www.googleapis.com/auth/gmail.readonly"],
            ),
            (Provider::Google, "control") => (
                "https://accounts.google.com/o/oauth2/v2/auth",
                "https://oauth2.googleapis.com/token",
                option_env!("IRONPAW_GOOGLE_CLIENT_ID"),
                "127.0.0.1",
                "EMAIL_GOOGLE_REFRESH_TOKEN",
                vec![
                    "https://www.googleapis.com/auth/gmail.modify",
                    "https://www.googleapis.com/auth/gmail.send",
                ],
            ),
            (Provider::Microsoft, "readonly") => (
                "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize",
                "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
                option_env!("IRONPAW_MICROSOFT_CLIENT_ID"),
                "localhost",
                "EMAIL_MICROSOFT_REFRESH_TOKEN",
                vec!["offline_access", "https://graph.microsoft.com/Mail.Read"],
            ),
            (Provider::Microsoft, "control") => (
                "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize",
                "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
                option_env!("IRONPAW_MICROSOFT_CLIENT_ID"),
                "localhost",
                "EMAIL_MICROSOFT_REFRESH_TOKEN",
                vec![
                    "offline_access",
                    "https://graph.microsoft.com/Mail.ReadWrite",
                    "https://graph.microsoft.com/Mail.Send",
                ],
            ),
            (_, _) => return Err("unsupported email access mode".to_string()),
        };
    let missing_name = match provider {
        Provider::Google => "GOOGLE_CLIENT_ID",
        Provider::Microsoft => "MICROSOFT_CLIENT_ID",
    };
    let client_id = client_id.ok_or_else(|| {
        format!(
            "this Ironpaw binary was built without {missing_name}; add it to ironpaw/.env and rebuild"
        )
    })?;
    let client_secret = match provider {
        Provider::Google => Some(option_env!("IRONPAW_GOOGLE_CLIENT_SECRET").ok_or_else(|| {
            "this Ironpaw binary was built without GOOGLE_CLIENT_SECRET; add it to ironpaw/.env and rebuild"
                .to_string()
        })?),
        Provider::Microsoft => None,
    };
    Ok(ProviderSettings {
        authorize_url,
        token_url,
        client_id,
        client_secret,
        redirect_host,
        refresh_token_secret,
        scopes,
        extra_authorize_params: match provider {
            Provider::Google => vec![("access_type", "offline"), ("prompt", "consent")],
            Provider::Microsoft => Vec::new(),
        },
    })
}

fn string_value<'a>(
    values: &'a BTreeMap<String, serde_json::Value>,
    path: &str,
    label: &str,
) -> Result<&'a str, String> {
    values
        .get(path)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{label} is missing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_use_least_privilege_read_scopes() {
        if let Ok(settings) = oauth_settings(Provider::Google, "readonly") {
            assert_eq!(
                settings.scopes,
                ["https://www.googleapis.com/auth/gmail.readonly"]
            );
        }
        if let Ok(settings) = oauth_settings(Provider::Microsoft, "readonly") {
            assert_eq!(
                settings.scopes,
                ["offline_access", "https://graph.microsoft.com/Mail.Read"]
            );
        }
    }
}
