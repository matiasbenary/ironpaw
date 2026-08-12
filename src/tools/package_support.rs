use std::collections::BTreeMap;

#[derive(Clone)]
pub(crate) struct PendingSetup {
    inner: crate::tools::email_host::PendingSetup,
}

pub(crate) enum SetupOutcome {
    Complete(BTreeMap<String, serde_json::Value>),
    Pending(PendingSetup),
}

impl PendingSetup {
    pub(crate) fn prompt(&self) -> String {
        self.inner.prompt()
    }

    pub(crate) async fn finish(&self) -> Result<Option<(String, String)>, String> {
        self.inner.finish().await
    }

    pub(crate) fn waiting_prompt(&self) -> &'static str {
        self.inner.waiting_prompt()
    }
}

pub(crate) async fn run_setup_action(
    handler: &str,
    action: &str,
    values: &BTreeMap<String, serde_json::Value>,
    value: &serde_json::Value,
) -> Result<SetupOutcome, String> {
    match handler {
        "email" => match crate::tools::email_host::run_setup_action(action, values, value).await? {
            crate::tools::email_host::SetupOutcome::Complete(values) => {
                Ok(SetupOutcome::Complete(values))
            }
            crate::tools::email_host::SetupOutcome::Pending(inner) => {
                Ok(SetupOutcome::Pending(PendingSetup { inner }))
            }
        },
        _ => Err(format!("unknown package setup handler: {handler}")),
    }
}
