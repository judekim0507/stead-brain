use serde_json::Value;

pub(crate) fn render_aria_snapshot_as_yaml(snapshot: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(nodes) = snapshot.as_array() {
        for node in nodes {
            visit(node, 0, &mut lines);
        }
    }
    lines.join("\n")
}

fn visit(node: &Value, depth: usize, lines: &mut Vec<String>) {
    if let Some(text) = node.as_str() {
        visit_text(text, depth, lines);
        return;
    }
    let Some(object) = node.as_object() else {
        return;
    };
    if object.get("role").and_then(Value::as_str) == Some("text") {
        visit_text(
            object.get("text").and_then(Value::as_str).unwrap_or(""),
            depth,
            lines,
        );
        return;
    }

    let key = create_key(node);
    let escaped_key = format!("{}- {}", indent(depth), yaml_escape_key_if_needed(&key));
    let mut props = Vec::new();
    if let Some(url) = object.get("url").and_then(Value::as_str) {
        props.push(("url", url));
    }
    if let Some(placeholder) = object.get("placeholder").and_then(Value::as_str) {
        props.push(("placeholder", placeholder));
    }
    let text = object.get("text").and_then(Value::as_str);
    let children = object.get("children").and_then(Value::as_array);
    if text.is_none() && props.is_empty() && children.is_none_or(Vec::is_empty) {
        lines.push(escaped_key);
    } else if let (Some(text), true) = (text, props.is_empty()) {
        lines.push(format!(
            "{escaped_key}: {}",
            yaml_escape_value_if_needed(text)
        ));
    } else {
        lines.push(format!("{escaped_key}:"));
        for (name, value) in props {
            lines.push(format!(
                "{}- /{name}: {}",
                indent(depth + 1),
                yaml_escape_value_if_needed(value)
            ));
        }
        if let Some(text) = text {
            visit_text(text, depth + 1, lines);
        } else if let Some(children) = children {
            for child in children {
                visit(child, depth + 1, lines);
            }
        }
    }
}

fn visit_text(text: &str, depth: usize, lines: &mut Vec<String>) {
    let escaped = yaml_escape_value_if_needed(text);
    if !escaped.is_empty() {
        lines.push(format!("{}- text: {escaped}", indent(depth)));
    }
}

fn create_key(node: &Value) -> String {
    let object = node.as_object().expect("ARIA nodes are objects");
    let mut key = object
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Some(name) = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && name.len() <= 900)
    {
        key.push(' ');
        if name.starts_with('/') && name.ends_with('/') {
            key.push_str(name);
        } else {
            key.push_str(&serde_json::to_string(name).expect("strings always serialize as JSON"));
        }
    }
    if let Some(value) = object.get("_interactiveValue").and_then(Value::as_str) {
        key.push_str(" [value=");
        key.push_str(&serde_json::to_string(value).expect("strings always serialize as JSON"));
        key.push(']');
    }
    if object.get("checked").and_then(Value::as_str) == Some("mixed") {
        key.push_str(" [checked=mixed]");
    } else if truthy(object.get("checked")) {
        key.push_str(" [checked]");
    }
    if truthy(object.get("disabled")) {
        key.push_str(" [disabled]");
    }
    if truthy(object.get("expanded")) {
        key.push_str(" [expanded]");
    }
    if truthy(object.get("active")) {
        key.push_str(" [active]");
    }
    match object.get("invalid") {
        Some(Value::String(value)) if matches!(value.as_str(), "grammar" | "spelling") => {
            key.push_str(&format!(" [invalid={value}]"));
        }
        value if truthy(value) => key.push_str(" [invalid]"),
        _ => {}
    }
    if let Some(level) = object.get("level").and_then(Value::as_u64) {
        if level != 0 {
            key.push_str(&format!(" [level={level}]"));
        }
    }
    if object.get("pressed").and_then(Value::as_str) == Some("mixed") {
        key.push_str(" [pressed=mixed]");
    } else if truthy(object.get("pressed")) {
        key.push_str(" [pressed]");
    }
    if truthy(object.get("selected")) {
        key.push_str(" [selected]");
    }
    if truthy(object.get("ariaHidden")) {
        key.push_str(" [aria-hidden]");
    }
    if let Some(reference) = object.get("ref").and_then(Value::as_str) {
        key.push_str(&format!(" [ref={reference}]"));
        if object.get("cursor").and_then(Value::as_str) == Some("pointer") {
            key.push_str(" [cursor=pointer]");
        }
    }
    if let Some(rect) = object.get("box").and_then(Value::as_object) {
        let field = |name| rect.get(name).map(number_string).unwrap_or_default();
        key.push_str(&format!(
            " [box={},{},{},{}]",
            field("x"),
            field("y"),
            field("width"),
            field("height")
        ));
    }
    key
}

