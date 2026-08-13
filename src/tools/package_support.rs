use std::collections::BTreeMap;

pub(crate) use crate::tools::email_host::{PendingSetup, SetupOutcome};

pub(crate) async fn run_setup_action(
    handler: &str,
    action: &str,
    values: &BTreeMap<String, serde_json::Value>,
    value: &serde_json::Value,
) -> Result<SetupOutcome, String> {
    match handler {
        "email" => crate::tools::email_host::run_setup_action(action, values, value).await,
        _ => Err(format!("unknown package setup handler: {handler}")),
    }
}
