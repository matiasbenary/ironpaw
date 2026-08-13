mod discovery;
pub(crate) mod message;
pub(crate) mod protocol;

use std::collections::BTreeMap;

pub(crate) fn is_email_address(value: &str) -> bool {
    discovery::is_email_address(value)
}

pub(crate) async fn run_setup_action(
    action: &str,
    value: &serde_json::Value,
) -> Result<BTreeMap<String, serde_json::Value>, String> {
    match action {
        "discover" => {
            let address = value
                .as_str()
                .ok_or_else(|| "email discovery requires an address".to_string())?;
            match discovery::discover(address).await {
                Ok(discovered) => Ok(discovered),
                Err(_) => Ok(BTreeMap::from([
                    ("/email/provider".to_string(), "imap".into()),
                    ("/email/username".to_string(), address.trim().into()),
                    ("/email/from".to_string(), address.trim().into()),
                ])),
            }
        }
        _ => Err(format!("unknown email setup action: {action}")),
    }
}