fn truthy(value: Option<&Value>) -> bool {
    value.and_then(Value::as_bool).unwrap_or(false)
}

fn number_string(value: &Value) -> String {
    value
        .as_f64()
        .map(|number| {
            if number.fract() == 0.0 {
                format!("{number:.0}")
            } else {
                number.to_string()
            }
        })
        .unwrap_or_default()
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

fn yaml_escape_key_if_needed(value: &str) -> String {
    if !yaml_string_needs_quotes(value) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "''"))
}

fn yaml_escape_value_if_needed(value: &str) -> String {
    if !yaml_string_needs_quotes(value) {
        return value.to_owned();
    }
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\u{0008}' => escaped.push_str("\\b"),
            '\u{000c}' => escaped.push_str("\\f"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if ('\u{0000}'..='\u{001f}').contains(&c)
                || ('\u{007f}'..='\u{009f}').contains(&c) =>
            {
                escaped.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => escaped.push(c),
        }
    }
    format!("\"{escaped}\"")
}

fn yaml_string_needs_quotes(value: &str) -> bool {
    if value.is_empty()
        || value.starts_with(char::is_whitespace)
        || value.ends_with(char::is_whitespace)
        || value.starts_with('-')
        || value.starts_with('[')
        || value.chars().any(|c| {
            ('\u{0000}'..='\u{0008}').contains(&c)
                || matches!(c, '\u{000b}' | '\u{000c}')
                || ('\u{000e}'..='\u{001f}').contains(&c)
                || ('\u{007f}'..='\u{009f}').contains(&c)
        })
        || value.contains('\n')
        || value.contains('\r')
        || value.contains('{')
        || value.contains('}')
        || value.contains('`')
    {
        return true;
    }
    if value.char_indices().any(|(index, c)| {
        c == ':'
            && (index + c.len_utf8() == value.len()
                || value[index + c.len_utf8()..].starts_with(char::is_whitespace))
    }) || value
        .char_indices()
        .any(|(index, c)| c == '#' && value[..index].ends_with(char::is_whitespace))
    {
        return true;
    }
    if matches!(
        value.chars().next(),
        Some('&' | '*' | ']' | ',' | '?' | '!' | '>' | '|' | '@' | '"' | '\'' | '#' | '%')
    ) {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    value.parse::<f64>().is_ok()
        || matches!(
            lower.as_str(),
            "y" | "n" | "yes" | "no" | "true" | "false" | "on" | "off" | "null"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_playwright_aria_yaml_shape() {
        let snapshot = json!([
            {"role":"heading","name":"Welcome","level":2,"ref":"e1"},
            {"role":"checkbox","name":"Ready","checked":true,"disabled":true,"ref":"e2"},
            {"role":"textbox","name":"Email","placeholder":"you@example.test","children":["ignored"]},
            {"role":"paragraph","children":[{"role":"text","text":"hello: world"}]}
        ]);
        assert_eq!(
            render_aria_snapshot_as_yaml(&snapshot),
            r#"- heading "Welcome" [level=2] [ref=e1]
- checkbox "Ready" [checked] [disabled] [ref=e2]
- textbox "Email":
  - /placeholder: you@example.test
  - text: ignored
- paragraph:
  - text: "hello: world""#
        );
    }

    #[test]
    fn renders_interactive_values_in_the_node_key() {
        let snapshot = json!([
            {"role":"textbox","name":"Search","_interactiveValue":"quoted \"value\"","ref":"e1"},
            {"role":"combobox","name":"Size","_interactiveValue":"large","expanded":true,"ref":"e2"}
        ]);
        assert_eq!(
            render_aria_snapshot_as_yaml(&snapshot),
            "- textbox \"Search\" [value=\"quoted \\\"value\\\"\"] [ref=e1]\n- combobox \"Size\" [value=\"large\"] [expanded] [ref=e2]"
        );
    }

    #[test]
    fn ignores_raw_values_to_preserve_default_snapshot_output() {
        let snapshot = json!([
            {"role":"textbox","name":"Search","value":"raw upstream value","ref":"e1"}
        ]);
        assert_eq!(
            render_aria_snapshot_as_yaml(&snapshot),
            "- textbox \"Search\" [ref=e1]"
        );
    }
}
