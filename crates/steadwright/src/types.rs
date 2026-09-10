use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::ElementHandle;
use crate::JsHandle;
use serde_json::Value;

/// Default per-action timeout. Playwright's library default is 30 s, but an
/// agent pays the full timeout on every wrong guess, so this follows
/// Playwright MCP's 5 s action default instead.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// Default navigation timeout (Playwright MCP uses 60 s).
pub const DEFAULT_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Internal bootstrap waits (connect, new page attach).
pub const DEFAULT_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);

pub trait IntoTimeout {
    fn into_timeout(self) -> Duration;
}

impl IntoTimeout for Duration {
    fn into_timeout(self) -> Duration {
        self
    }
}

impl IntoTimeout for u64 {
    fn into_timeout(self) -> Duration {
        Duration::from_millis(self)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum World {
    #[default]
    Main,
    Utility,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LoadState {
    #[default]
    Load,
    DomContentLoaded,
    NetworkIdle,
    Commit,
}

#[derive(Clone, Debug)]
pub struct GotoOptions {
    pub wait_until: LoadState,
    pub timeout: Option<Duration>,
    pub referer: Option<String>,
}
impl Default for GotoOptions {
    fn default() -> Self {
        Self {
            wait_until: LoadState::Load,
            timeout: None,
            referer: None,
        }
    }
}

pub enum UrlMatcher {
    Glob(String),
    Regex(regex::Regex),
    Predicate(Arc<dyn Fn(&str) -> bool + Send + Sync>),
}
impl std::fmt::Debug for UrlMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Glob(v) => f.debug_tuple("Glob").field(v).finish(),
            Self::Regex(v) => f.debug_tuple("Regex").field(v).finish(),
            Self::Predicate(_) => f.write_str("Predicate(..)"),
        }
    }
}
impl UrlMatcher {
    pub fn predicate(f: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self::Predicate(Arc::new(f))
    }
    pub fn matches(&self, url: &str) -> bool {
        match self {
            Self::Glob(v) => glob_matches(v, url),
            Self::Regex(v) => v.is_match(url),
            Self::Predicate(v) => v(url),
        }
    }
}

