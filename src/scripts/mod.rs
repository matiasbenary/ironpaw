use std::{collections::BTreeSet, sync::Arc, time::Duration};

use rquickjs::{CatchResultExt, Context, Ctx, Promise, Runtime, Value, function::Func};
use tree_sitter::{Node, Parser};

use crate::tools::{self, ExecutionContext, ToolRegistry, tool_result_json};

/// Wall-clock budget for one script execution: a runaway loop gets
/// interrupted instead of wedging the executor.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);
const SCRIPT_MEMORY_LIMIT: usize = 32 * 1024 * 1024;

/// Runs dynamic-tool scripts in a QuickJS sandbox. Scripts have no ambient
/// capabilities: the only way to act is by calling registered tools exposed
/// as global functions. The tool's arguments are exposed as the global `args`,
/// and the script returns the tool result with an explicit `return`.
pub struct ScriptRuntime {
    registry: Arc<ToolRegistry>,
}

impl ScriptRuntime {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }

    pub async fn run(
        &self,
        source: String,
        ctx: ExecutionContext,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let registry = Arc::clone(&self.registry);
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || execute_script(registry, handle, source, ctx, args))
            .await
            .map_err(|err| format!("script task failed: {err}"))?
    }

    pub fn validate_source(&self, source: &str) -> Result<(), String> {
        let tool_names = self
            .registry
            .all()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();
        validate_source(source, &tool_names)
    }
}

pub fn validate_source(source: &str, tools: &BTreeSet<String>) -> Result<(), String> {
    validate_tool_calls(source, tools)?;
    let runtime = Runtime::new().map_err(|err| err.to_string())?;
    runtime.set_memory_limit(SCRIPT_MEMORY_LIMIT);
    let context = Context::full(&runtime).map_err(|err| err.to_string())?;
    context.with(|js| -> Result<(), String> {
        let globals = js.globals();
        globals
            .set("__source", source)
            .map_err(|err| err.to_string())?;
        js.eval::<(), _>("Object.getPrototypeOf(async function () {}).constructor(__source);")
            .catch(&js)
            .map_err(|err| format!("script syntax error: {err}"))
    })
}

fn validate_tool_calls(source: &str, tools: &BTreeSet<String>) -> Result<(), String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|err| err.to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse script".to_string())?;
    if tree.root_node().has_error() {
        return Err("script syntax error".to_string());
    }

    let mut globals = tools.clone();
    globals.extend(
        [
            "Array",
            "BigInt",
            "Boolean",
            "Error",
            "Number",
            "Object",
            "Promise",
            "String",
            "Symbol",
            "TypeError",
            "parseFloat",
            "parseInt",
            "require",
        ]
        .map(str::to_string),
    );
    validate_node(tree.root_node(), source.as_bytes(), &globals)
}

fn validate_node(
    node: Node<'_>,
    source: &[u8],
    inherited: &BTreeSet<String>,
) -> Result<(), String> {
    let mut names = inherited.clone();
    collect_bindings(node, source, &mut names, true);

    if node.kind() == "call_expression"
        && let Some(function) = node.child_by_field_name("function")
        && function.kind() == "identifier"
    {
        let name = function.utf8_text(source).map_err(|err| err.to_string())?;
        if !names.contains(name) {
            return Err(format!(
                "script calls unavailable function `{name}`; use a registered tool or a locally declared function"
            ));
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        validate_node(child, source, &names)?;
    }
    Ok(())
}

fn is_scope(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "statement_block" | "function_expression" | "arrow_function" | "function_declaration"
    )
}

