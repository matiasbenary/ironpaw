/// Fill `{{event.field}}` placeholders in stored args from the triggering
/// event. A string that is exactly one placeholder takes the field's JSON
/// value; placeholders embedded in longer strings are stringified in place.
pub(super) fn render_args(
    args: &serde_json::Value,
    event: &serde_json::Value,
) -> serde_json::Value {
    match args {
        serde_json::Value::String(text) => render_string(text, event),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|item| render_args(item, event)).collect())
        }
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), render_args(value, event)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn render_string(text: &str, event: &serde_json::Value) -> serde_json::Value {
    let trimmed = text.trim();
    if let Some(path) = exact_placeholder(trimmed) {
        return lookup(event, path)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
    }

    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            result.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let path = after[..end].trim();
        match lookup(event, path) {
            Some(serde_json::Value::String(value)) => result.push_str(value),
            Some(value) => result.push_str(&value.to_string()),
            None => {}
        }
        rest = &after[end + 2..];
    }
    result.push_str(rest);
    serde_json::Value::String(result)
}

fn exact_placeholder(text: &str) -> Option<&str> {
    let inner = text.strip_prefix("{{")?.strip_suffix("}}")?;
    let inner = inner.trim();
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    Some(inner)
}

/// Look up a dot path like `event.text`; the leading `event.` is optional.
fn lookup<'a>(event: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let path = path.strip_prefix("event.").unwrap_or(path);
    let mut current = event;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_placeholders_fill_from_the_event() {
        let event = serde_json::json!({
            "channel": "tui",
            "text": "hi",
        });
        let args = serde_json::json!({
            "message": "{{event.text}}",
            "summary": "got '{{event.text}}' on {{event.channel}}",
            "missing": "{{event.nope}}",
            "untouched": 42,
        });

        let rendered = render_args(&args, &event);

        assert_eq!(rendered["message"], "hi");
        assert_eq!(rendered["summary"], "got 'hi' on tui");
        assert_eq!(rendered["missing"], serde_json::Value::Null);
        assert_eq!(rendered["untouched"], 42);
    }
}
