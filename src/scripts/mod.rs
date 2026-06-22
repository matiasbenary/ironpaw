use std::{sync::Arc, time::Duration};

use rquickjs::{CatchResultExt, Context, Ctx, Runtime, Value, function::Func};

use crate::tools::{self, ToolContext, ToolRegistry};

/// Wall-clock budget for one script execution: a runaway loop gets
/// interrupted instead of wedging the executor.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);
const SCRIPT_MEMORY_LIMIT: usize = 32 * 1024 * 1024;

/// Runs dynamic-tool scripts in a QuickJS sandbox. Scripts have no ambient
/// capabilities: the only way to act is the host-provided `tool(name, args)`
/// function, which is defined fresh per execution with the caller's tool
/// context. The tool's arguments are exposed as the global `args`, and the
/// script returns the tool result with an explicit `return`.
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
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let registry = Arc::clone(&self.registry);
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || execute_script(registry, handle, source, ctx, args))
            .await
            .map_err(|err| format!("script task failed: {err}"))?
    }
}

pub fn validate_source(source: &str) -> Result<(), String> {
    let runtime = Runtime::new().map_err(|err| err.to_string())?;
    runtime.set_memory_limit(SCRIPT_MEMORY_LIMIT);
    let context = Context::full(&runtime).map_err(|err| err.to_string())?;
    context.with(|js| -> Result<(), String> {
        let globals = js.globals();
        globals
            .set("__source", source)
            .map_err(|err| err.to_string())?;
        js.eval::<(), _>("new Function(__source);")
            .catch(&js)
            .map_err(|err| format!("script syntax error: {err}"))
    })
}

fn execute_script(
    registry: Arc<ToolRegistry>,
    handle: tokio::runtime::Handle,
    source: String,
    ctx: ToolContext,
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

        // The host boundary is string-typed; the prelude below wraps it
        // into the `tool(name, args)` the scripts actually use.
        let tool_json = Func::from(move |name: String, args: String| -> String {
            let args: serde_json::Value =
                serde_json::from_str(&args).unwrap_or(serde_json::Value::Null);
            let result = handle.block_on(tools::run_registered_tool(&registry, &name, args, &ctx));
            serde_json::to_string(&result)
                .unwrap_or_else(|err| format!(r#"{{"ok":false,"error":"{err}"}}"#))
        });
        globals
            .set("__tool_json", tool_json)
            .map_err(|err| err.to_string())?;

        let log = Func::from(|message: String| eprintln!("script: {message}"));
        globals.set("__log", log).map_err(|err| err.to_string())?;

        const PRELUDE: &str = r#"
            globalThis.tool = (name, args) => {
                return JSON.parse(__tool_json(name, JSON.stringify(args === undefined ? {} : args)));
            };
            globalThis.console = { log: (...a) => __log(a.map(String).join(" ")) };
        "#;
        js.eval::<(), _>(PRELUDE)
            .catch(&js)
            .map_err(|err| format!("script prelude error: {err}"))?;

        globals
            .set("__source", source)
            .map_err(|err| err.to_string())?;
        let result: Value = js
            .eval("new Function(__source)();")
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
        message::Message,
        tools::{InternalTools, messages::SendMessageTool, raw_inference::RawInferenceTool},
    };

    fn test_tools(bus: &Bus) -> Arc<InternalTools> {
        let root = std::env::temp_dir().join(format!(
            "warden-script-test-runtime-{}",
            uuid::Uuid::new_v4()
        ));
        let layout = crate::config::RuntimeLayout::from_root(root);
        let files_dir = layout.files_dir();
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "warden-script-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        Arc::new(InternalTools::new(
            bus.clone(),
            layout,
            RawInferenceTool::new().unwrap(),
            SendMessageTool::new(bus.clone(), |_| Err("no attachments".to_string())),
            crate::tools::messages::DeleteMessageTool::new(bus.clone()),
            files_dir,
            Arc::new(SqliteHistory::new(history).unwrap()),
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
                const r = tool("send_message", { channel: "telegram", content: "hi from script" });
                if (!r.ok) { throw new Error("send failed"); }
                "#
                .to_string(),
                ToolContext::default(),
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
    async fn script_reads_args_and_returns_explicit_value() {
        let bus = Bus::new(16);
        let runtime = test_runtime(&bus);

        let result = runtime
            .run(
                r#"
                const doubled = args.value * 2;
                return { doubled };
                "#
                .to_string(),
                ToolContext::default(),
                serde_json::json!({ "value": 21 }),
            )
            .await
            .expect("script should run");

        assert_eq!(result, serde_json::json!({ "doubled": 42 }));
    }

    #[tokio::test]
    async fn script_returns_from_function_body() {
        let bus = Bus::new(16);
        let runtime = test_runtime(&bus);

        let result = runtime
            .run(
                r#"
                return { ok: true, value: args.value };
                "#
                .to_string(),
                ToolContext::default(),
                serde_json::json!({ "value": "kiwi" }),
            )
            .await
            .expect("script should run");

        assert_eq!(result, serde_json::json!({ "ok": true, "value": "kiwi" }));
    }

    #[tokio::test]
    async fn web_fetch_result_is_plain_string() {
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
	                const r = tool("web_fetch", {{ url: "{url}" }});
	                return {{ first: JSON.parse(r)[0], third: JSON.parse(r)[2] }};
	                "#
                ),
                ToolContext::default(),
                serde_json::json!({}),
            )
            .await
            .expect("script should run");

        assert_eq!(result, serde_json::json!({ "first": 1, "third": 3 }));
    }

    #[tokio::test]
    async fn script_errors_are_reported() {
        let bus = Bus::new(16);
        let runtime = test_runtime(&bus);

        let result = runtime
            .run(
                "this is not valid javascript {".to_string(),
                ToolContext::default(),
                serde_json::json!({}),
            )
            .await;

        assert!(result.is_err());
    }
}
