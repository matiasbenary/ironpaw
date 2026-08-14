use crate::tools::{ExecutionContext, InvocationScope};

#[derive(serde::Serialize)]
struct ScheduledInvocation<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    automation: &'a str,
}

pub(super) fn from_timer(
    kind: &'static str,
    automation_name: &str,
) -> (ExecutionContext, serde_json::Value) {
    (
        ExecutionContext::new(InvocationScope::ScheduledAutomation {
            automation_id: automation_name.to_string(),
        }),
        serde_json::to_value(ScheduledInvocation {
            kind,
            automation: automation_name,
        })
        .expect("serialize scheduled invocation"),
    )
}