#[derive(Debug)]
pub struct WaitForUrlOptions {
    pub wait_until: LoadState,
    pub timeout: Option<Duration>,
}
impl Default for WaitForUrlOptions {
    fn default() -> Self {
        Self {
            wait_until: LoadState::Load,
            timeout: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub url: String,
    pub status: u16,
    pub ok: bool,
    pub headers: HashMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub post_data: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewportSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenshotClip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScreenshotFormat {
    #[default]
    Png,
    Jpeg,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScreenshotScale {
    #[default]
    Css,
    Device,
}

#[derive(Clone, Debug, Default)]
pub struct ScreenshotOptions {
    pub full_page: bool,
    pub clip: Option<ScreenshotClip>,
    pub format: ScreenshotFormat,
    pub quality: Option<u8>,
    pub omit_background: bool,
    pub scale: ScreenshotScale,
    pub timeout: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
    Back,
    Forward,
}
impl MouseButton {
    pub(crate) fn cdp_name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
            Self::Back => "back",
            Self::Forward => "forward",
        }
    }
    pub(crate) fn mask(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 4,
            Self::Back => 8,
            Self::Forward => 16,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClickOptions {
    pub button: MouseButton,
    pub click_count: u8,
    pub delay: Option<Duration>,
    pub steps: u32,
}
impl Default for ClickOptions {
    fn default() -> Self {
        Self {
            button: MouseButton::Left,
            click_count: 1,
            delay: None,
            steps: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogType {
    Alert,
    BeforeUnload,
    Confirm,
    Prompt,
}

#[derive(Clone, Debug)]
pub struct ConsoleMessage {
    pub type_: String,
    pub text: String,
    pub args: Vec<JsHandle>,
}

#[derive(Clone, Debug)]
pub struct PageError {
    pub message: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BoundingBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyboardModifier {
    Alt,
    Control,
    Meta,
    Shift,
}

impl KeyboardModifier {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Alt => "Alt",
            Self::Control => "Control",
            Self::Meta => "Meta",
            Self::Shift => "Shift",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ActionOptions {
    pub button: MouseButton,
    pub click_count: u8,
    pub delay: Option<Duration>,
    pub position: Option<Point>,
    pub modifiers: Vec<KeyboardModifier>,
    pub force: bool,
    pub no_wait_after: bool,
    pub trial: bool,
    pub timeout: Option<Duration>,
}

impl Default for ActionOptions {
    fn default() -> Self {
        Self {
            button: MouseButton::Left,
            click_count: 1,
            delay: None,
            position: None,
            modifiers: Vec::new(),
            force: false,
            no_wait_after: false,
            trial: false,
            timeout: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct LocatorFilter {
    pub has_text: Option<crate::TextMatch>,
    pub has_not_text: Option<crate::TextMatch>,
    pub has: Option<crate::Locator>,
    pub has_not: Option<crate::Locator>,
    pub visible: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WaitForSelectorState {
    Attached,
    Detached,
    #[default]
    Visible,
    Hidden,
}

#[derive(Clone, Debug, Default)]
pub struct WaitForOptions {
    pub state: WaitForSelectorState,
    pub timeout: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Polling {
    #[default]
    Raf,
    Interval(Duration),
}

#[derive(Clone, Debug, Default)]
pub struct WaitForFunctionOptions {
    pub polling: Polling,
    pub timeout: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
pub struct SelectOptionValue {
    pub value: Option<String>,
    pub label: Option<String>,
    pub index: Option<usize>,
    pub element: Option<ElementHandle>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePayload {
    pub name: String,
    pub mime_type: String,
    pub buffer: Vec<u8>,
    pub last_modified_ms: u64,
}

#[derive(Clone, Debug)]
pub enum PageEvent {
    Download(crate::Download),
    Dialog(crate::Dialog),
    FileChooser(crate::FileChooser),
    Popup(crate::Page),
}

#[derive(Clone, Debug)]
pub enum InputFiles {
    Paths(Vec<PathBuf>),
    Payloads(Vec<FilePayload>),
}

impl From<Vec<PathBuf>> for InputFiles {
    fn from(value: Vec<PathBuf>) -> Self {
        Self::Paths(value)
    }
}

impl From<Vec<FilePayload>> for InputFiles {
    fn from(value: Vec<FilePayload>) -> Self {
        Self::Payloads(value)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AriaSnapshotMode {
    #[default]
    Ai,
    Default,
}

#[derive(Clone, Debug)]
pub struct AriaSnapshotOptions {
    pub mode: AriaSnapshotMode,
    pub depth: Option<u32>,
    pub boxes: bool,
    pub selector: Option<String>,
    pub timeout: Option<Duration>,
}

impl Default for AriaSnapshotOptions {
    fn default() -> Self {
        Self {
            mode: AriaSnapshotMode::Ai,
            depth: None,
            boxes: false,
            selector: None,
            timeout: None,
        }
    }
}

/// A JavaScript value as represented by Playwright's evaluation protocol.
///
/// Unlike [`serde_json::Value`], this preserves JavaScript-only values such as
/// `undefined`, non-finite numbers, bigint, dates, regular expressions, maps,
/// and sets.
#[derive(Clone, Debug, PartialEq)]
pub enum JsValue {
    Null,
    Undefined,
    Bool(bool),
    Number(f64),
    BigInt(String),
    String(String),
    /// An ISO-8601 string, as produced by JavaScript's `Date::toJSON`.
    Date(String),
    RegExp {
        source: String,
        flags: String,
    },
    Array(Vec<JsValue>),
    Object(Vec<(String, JsValue)>),
    Map(Vec<(JsValue, JsValue)>),
    Set(Vec<JsValue>),
}

impl JsValue {
    /// Converts this value to ordinary JSON, losing JavaScript-specific type
    /// information where JSON has no equivalent representation.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Null | Self::Undefined => Value::Null,
            Self::Bool(value) => Value::Bool(*value),
            Self::Number(value) => number_to_json(*value),
            Self::BigInt(value) | Self::String(value) | Self::Date(value) => {
                Value::String(value.clone())
            }
            Self::RegExp { source, flags } => Value::Object(
                [
                    ("source".to_owned(), Value::String(source.clone())),
                    ("flags".to_owned(), Value::String(flags.clone())),
                ]
                .into_iter()
                .collect(),
            ),
            Self::Array(values) | Self::Set(values) => {
                Value::Array(values.iter().map(Self::to_json).collect())
            }
            Self::Object(entries) => Value::Object(
                entries
                    .iter()
                    .map(|(key, value)| (key.clone(), value.to_json()))
                    .collect(),
            ),
            Self::Map(entries) => Value::Array(
                entries
                    .iter()
                    .map(|(key, value)| Value::Array(vec![key.to_json(), value.to_json()]))
                    .collect(),
            ),
        }
    }
}

fn number_to_json(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value == 0.0 && value.is_sign_negative() {
        return Value::Number(
            serde_json::Number::from_f64(value).expect("negative zero is a finite JSON number"),
        );
    }
    if value.fract() == 0.0 {
        if value >= 0.0 && value < u64::MAX as f64 {
            return Value::Number(serde_json::Number::from(value as u64));
        }
        if value >= i64::MIN as f64 && value < -(i64::MIN as f64) {
            return Value::Number(serde_json::Number::from(value as i64));
        }
    }
    Value::Number(serde_json::Number::from_f64(value).expect("a finite f64 is a valid JSON number"))
}

impl From<Value> for JsValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(value),
            Value::Number(value) => Self::Number(
                value
                    .as_f64()
                    .expect("a serde_json::Number is always representable as f64"),
            ),
            Value::String(value) => Self::String(value),
            Value::Array(values) => Self::Array(values.into_iter().map(JsValue::from).collect()),
            Value::Object(entries) => Self::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, JsValue::from(value)))
                    .collect(),
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub enum CallArg {
    Value(JsValue),
    Handle(JsHandle),
    Undefined,
}
impl From<JsValue> for CallArg {
    fn from(value: JsValue) -> Self {
        Self::Value(value)
    }
}
impl From<Value> for CallArg {
    fn from(value: Value) -> Self {
        Self::Value(value.into())
    }
}
impl From<&str> for CallArg {
    fn from(value: &str) -> Self {
        Self::Value(JsValue::String(value.to_owned()))
    }
}
impl From<String> for CallArg {
    fn from(value: String) -> Self {
        Self::Value(JsValue::String(value))
    }
}
impl From<bool> for CallArg {
    fn from(value: bool) -> Self {
        Self::Value(JsValue::Bool(value))
    }
}
impl From<u64> for CallArg {
    fn from(value: u64) -> Self {
        Self::Value(JsValue::Number(value as f64))
    }
}
impl From<&JsHandle> for CallArg {
    fn from(value: &JsHandle) -> Self {
        Self::Handle(value.clone())
    }
}
impl From<JsHandle> for CallArg {
    fn from(value: JsHandle) -> Self {
        Self::Handle(value)
    }
}

#[derive(Clone, Debug)]
pub struct DownloadOptions {
    pub path: PathBuf,
}

pub(crate) fn glob_matches(pattern: &str, value: &str) -> bool {
    glob_regex(pattern).is_some_and(|regex| regex.is_match(value))
}

fn glob_regex(glob: &str) -> Option<regex::Regex> {
    let chars: Vec<char> = glob.chars().collect();
    let mut pattern = String::from("^");
    let mut in_group = false;
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '\\' && index + 1 < chars.len() {
            index += 1;
            pattern.push_str(&regex::escape(&chars[index].to_string()));
        } else if character == '*' {
            let before = index.checked_sub(1).and_then(|i| chars.get(i)).copied();
            let mut count = 1;
            while chars.get(index + 1) == Some(&'*') {
                count += 1;
                index += 1;
            }
            if count > 1 {
                if chars.get(index + 1) == Some(&'/') {
                    pattern.push_str(if before == Some('/') {
                        "((.+/)|)"
                    } else {
                        "(.*/)"
                    });
                    index += 1;
                } else {
                    pattern.push_str("(.*)");
                }
            } else {
                pattern.push_str("([^/]*)");
            }
        } else {
            match character {
                '{' if !in_group => {
                    in_group = true;
                    pattern.push('(');
                }
                '}' if in_group => {
                    in_group = false;
                    pattern.push(')');
                }
                ',' if in_group => pattern.push('|'),
                '{' | '}' => return None,
                _ => pattern.push_str(&regex::escape(&character.to_string())),
            }
        }
        index += 1;
    }
    if in_group {
        return None;
    }
    pattern.push('$');
    regex::Regex::new(&pattern).ok()
}

#[cfg(test)]
mod tests {
    use super::{JsValue, glob_matches};
    use serde_json::{Value, json};

    #[test]
    fn url_globs() {
        assert!(glob_matches("**/foo?bar=*", "https://a.test/x/foo?bar=42"));
        assert!(glob_matches("https://*.test/**", "https://a.test/x/y"));
        assert!(glob_matches("**/*.{png,jpg}", "https://a.test/x/photo.png"));
        assert!(!glob_matches("https://*.test/a", "http://a.test/a"));
    }

    #[test]
    fn json_values_convert_to_js_values_and_back() {
        let json = json!({
            "null": null,
            "bool": true,
            "number": 42.5,
            "string": "steadwright",
            "array": [1, false, { "nested": "value" }]
        });

        let value = JsValue::from(json.clone());

        assert_eq!(
            value,
            JsValue::Object(vec![
                ("null".into(), JsValue::Null),
                ("bool".into(), JsValue::Bool(true)),
                ("number".into(), JsValue::Number(42.5)),
                ("string".into(), JsValue::String("steadwright".into())),
                (
                    "array".into(),
                    JsValue::Array(vec![
                        JsValue::Number(1.0),
                        JsValue::Bool(false),
                        JsValue::Object(vec![("nested".into(), JsValue::String("value".into()),)]),
                    ]),
                ),
            ])
        );
        assert_eq!(value.to_json(), json);
    }

    #[test]
    fn to_json_has_documented_lossy_special_value_shapes() {
        assert_eq!(JsValue::Undefined.to_json(), Value::Null);
        assert_eq!(JsValue::Number(f64::NAN).to_json(), Value::Null);
        assert_eq!(JsValue::Number(f64::INFINITY).to_json(), Value::Null);
        assert_eq!(JsValue::Number(f64::NEG_INFINITY).to_json(), Value::Null);

        let negative_zero = JsValue::Number(-0.0).to_json();
        assert!(negative_zero.as_f64().unwrap().is_sign_negative());
        assert_eq!(JsValue::Number(1.0).to_json(), json!(1));
        assert_eq!(JsValue::Number(-2.0).to_json(), json!(-2));
        assert_eq!(JsValue::Number(1.5).to_json(), json!(1.5));
        assert_eq!(
            JsValue::BigInt("12345678901234567890".into()).to_json(),
            json!("12345678901234567890")
        );
        assert_eq!(
            JsValue::Date("2020-05-06T07:08:09.000Z".into()).to_json(),
            json!("2020-05-06T07:08:09.000Z")
        );
        assert_eq!(
            JsValue::RegExp {
                source: "a+b".into(),
                flags: "gi".into(),
            }
            .to_json(),
            json!({ "source": "a+b", "flags": "gi" })
        );
        assert_eq!(
            JsValue::Map(vec![(JsValue::String("one".into()), JsValue::Number(1.0),)]).to_json(),
            json!([["one", 1]])
        );
        assert_eq!(
            JsValue::Set(vec![JsValue::String("two".into())]).to_json(),
            json!(["two"])
        );
    }
}
