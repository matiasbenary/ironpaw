use async_openai::types::chat::{
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
    CreateChatCompletionRequestArgs, ResponseFormat,
};

use crate::{
    openai::{EasyInputContent, EasyInputMessage, InputItem, Role},
    tools::{InternalTools, ai_inference::AiInferenceTool, send_message::SendMessageParams},
};

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub(super) struct ExecutionPlan {
    #[serde(default)]
    user_goal: String,
    #[serde(default)]
    requirements: Vec<Requirement>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct Requirement {
    id: String,
    action: String,
    #[serde(default)]
    object: RequirementObject,
    #[serde(default)]
    object_ref: Option<String>,
    #[serde(default)]
    target: Option<RequirementTarget>,
    #[serde(default)]
    depends_on: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct RequirementObject {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    ref_id: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct RequirementTarget {
    kind: String,
    name: String,
}

#[derive(Clone, Debug)]
pub(super) struct GeneratedFile {
    requirement_id: String,
    kind: String,
    file_id: u32,
}

pub(super) async fn construct_execution_plan(
    ai_inference: &AiInferenceTool,
    channel: &str,
    messages: &[InputItem],
) -> Result<ExecutionPlan, String> {
    let user_text = extract_text_from_input_items(messages);
    if user_text.trim().is_empty() {
        return Ok(ExecutionPlan::default());
    }

    let request = CreateChatCompletionRequestArgs::default()
        .messages([
            ChatCompletionRequestSystemMessageArgs::default()
                .content(format!(
                    "Extract compact operational requirements from the user's request. \
                    Return only valid JSON with this shape: \
                    {{\"user_goal\":\"...\",\"requirements\":[{{\"id\":\"r1\",\"action\":\"create|deliver|reply|fetch|transform\",\"object\":{{\"kind\":\"image|text|file|generated_file|unknown\",\"description\":\"...\",\"ref_id\":\"...\"}},\"object_ref\":\"r1\",\"target\":{{\"kind\":\"channel|contact|current_channel\",\"name\":\"...\"}},\"depends_on\":\"r1\"}}]}}. \
                    Use deliver for requests to send/share/post something to a destination. \
                    Use target.kind=\"channel\" for app channels such as telegram or tui. \
                    Current channel is '{channel}'. Omit optional fields when absent."
                ))
                .build()
                .map_err(|err| err.to_string())?
                .into(),
            ChatCompletionRequestUserMessageArgs::default()
                .content(user_text)
                .build()
                .map_err(|err| err.to_string())?
                .into(),
        ])
        .response_format(ResponseFormat::JsonObject)
        .build()
        .map_err(|err| err.to_string())?;

    let response = ai_inference.execute(request).await?;
    let content = response
        .choices
        .first()
        .and_then(|choice| choice.message.content.as_deref())
        .ok_or_else(|| "plan response missing content".to_string())?;
    let mut plan: ExecutionPlan = serde_json::from_str(content).map_err(|err| err.to_string())?;
    normalize_execution_plan(&mut plan);
    Ok(plan)
}

fn normalize_execution_plan(plan: &mut ExecutionPlan) {
    plan.requirements.retain(|requirement| {
        !requirement.id.trim().is_empty() && !requirement.action.trim().is_empty()
    });
    for requirement in &mut plan.requirements {
        requirement.action = requirement.action.trim().to_ascii_lowercase();
        requirement.object.kind = requirement.object.kind.trim().to_ascii_lowercase();
        if let Some(target) = &mut requirement.target {
            target.kind = target.kind.trim().to_ascii_lowercase();
            target.name = target.name.trim().to_ascii_lowercase();
        }
    }
}

pub(super) fn record_generated_file(
    generated_files: &mut Vec<GeneratedFile>,
    plan: &ExecutionPlan,
    tool_name: &str,
    result: &serde_json::Value,
) {
    if tool_name != "image_generation" {
        return;
    }
    let Some(file_id) = result.get("file_id").and_then(|value| value.as_u64()) else {
        return;
    };
    let requirement_id = plan
        .requirements
        .iter()
        .find(|requirement| requirement.action == "create" && requirement.object.kind == "image")
        .map(|requirement| requirement.id.clone())
        .unwrap_or_else(|| "generated_image".to_string());

    generated_files.push(GeneratedFile {
        requirement_id,
        kind: "image".to_string(),
        file_id: file_id as u32,
    });
}

pub(super) async fn fulfill_pending_deliveries(
    internal_tools: &InternalTools,
    current_channel: &str,
    plan: &ExecutionPlan,
    generated_files: &[GeneratedFile],
    fulfilled_requirements: &mut std::collections::HashSet<String>,
) -> Result<Vec<InputItem>, String> {
    let mut fulfilled = Vec::new();
    for requirement in &plan.requirements {
        if fulfilled_requirements.contains(&requirement.id) {
            continue;
        }
        let Some(target) = &requirement.target else {
            continue;
        };
        if target.kind != "channel" || target.name == current_channel {
            continue;
        }
        let generated_file = if requirement.action == "deliver" {
            generated_file_for_requirement(requirement, generated_files)
        } else if requirement.action == "create" {
            generated_files
                .iter()
                .find(|file| file.requirement_id == requirement.id)
        } else {
            None
        };
        let Some(generated_file) = generated_file else {
            continue;
        };
        internal_tools
            .send_message
            .execute(SendMessageParams {
                channel: target.name.clone(),
                content: match generated_file.kind.as_str() {
                    "image" => "Here is the generated image.".to_string(),
                    _ => "Here is the requested result.".to_string(),
                },
                file_id: Some(generated_file.file_id),
                image_url: None,
                image_base64: None,
                image_mime_type: None,
                image_filename: None,
                conversation_id: None,
                thread_id: None,
            })
            .await?;
        fulfilled_requirements.insert(requirement.id.clone());
        fulfilled.push(requirement_fulfilled_input_item(requirement.id.clone()));
    }
    Ok(fulfilled)
}

fn generated_file_for_requirement<'a>(
    requirement: &Requirement,
    generated_files: &'a [GeneratedFile],
) -> Option<&'a GeneratedFile> {
    let refs = [
        requirement.object_ref.as_deref(),
        requirement.depends_on.as_deref(),
        requirement.object.ref_id.as_deref(),
    ];
    generated_files.iter().find(|file| {
        refs.iter()
            .flatten()
            .any(|ref_id| *ref_id == file.requirement_id)
    })
}

fn extract_text_from_input_items(items: &[InputItem]) -> String {
    items
        .iter()
        .filter_map(|item| serde_json::to_value(item).ok())
        .flat_map(|value| collect_text_values_for_keys(&value, &["content", "text"]))
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect_text_values_for_keys(value: &serde_json::Value, keys: &[&str]) -> Vec<String> {
    match value {
        serde_json::Value::Object(object) => object
            .iter()
            .flat_map(|(key, value)| {
                if keys.contains(&key.as_str()) {
                    match value {
                        serde_json::Value::String(text) => vec![text.clone()],
                        _ => collect_text_values_for_keys(value, keys),
                    }
                } else {
                    collect_text_values_for_keys(value, keys)
                }
            })
            .collect(),
        serde_json::Value::Array(values) => values
            .iter()
            .flat_map(|value| collect_text_values_for_keys(value, keys))
            .collect(),
        _ => Vec::new(),
    }
}

fn requirement_fulfilled_input_item(requirement_id: String) -> InputItem {
    EasyInputMessage {
        r#type: Default::default(),
        role: Role::System,
        content: EasyInputContent::Text(format!(
            "Internal obligation fulfilled: requirement '{requirement_id}'."
        )),
        phase: None,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;
    use crate::{
        bus::{Bus, Event},
        tools::send_message::SendMessageTool,
    };

    #[tokio::test]
    async fn delivery_requirement_sends_generated_file() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let files_dir = test_files_dir("delivery");
        std::fs::create_dir_all(&files_dir).unwrap();
        std::fs::write(files_dir.join("42.png"), b"stub-png").unwrap();

        let internal_tools = InternalTools::new(
            AiInferenceTool::new("test-key".to_string()).unwrap(),
            SendMessageTool::new(bus.clone(), files_dir),
            "test-key".to_string(),
            std::env::temp_dir(),
        );
        let plan = ExecutionPlan {
            user_goal: "Create an image of a dog and send it to Telegram.".to_string(),
            requirements: vec![
                Requirement {
                    id: "r1".to_string(),
                    action: "create".to_string(),
                    object: RequirementObject {
                        kind: "image".to_string(),
                        description: Some("a dog".to_string()),
                        ref_id: None,
                    },
                    object_ref: None,
                    target: None,
                    depends_on: None,
                },
                Requirement {
                    id: "r2".to_string(),
                    action: "deliver".to_string(),
                    object: RequirementObject {
                        kind: "generated_file".to_string(),
                        description: None,
                        ref_id: None,
                    },
                    object_ref: Some("r1".to_string()),
                    target: Some(RequirementTarget {
                        kind: "channel".to_string(),
                        name: "telegram".to_string(),
                    }),
                    depends_on: Some("r1".to_string()),
                },
            ],
        };
        let mut generated_files = Vec::new();
        let mut fulfilled_requirements = std::collections::HashSet::new();

        record_generated_file(
            &mut generated_files,
            &plan,
            "image_generation",
            &serde_json::json!({ "file_id": 42 }),
        );
        let fulfilled = fulfill_pending_deliveries(
            &internal_tools,
            "tui",
            &plan,
            &generated_files,
            &mut fulfilled_requirements,
        )
        .await
        .unwrap();

        assert_eq!(fulfilled.len(), 1);
        assert_telegram_attachment(events.recv()).await;
    }

    #[tokio::test]
    async fn create_requirement_target_sends_generated_file() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let files_dir = test_files_dir("create-target");
        std::fs::create_dir_all(&files_dir).unwrap();
        std::fs::write(files_dir.join("43.png"), b"stub-png").unwrap();

        let internal_tools = InternalTools::new(
            AiInferenceTool::new("test-key".to_string()).unwrap(),
            SendMessageTool::new(bus.clone(), files_dir),
            "test-key".to_string(),
            std::env::temp_dir(),
        );
        let plan = ExecutionPlan {
            user_goal: "Create and send an image of a dog to Telegram.".to_string(),
            requirements: vec![Requirement {
                id: "r1".to_string(),
                action: "create".to_string(),
                object: RequirementObject {
                    kind: "image".to_string(),
                    description: Some("a dog".to_string()),
                    ref_id: Some("r1".to_string()),
                },
                object_ref: Some("r1".to_string()),
                target: Some(RequirementTarget {
                    kind: "channel".to_string(),
                    name: "telegram".to_string(),
                }),
                depends_on: None,
            }],
        };
        let mut generated_files = Vec::new();
        let mut fulfilled_requirements = std::collections::HashSet::new();

        record_generated_file(
            &mut generated_files,
            &plan,
            "image_generation",
            &serde_json::json!({ "file_id": 43 }),
        );
        let fulfilled = fulfill_pending_deliveries(
            &internal_tools,
            "tui",
            &plan,
            &generated_files,
            &mut fulfilled_requirements,
        )
        .await
        .unwrap();

        assert_eq!(fulfilled.len(), 1);
        assert_telegram_attachment(events.recv()).await;
    }

    fn test_files_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "warden-ai-engine-orchestrator-{name}-{}",
            std::process::id()
        ))
    }

    async fn assert_telegram_attachment(
        event: impl std::future::Future<
            Output = Result<Event, tokio::sync::broadcast::error::RecvError>,
        >,
    ) {
        let event = timeout(Duration::from_secs(1), event)
            .await
            .expect("timed out waiting for bus event")
            .expect("bus closed");
        let Event::MessageOut {
            channel, message, ..
        } = event
        else {
            panic!("expected MessageOut");
        };
        assert_eq!(channel, "telegram");
        assert_eq!(message.attachments.len(), 1);
    }
}
