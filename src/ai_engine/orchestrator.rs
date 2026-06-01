use async_openai::types::responses::{ResponseTextParam, TextResponseFormatConfiguration};

use crate::{
    message::OutboundAttachment,
    models,
    openai::{
        CreateResponse, EasyInputContent, EasyInputMessage, InputItem, InputParam, Role,
        response_text,
    },
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

pub(super) async fn construct_execution_plan(
    ai_inference: &AiInferenceTool,
    channel: &str,
    messages: &[InputItem],
) -> Result<ExecutionPlan, String> {
    let user_text = extract_text_from_input_items(messages);
    if user_text.trim().is_empty() {
        return Ok(ExecutionPlan::default());
    }

    let request = CreateResponse {
        model: Some(models::text_model()),
        input: InputParam::Items(vec![
            EasyInputMessage {
                r#type: Default::default(),
                role: Role::System,
                content: EasyInputContent::Text(format!(
                    "Extract compact operational requirements from the user's request. \
                    Return only valid JSON with this shape: \
                    {{\"user_goal\":\"...\",\"requirements\":[{{\"id\":\"r1\",\"action\":\"create|deliver|reply|fetch|transform\",\"object\":{{\"kind\":\"image|text|file|generated_file|unknown\",\"description\":\"...\",\"ref_id\":\"...\"}},\"object_ref\":\"r1\",\"target\":{{\"kind\":\"channel|contact|current_channel\",\"name\":\"...\"}},\"depends_on\":\"r1\"}}]}}. \
                    Use deliver for requests to send/share/post something to a destination. \
                    Use target.kind=\"channel\" for app channels such as telegram or tui. \
                    Current channel is '{channel}'. Omit optional fields when absent."
                )),
                phase: None,
            }
            .into(),
            EasyInputMessage {
                r#type: Default::default(),
                role: Role::User,
                content: EasyInputContent::Text(user_text),
                phase: None,
            }
            .into(),
        ]),
        text: Some(ResponseTextParam {
            format: TextResponseFormatConfiguration::JsonObject,
            verbosity: None,
        }),
        parallel_tool_calls: Some(false),
        ..Default::default()
    };

    let content = response_text(ai_inference.execute(request).await?);
    if content.trim().is_empty() {
        return Err("plan response missing content".to_string());
    }
    let mut plan: ExecutionPlan = serde_json::from_str(&content).map_err(|err| err.to_string())?;
    normalize_execution_plan(&mut plan);
    Ok(plan)
}

#[derive(Clone, Debug)]
pub(super) struct GeneratedFile {
    pub(super) requirement_id: String,
    pub(super) kind: String,
    pub(super) file_id: u32,
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
    let Some(file_id) = result.get("file_id").and_then(|v| v.as_u64()) else {
        return;
    };
    let requirement_id = plan
        .requirements
        .iter()
        .find(|r| r.action == "create" && r.object.kind == "image")
        .map(|r| r.id.clone())
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
    response_text: Option<&str>,
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
        if let Some(generated_file) = generated_file {
            internal_tools
                .send_message
                .execute(SendMessageParams {
                    channel: target.name.clone(),
                    content: match generated_file.kind.as_str() {
                        "image" => "Here is the generated image.".to_string(),
                        _ => "Here is the requested result.".to_string(),
                    },
                    attachments: vec![OutboundAttachment {
                        kind: "image".to_string(),
                        mime_type: "image/png".to_string(),
                        filename: format!("{}.png", generated_file.file_id),
                        file_id: Some(generated_file.file_id),
                        data_base64: None,
                        url: None,
                    }],
                    conversation_id: None,
                    thread_id: None,
                })
                .await?;
        } else if matches!(requirement.action.as_str(), "deliver" | "reply")
            && matches!(
                requirement.object.kind.as_str(),
                "text" | "reply" | "unknown" | ""
            )
        {
            let Some(text) = response_text.filter(|t| !t.is_empty()) else {
                continue;
            };
            internal_tools
                .send_message
                .execute(SendMessageParams {
                    channel: target.name.clone(),
                    content: text.to_string(),
                    attachments: vec![],
                    conversation_id: None,
                    thread_id: None,
                })
                .await?;
        } else {
            continue;
        }
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
