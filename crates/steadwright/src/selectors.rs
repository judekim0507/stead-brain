use serde_json::to_string;

/// A string or JavaScript regular expression accepted by Playwright text selectors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextMatch {
    Str(String),
    Regex(String, String),
}

impl From<&str> for TextMatch {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

impl From<String> for TextMatch {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ByRoleOptions {
    pub name: Option<TextMatch>,
    pub exact: bool,
    pub checked: Option<bool>,
    pub disabled: Option<bool>,
    pub expanded: Option<bool>,
    pub include_hidden: Option<bool>,
    pub level: Option<u32>,
    pub pressed: Option<bool>,
    pub selected: Option<bool>,
}

pub fn get_by_role(role: &str, options: &ByRoleOptions) -> String {
    let mut selector = format!("internal:role={role}");
    append_bool(&mut selector, "checked", options.checked);
    append_bool(&mut selector, "disabled", options.disabled);
    append_bool(&mut selector, "selected", options.selected);
    append_bool(&mut selector, "expanded", options.expanded);
    append_bool(&mut selector, "include-hidden", options.include_hidden);
    if let Some(level) = options.level {
        selector.push_str(&format!("[level={level}]"));
    }
    if let Some(name) = &options.name {
        selector.push_str("[name=");
        selector.push_str(&escape_for_attribute_selector(name, options.exact));
        selector.push(']');
    }
    append_bool(&mut selector, "pressed", options.pressed);
    selector
}

pub fn get_by_text(text: &TextMatch, exact: bool) -> String {
    format!("internal:text={}", escape_for_text_selector(text, exact))
}

pub fn get_by_label(text: &TextMatch, exact: bool) -> String {
    format!("internal:label={}", escape_for_text_selector(text, exact))
}

pub fn get_by_placeholder(text: &TextMatch, exact: bool) -> String {
    by_attribute("placeholder", text, exact)
}

pub fn get_by_alt_text(text: &TextMatch, exact: bool) -> String {
    by_attribute("alt", text, exact)
}

pub fn get_by_title(text: &TextMatch, exact: bool) -> String {
    by_attribute("title", text, exact)
}

pub fn get_by_test_id(text: &TextMatch) -> String {
    format!(
        "internal:testid=[data-testid={}]",
        escape_for_attribute_selector(text, true)
    )
}

pub(crate) fn escape_for_text_selector(text: &TextMatch, exact: bool) -> String {
    match text {
        TextMatch::Str(value) => format!(
            "{}{}",
            to_string(value).expect("strings always serialize as JSON"),
            if exact { 's' } else { 'i' }
        ),
        TextMatch::Regex(source, flags) => escape_regex(source, flags),
    }
}

pub(crate) fn json_string(value: &str) -> String {
    to_string(value).expect("strings always serialize as JSON")
}

fn by_attribute(name: &str, text: &TextMatch, exact: bool) -> String {
    format!(
        "internal:attr=[{name}={}]",
        escape_for_attribute_selector(text, exact)
    )
}

fn escape_for_attribute_selector(text: &TextMatch, exact: bool) -> String {
    match text {
        TextMatch::Str(value) => {
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"{}", if exact { 's' } else { 'i' })
        }
        TextMatch::Regex(source, flags) => escape_regex(source, flags),
    }
}

fn escape_regex(source: &str, flags: &str) -> String {
    if flags.contains('u') || flags.contains('v') {
        return format!("/{source}/{flags}");
    }
    let mut escaped = String::with_capacity(source.len());
    let mut backslashes = 0usize;
    for character in source.chars() {
        if matches!(character, '"' | '\'' | '`') && backslashes % 2 == 0 {
            escaped.push('\\');
        }
        escaped.push(character);
        if character == '\\' {
            backslashes += 1;
        } else {
            backslashes = 0;
        }
    }
    format!("/{}/{flags}", escaped.replace(">>", "\\>\\>"))
}

fn append_bool(selector: &mut String, name: &str, value: Option<bool>) {
    if let Some(value) = value {
        selector.push_str(&format!("[{name}={value}]"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(value: &str) -> TextMatch {
        TextMatch::Str(value.to_owned())
    }

    fn regex(source: &str, flags: &str) -> TextMatch {
        TextMatch::Regex(source.to_owned(), flags.to_owned())
    }

    #[test]
    fn locator_utils_selector_table_matches_playwright() {
        let cases = [
            (
                get_by_text(&string("Hello"), false),
                "internal:text=\"Hello\"i",
            ),
            (
                get_by_text(&string("Hello"), true),
                "internal:text=\"Hello\"s",
            ),
            (
                get_by_text(&string("say \"hi\""), false),
                "internal:text=\"say \\\"hi\\\"\"i",
            ),
            (
                get_by_text(&string("a\\b"), true),
                "internal:text=\"a\\\\b\"s",
            ),
            (
                get_by_text(&string("line\nfeed"), false),
                "internal:text=\"line\\nfeed\"i",
            ),
            (
                get_by_text(&regex("hello.*", "i"), false),
                "internal:text=/hello.*/i",
            ),
            (
                get_by_text(&regex("a>>b", ""), true),
                "internal:text=/a\\>\\>b/",
            ),
            (
                get_by_text(&regex("a\"b", "g"), false),
                "internal:text=/a\\\"b/g",
            ),
            (
                get_by_text(&regex("a\"b", "u"), false),
                "internal:text=/a\"b/u",
            ),
            (
                get_by_label(&string("Email"), false),
                "internal:label=\"Email\"i",
            ),
            (
                get_by_label(&string("Email"), true),
                "internal:label=\"Email\"s",
            ),
            (
                get_by_label(&regex("E.?mail", "i"), true),
                "internal:label=/E.?mail/i",
            ),
            (
                get_by_placeholder(&string("name"), false),
                "internal:attr=[placeholder=\"name\"i]",
            ),
            (
                get_by_placeholder(&string("a\\b"), true),
                "internal:attr=[placeholder=\"a\\\\b\"s]",
            ),
            (
                get_by_alt_text(&string("A \"cat\""), false),
                "internal:attr=[alt=\"A \\\"cat\\\"\"i]",
            ),
            (
                get_by_alt_text(&regex("cat|dog", "i"), true),
                "internal:attr=[alt=/cat|dog/i]",
            ),
            (
                get_by_title(&string("Settings"), true),
                "internal:attr=[title=\"Settings\"s]",
            ),
            (
                get_by_title(&regex("Set.*", ""), false),
                "internal:attr=[title=/Set.*/]",
            ),
            (
                get_by_test_id(&string("submit")),
                "internal:testid=[data-testid=\"submit\"s]",
            ),
            (
                get_by_test_id(&string("a\\\"b")),
                "internal:testid=[data-testid=\"a\\\\\\\"b\"s]",
            ),
            (
                get_by_test_id(&regex("submit-\\d+", "i")),
                "internal:testid=[data-testid=/submit-\\d+/i]",
            ),
        ];
        for (actual, expected) in cases {
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn role_options_follow_playwright_property_order() {
        assert_eq!(
            get_by_role(
                "button",
                &ByRoleOptions {
                    name: Some(string("Submit \"now\"")),
                    exact: false,
                    checked: Some(false),
                    disabled: Some(true),
                    expanded: Some(false),
                    include_hidden: Some(true),
                    level: Some(2),
                    pressed: Some(true),
                    selected: Some(false),
                }
            ),
            "internal:role=button[checked=false][disabled=true][selected=false][expanded=false][include-hidden=true][level=2][name=\"Submit \\\"now\\\"\"i][pressed=true]"
        );
        assert_eq!(
            get_by_role(
                "heading",
                &ByRoleOptions {
                    name: Some(regex("Chapter \\d+", "i")),
                    exact: true,
                    level: Some(3),
                    ..Default::default()
                }
            ),
            "internal:role=heading[level=3][name=/Chapter \\d+/i]"
        );
        assert_eq!(
            get_by_role("link", &ByRoleOptions::default()),
            "internal:role=link"
        );
    }
}