fn collect_bindings(node: Node<'_>, source: &[u8], names: &mut BTreeSet<String>, first: bool) {
    if !first && is_scope(node) {
        if node.kind() == "function_declaration"
            && let Some(name) = node.child_by_field_name("name")
            && let Ok(name) = name.utf8_text(source)
        {
            names.insert(name.to_string());
        }
        return;
    }
    match node.kind() {
        "variable_declarator" | "function_declaration" | "function_expression" => {
            if let Some(name) = node.child_by_field_name("name")
                && let Ok(name) = name.utf8_text(source)
            {
                names.insert(name.to_string());
            }
        }
        "formal_parameters" => {
            let mut cursor = node.walk();
            for parameter in node.named_children(&mut cursor) {
                if parameter.kind() == "identifier"
                    && let Ok(name) = parameter.utf8_text(source)
                {
                    names.insert(name.to_string());
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_bindings(child, source, names, false);
    }
}

fn execute_script(
    registry: Arc<ToolRegistry>,
    handle: tokio::runtime::Handle,
    source: String,
    ctx: ExecutionContext,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let runtime = Runtime::new().map_err(|err| err.to_string())?;
    runtime.set_memory_limit(SCRIPT_MEMORY_LIMIT);
    let deadline = std::time::Instant::now() + SCRIPT_TIMEOUT;
    runtime.set_interrupt_handler(Some(Box::new(move || std::time::Instant::now() > deadline)));

    let context = Context::full(&runtime).map_err(|err| err.to_string())?;
    context.with(|js| -> Result<serde_json::Value, String> {
        let globals = js.globals();

        let args_value = json_to_js(&js, &args)?;
        globals
            .set("args", args_value)
            .map_err(|err| err.to_string())?;

        let tool_names = registry
            .all()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();
        let tool_json = Func::from(move |name: String, args: String| -> String {
            let args: serde_json::Value =
                serde_json::from_str(&args).unwrap_or(serde_json::Value::Null);
            let result = handle.block_on(tools::run_registered_tool(&registry, &name, args, &ctx));
            serde_json::to_string(&tool_result_json(&result))
                .unwrap_or_else(|err| format!(r#"{{"status":"failed","error":"{err}"}}"#))
        });
        globals
            .set("__tool_json", tool_json)
            .map_err(|err| err.to_string())?;

        let log = Func::from(|message: String| eprintln!("script: {message}"));
        globals.set("__log", log).map_err(|err| err.to_string())?;

        let tool_names = serde_json::to_string(&tool_names).map_err(|err| err.to_string())?;
        globals
            .set("__tool_names", tool_names)
            .map_err(|err| err.to_string())?;

        const PRELUDE: &str = r#"
            for (const name of JSON.parse(__tool_names)) {
                globalThis[name] = (args) => {
                    const response = JSON.parse(__tool_json(name, JSON.stringify(args === undefined ? {} : args)));
                    if (response.status === "failed") {
                        throw new Error(response.error);
                    }
                    return response.result;
                };
            }
            globalThis.console = { log: (...a) => __log(a.map(String).join(" ")) };
            globalThis.fetch = () => {
                throw new Error("fetch is unavailable in scripts. For network requests, use web_fetch({ url }).");
            };
            globalThis.require = (module) => {
                switch (module) {
                    case "fs":
                    case "node:fs":
                        throw new Error("require is unavailable in scripts. For filesystem work, use list_files, read_file, create_file, or update_file.");
                    case "child_process":
                    case "node:child_process":
                        throw new Error("require is unavailable in scripts; scripts cannot start processes.");
                    case "http":
                    case "https":
                    case "node:http":
                    case "node:https":
                        throw new Error("require is unavailable in scripts. For network requests, use web_fetch or web_search.");
                    default:
                        throw new Error("require is unavailable in scripts.");
                }
            };
        "#;
        js.eval::<(), _>(PRELUDE)
            .catch(&js)
            .map_err(|err| format!("script prelude error: {err}"))?;

        globals
            .set("__source", source)
            .map_err(|err| err.to_string())?;
        let result: Promise = js
            .eval("Object.getPrototypeOf(async function () {}).constructor(__source)()")
            .catch(&js)
            .map_err(|err| format!("script error: {err}"))?;
        let result: Value = result
            .finish()
            .catch(&js)
            .map_err(|err| format!("script error: {err}"))?;
        js_result_to_json(&js, result)
    })
}

fn json_to_js<'js>(js: &Ctx<'js>, value: &serde_json::Value) -> Result<Value<'js>, String> {
    let text = serde_json::to_string(value).map_err(|err| err.to_string())?;
    js.json_parse(text).map_err(|err| err.to_string())
}

/// The script's returned value, as JSON. Undefined becomes null.
fn js_result_to_json<'js>(js: &Ctx<'js>, value: Value<'js>) -> Result<serde_json::Value, String> {
    if value.is_undefined() {
        return Ok(serde_json::Value::Null);
    }
    let Some(text) = js
        .json_stringify(value)
        .map_err(|err| err.to_string())?
        .map(|text| text.to_string())
        .transpose()
        .map_err(|err| err.to_string())?
    else {
        return Ok(serde_json::Value::Null);
    };
    serde_json::from_str(&text).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bus::{Bus, Event},
        history::SqliteHistory,
        inference::Inference,
        message::Message,
        tools::{InternalTools, InvocationScope, messages::SendMessageTool},
    };

    fn test_tools(bus: &Bus) -> Arc<InternalTools> {
        let root = std::env::temp_dir().join(format!(
            "ironpaw-script-test-runtime-{}",
            uuid::Uuid::new_v4()
        ));
        let layout = crate::config::RuntimeLayout::from_root(root);
        let files_dir = layout.files_dir();
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "ironpaw-script-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        let secrets = crate::secrets::Secrets::for_tests();
        Arc::new(InternalTools::new(
            bus.clone(),
            layout,
            Inference::new(bus.clone(), secrets.tools.clone()).unwrap(),
            SendMessageTool::new(bus.clone(), |_, _| Err("no attachments".to_string())),
            crate::tools::messages::DeleteMessageTool::new(bus.clone()),
            files_dir,
            Arc::new(SqliteHistory::new(history).unwrap()),
            secrets,
        ))
    }

    fn test_runtime(bus: &Bus) -> Arc<ScriptRuntime> {
        Arc::clone(&test_tools(bus).scripts)
    }

    #[tokio::test]
    async fn script_calls_send_message_tool() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let runtime = test_runtime(&bus);

        runtime
            .run(
                r#"
                const r = send_message({ channel: "telegram", chat_id: "chat-1", content: "hi from script" });
                if (!r.ok) { throw new Error("send failed"); }
                "#
                .to_string(),
                ExecutionContext::new(InvocationScope::ScheduledAutomation {
                    automation_id: "script-test".to_string(),
                }),
                serde_json::json!({}),
            )
            .await
            .expect("script should run");

        let event = events.try_recv().expect("expected MessageOut");
        let Event::MessageOut {
            channel, message, ..
        } = event
        else {
            panic!("expected MessageOut");
        };
        assert_eq!(channel, "telegram");
        let Message::Assistant { text, .. } = message else {
            panic!("expected assistant message");
        };
        assert_eq!(text.as_deref(), Some("hi from script"));
    }

    #[tokio::test]
    async fn script_can_await_a_tool_call() {
        let bus = Bus::new(16);
        let runtime = test_runtime(&bus);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0; 1024];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\nconnection: close\r\n\r\n[1,2,3]",
                )
                .await
                .unwrap();
        });

        let result = runtime
            .run(
                format!(
                    r#"
                    const r = JSON.parse(await web_fetch({{ url: "{url}" }}));
                    return {{ first: r[0], third: r[2] }};
	                "#
                ),
                ExecutionContext::new(InvocationScope::ScheduledAutomation {
                    automation_id: "script-test".to_string(),
                }),
                serde_json::json!({}),
            )
            .await
            .expect("script should run");

        assert_eq!(result, serde_json::json!({ "first": 1, "third": 3 }));
    }

    #[test]
    fn validation_allows_registered_and_local_functions() {
        let tools = BTreeSet::from(["web_fetch".to_string()]);

        validate_source(
            "function format(value) { return String(value); } const body = web_fetch({ url: 'https://example.com' }); return format(body);",
            &tools,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn require_explains_unavailable_modules() {
        let bus = Bus::new(16);
        let runtime = test_runtime(&bus);
        let ctx = ExecutionContext::new(InvocationScope::ScheduledAutomation {
            automation_id: "script-test".to_string(),
        });

        let fs_error = runtime
            .run(
                r#"require("fs");"#.to_string(),
                ctx.clone(),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert!(fs_error.contains("list_files"));
        assert!(fs_error.contains("require is unavailable"));

        let module_error = runtime
            .run(
                r#"require("left-pad");"#.to_string(),
                ctx,
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert!(module_error.contains("require is unavailable"));
    }
}
