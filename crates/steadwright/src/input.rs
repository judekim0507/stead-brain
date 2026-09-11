use std::collections::HashSet;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex;

use crate::{ClickOptions, Error, MouseButton, Page, Result};

const KEYPAD_LOCATION: u8 = 3;

#[derive(Default)]
pub(crate) struct InputState {
    keyboard: Mutex<KeyboardState>,
    mouse: Mutex<MouseState>,
}

#[derive(Default)]
struct KeyboardState {
    pressed_modifiers: HashSet<&'static str>,
    pressed_keys: HashSet<&'static str>,
}

#[derive(Default)]
struct MouseState {
    x: f64,
    y: f64,
    last_button: Option<MouseButton>,
    buttons: u8,
}

#[derive(Clone)]
pub struct Keyboard {
    page: Page,
    state: Arc<InputState>,
}

impl std::fmt::Debug for Keyboard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Keyboard").finish_non_exhaustive()
    }
}

impl Keyboard {
    pub(crate) fn new(page: Page, state: Arc<InputState>) -> Self {
        Self { page, state }
    }

    pub async fn down(&self, key: &str) -> Result<()> {
        let mut state = self.state.keyboard.lock().await;
        self.down_locked(&mut state, key).await
    }

    pub async fn up(&self, key: &str) -> Result<()> {
        let mut state = self.state.keyboard.lock().await;
        self.up_locked(&mut state, key).await
    }

    pub async fn press(&self, key: &str) -> Result<()> {
        let tokens = split_key_combination(key);
        let Some((pressed_key, modifiers)) = tokens.split_last() else {
            return Err(Error::InvalidArgument("Unknown key: \"\"".to_owned()));
        };

        let mut state = self.state.keyboard.lock().await;
        for modifier in modifiers {
            self.down_locked(&mut state, modifier).await?;
        }
        self.down_locked(&mut state, pressed_key).await?;
        self.up_locked(&mut state, pressed_key).await?;
        for modifier in modifiers.iter().rev() {
            self.up_locked(&mut state, modifier).await?;
        }
        Ok(())
    }

    pub async fn type_text(&self, text: &str) -> Result<()> {
        let mut state = self.state.keyboard.lock().await;
        for character in text.chars() {
            let key = character.to_string();
            if lookup_key(&key).is_some() {
                self.down_locked(&mut state, &key).await?;
                self.up_locked(&mut state, &key).await?;
            } else {
                self.page
                    .send_main("Input.insertText", json!({ "text": key }))
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn insert_text(&self, text: &str) -> Result<()> {
        self.page
            .send_main("Input.insertText", json!({ "text": text }))
            .await?;
        Ok(())
    }

    async fn down_locked(&self, state: &mut KeyboardState, key: &str) -> Result<()> {
        let description = key_description(key, state)?;
        let auto_repeat = state.pressed_keys.contains(description.code);
        state.pressed_keys.insert(description.code);
        if let Some(modifier) = modifier_for_key(description.key) {
            state.pressed_modifiers.insert(modifier);
        }

        let modifiers = modifiers_mask(&state.pressed_modifiers);
        let commands = commands_for_code(description.code, &state.pressed_modifiers);
        self.page
            .send_main(
                "Input.dispatchKeyEvent",
                json!({
                    "type": if description.text.is_empty() { "rawKeyDown" } else { "keyDown" },
                    "modifiers": modifiers,
                    "windowsVirtualKeyCode": description.key_code_without_location,
                    "code": description.code,
                    "commands": commands,
                    "key": description.key,
                    "text": description.text,
                    "unmodifiedText": description.text,
                    "autoRepeat": auto_repeat,
                    "location": description.location,
                    "isKeypad": description.location == KEYPAD_LOCATION,
                }),
            )
            .await?;
        Ok(())
    }

    async fn up_locked(&self, state: &mut KeyboardState, key: &str) -> Result<()> {
        let description = key_description(key, state)?;
        if let Some(modifier) = modifier_for_key(description.key) {
            state.pressed_modifiers.remove(modifier);
        }
        state.pressed_keys.remove(description.code);

        self.page
            .send_main(
                "Input.dispatchKeyEvent",
                json!({
                    "type": "keyUp",
                    "modifiers": modifiers_mask(&state.pressed_modifiers),
                    "key": description.key,
                    "windowsVirtualKeyCode": description.key_code_without_location,
                    "code": description.code,
                    "location": description.location,
                }),
            )
            .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Mouse {
    page: Page,
    state: Arc<InputState>,
}

impl std::fmt::Debug for Mouse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Mouse").finish_non_exhaustive()
    }
}

impl Mouse {
    pub(crate) fn new(page: Page, state: Arc<InputState>) -> Self {
        Self { page, state }
    }

    pub async fn move_to(&self, x: f64, y: f64, steps: u32) -> Result<()> {
        if steps == 0 {
            return Err(Error::InvalidArgument(
                "mouse.move_to: steps must be greater than 0".to_owned(),
            ));
        }
        let modifiers = self.modifiers().await;
        let mut state = self.state.mouse.lock().await;
        self.move_locked(&mut state, x, y, steps, modifiers).await
    }

    pub async fn down(&self, button: MouseButton, click_count: u8) -> Result<()> {
        let modifiers = self.modifiers().await;
        let mut state = self.state.mouse.lock().await;
        self.down_locked(&mut state, button, click_count, modifiers)
            .await
    }

    pub async fn up(&self, button: MouseButton, click_count: u8) -> Result<()> {
        let modifiers = self.modifiers().await;
        let mut state = self.state.mouse.lock().await;
        self.up_locked(&mut state, button, click_count, modifiers)
            .await
    }

    pub async fn click(&self, x: f64, y: f64, options: ClickOptions) -> Result<()> {
        if options.steps == 0 {
            return Err(Error::InvalidArgument(
                "mouse.click: steps must be greater than 0".to_owned(),
            ));
        }
        if options.click_count == 0 {
            return Err(Error::InvalidArgument(
                "mouse.click: click_count must be greater than 0".to_owned(),
            ));
        }

        let modifiers = self.modifiers().await;
        let mut state = self.state.mouse.lock().await;
        self.move_locked(&mut state, x, y, options.steps, modifiers)
            .await?;
        for count in 1..=options.click_count {
            self.down_locked(&mut state, options.button, count, modifiers)
                .await?;
            if let Some(delay) = options.delay {
                tokio::time::sleep(delay).await;
            }
            self.up_locked(&mut state, options.button, count, modifiers)
                .await?;
            if let (true, Some(delay)) = (count < options.click_count, options.delay) {
                tokio::time::sleep(delay).await;
            }
        }
        Ok(())
    }

    pub async fn dblclick(&self, x: f64, y: f64, mut options: ClickOptions) -> Result<()> {
        options.click_count = 2;
        self.click(x, y, options).await
    }

    pub async fn wheel(&self, delta_x: f64, delta_y: f64) -> Result<()> {
        let modifiers = self.modifiers().await;
        let state = self.state.mouse.lock().await;
        self.page
            .send_main(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseWheel",
                    "x": state.x,
                    "y": state.y,
                    "modifiers": modifiers,
                    "deltaX": delta_x,
                    "deltaY": delta_y,
                }),
            )
            .await?;
        Ok(())
    }

    async fn modifiers(&self) -> u8 {
        let keyboard = self.state.keyboard.lock().await;
        modifiers_mask(&keyboard.pressed_modifiers)
    }

    async fn move_locked(
        &self,
        state: &mut MouseState,
        x: f64,
        y: f64,
        steps: u32,
        modifiers: u8,
    ) -> Result<()> {
        let (from_x, from_y) = (state.x, state.y);
        state.x = x;
        state.y = y;
        for step in 1..=steps {
            let fraction = f64::from(step) / f64::from(steps);
            let current_x = from_x + (x - from_x) * fraction;
            let current_y = from_y + (y - from_y) * fraction;
            self.page
                .send_main(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mouseMoved",
                        "button": state.last_button.map_or("none", MouseButton::cdp_name),
                        "buttons": state.buttons,
                        "x": current_x,
                        "y": current_y,
                        "modifiers": modifiers,
                        "force": if state.buttons == 0 { 0.0 } else { 0.5 },
                    }),
                )
                .await?;
        }
        Ok(())
    }

    async fn down_locked(
        &self,
        state: &mut MouseState,
        button: MouseButton,
        click_count: u8,
        modifiers: u8,
    ) -> Result<()> {
        state.last_button = Some(button);
        state.buttons |= button.mask();
        self.page
            .send_main(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mousePressed",
                    "button": button.cdp_name(),
                    "buttons": state.buttons,
                    "x": state.x,
                    "y": state.y,
                    "modifiers": modifiers,
                    "clickCount": click_count,
                    "force": 0.5,
                }),
            )
            .await?;
        Ok(())
    }

    async fn up_locked(
        &self,
        state: &mut MouseState,
        button: MouseButton,
        click_count: u8,
        modifiers: u8,
    ) -> Result<()> {
        state.last_button = None;
        state.buttons &= !button.mask();
        self.page
            .send_main(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseReleased",
                    "button": button.cdp_name(),
                    "buttons": state.buttons,
                    "x": state.x,
                    "y": state.y,
                    "modifiers": modifiers,
                    "clickCount": click_count,
                }),
            )
            .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Touchscreen {
    page: Page,
    state: Arc<InputState>,
}

impl std::fmt::Debug for Touchscreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Touchscreen")
            .finish_non_exhaustive()
    }
}

impl Touchscreen {
    pub(crate) fn new(page: Page, state: Arc<InputState>) -> Self {
        Self { page, state }
    }

    pub async fn tap(&self, x: f64, y: f64) -> Result<()> {
        self.page
            .send_main(
                "Emulation.setTouchEmulationEnabled",
                json!({ "enabled": true }),
            )
            .await?;
        let modifiers = {
            let keyboard = self.state.keyboard.lock().await;
            modifiers_mask(&keyboard.pressed_modifiers)
        };
        self.page
            .send_main(
                "Input.dispatchTouchEvent",
                json!({
                    "type": "touchStart",
                    "modifiers": modifiers,
                    "touchPoints": [{ "x": x, "y": y }],
                }),
            )
            .await?;
        self.page
            .send_main(
                "Input.dispatchTouchEvent",
                json!({
                    "type": "touchEnd",
                    "modifiers": modifiers,
                    "touchPoints": [],
                }),
            )
            .await?;
        Ok(())
    }
}

fn split_key_combination(value: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut building = String::new();
    for character in value.chars() {
        if character == '+' && !building.is_empty() {
            keys.push(std::mem::take(&mut building));
        } else {
            building.push(character);
        }
    }
    keys.push(building);
    keys
}

fn modifiers_mask(modifiers: &HashSet<&'static str>) -> u8 {
    (if modifiers.contains("Alt") { 1 } else { 0 })
        | (if modifiers.contains("Control") { 2 } else { 0 })
        | (if modifiers.contains("Meta") { 4 } else { 0 })
        | (if modifiers.contains("Shift") { 8 } else { 0 })
}

fn modifier_for_key(key: &str) -> Option<&'static str> {
    match key {
        "Alt" => Some("Alt"),
        "Control" => Some("Control"),
        "Meta" => Some("Meta"),
        "Shift" => Some("Shift"),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyDescription {
    key: &'static str,
    #[allow(dead_code)]
    key_code: u16,
    key_code_without_location: u16,
    code: &'static str,
    text: &'static str,
    location: u8,
}

#[derive(Clone, Copy)]
struct KeyDefinition {
    code: &'static str,
    key_code: u16,
    key_code_without_location: Option<u16>,
    key: &'static str,
    shift_key: Option<&'static str>,
    shift_key_code: Option<u16>,
    text: Option<&'static str>,
    location: u8,
}

impl KeyDefinition {
    fn description(self, shifted: bool) -> KeyDescription {
        let mut key = self.key;
        let mut key_code = self.key_code;
        let mut text = self.text.unwrap_or_else(|| {
            if self.key.chars().count() == 1 {
                self.key
            } else {
                ""
            }
        });
        if let (true, Some(shift_key)) = (shifted, self.shift_key) {
            key = shift_key;
            text = shift_key;
            if let Some(shift_key_code) = self.shift_key_code {
                key_code = shift_key_code;
            }
        }
        KeyDescription {
            key,
            key_code,
            key_code_without_location: self.key_code_without_location.unwrap_or(self.key_code),
            code: self.code,
            text,
            location: self.location,
        }
    }
}

struct KeyLookup {
    definition: &'static KeyDefinition,
    shifted: bool,
    can_shift: bool,
}

fn resolve_smart_modifier(key: &str) -> &str {
    if key == "ControlOrMeta" {
        if cfg!(target_os = "macos") {
            "Meta"
        } else {
            "Control"
        }
    } else {
        key
    }
}

fn lookup_key(key: &str) -> Option<KeyLookup> {
    let key = resolve_smart_modifier(key);
    let alias = match key {
        "Shift" => Some("ShiftLeft"),
        "Control" => Some("ControlLeft"),
        "Alt" => Some("AltLeft"),
        "Meta" => Some("MetaLeft"),
        "\n" | "\r" => Some("Enter"),
        _ => None,
    };
    if let Some(code) = alias {
        return KEYBOARD_LAYOUT
            .iter()
            .find(|definition| definition.code == code)
            .map(|definition| KeyLookup {
                definition,
                shifted: false,
                can_shift: false,
            });
    }
    if let Some(definition) = KEYBOARD_LAYOUT
        .iter()
        .find(|definition| definition.code == key)
    {
        return Some(KeyLookup {
            definition,
            shifted: false,
            can_shift: definition.shift_key.is_some(),
        });
    }
    for definition in KEYBOARD_LAYOUT {
        if definition.location != 0 {
            continue;
        }
        if definition.key.chars().count() == 1 && definition.key == key {
            return Some(KeyLookup {
                definition,
                shifted: false,
                can_shift: false,
            });
        }
        if definition.shift_key == Some(key) {
            return Some(KeyLookup {
                definition,
                shifted: true,
                can_shift: false,
            });
        }
    }
    None
}

fn key_description(key: &str, state: &KeyboardState) -> Result<KeyDescription> {
    let Some(lookup) = lookup_key(key) else {
        return Err(Error::InvalidArgument(format!("Unknown key: \"{key}\"")));
    };
    let shifted = lookup.shifted || (lookup.can_shift && state.pressed_modifiers.contains("Shift"));
    let mut description = lookup.definition.description(shifted);
    if state.pressed_modifiers.len() > 1
        || (state.pressed_modifiers.len() == 1 && !state.pressed_modifiers.contains("Shift"))
    {
        description.text = "";
    }
    Ok(description)
}

fn commands_for_code(code: &str, modifiers: &HashSet<&'static str>) -> Vec<&'static str> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let mut shortcut = String::new();
    for modifier in ["Shift", "Control", "Alt", "Meta"] {
        if modifiers.contains(modifier) {
            shortcut.push_str(modifier);
            shortcut.push('+');
        }
    }
    shortcut.push_str(code);
    mac_commands(&shortcut)
        .iter()
        .copied()
        .filter(|command| !command.starts_with("insert"))
        .map(|command| command.strip_suffix(':').unwrap_or(command))
        .collect()
}

fn mac_commands(shortcut: &str) -> &'static [&'static str] {
    match shortcut {
        "Backspace" => &["deleteBackward:"],
        "Enter" | "NumpadEnter" => &["insertNewline:"],
        "Escape" => &["cancelOperation:"],
        "ArrowUp" => &["moveUp:"],
        "ArrowDown" => &["moveDown:"],
        "ArrowLeft" => &["moveLeft:"],
        "ArrowRight" => &["moveRight:"],
        "F5" => &["complete:"],
        "Delete" => &["deleteForward:"],
        "Home" => &["scrollToBeginningOfDocument:"],
        "End" => &["scrollToEndOfDocument:"],
        "PageUp" => &["scrollPageUp:"],
        "PageDown" => &["scrollPageDown:"],
        "Shift+Backspace" => &["deleteBackward:"],
        "Shift+Enter" | "Shift+NumpadEnter" => &["insertNewline:"],
        "Shift+Escape" => &["cancelOperation:"],
        "Shift+ArrowUp" => &["moveUpAndModifySelection:"],
        "Shift+ArrowDown" => &["moveDownAndModifySelection:"],
        "Shift+ArrowLeft" => &["moveLeftAndModifySelection:"],
        "Shift+ArrowRight" => &["moveRightAndModifySelection:"],
        "Shift+F5" => &["complete:"],
        "Shift+Delete" => &["deleteForward:"],
        "Shift+Home" => &["moveToBeginningOfDocumentAndModifySelection:"],
        "Shift+End" => &["moveToEndOfDocumentAndModifySelection:"],
        "Shift+PageUp" => &["pageUpAndModifySelection:"],
        "Shift+PageDown" => &["pageDownAndModifySelection:"],
        "Shift+Numpad5" => &["delete:"],
        "Control+Tab" => &["selectNextKeyView:"],
        "Control+Enter" | "Control+NumpadEnter" => &["insertLineBreak:"],
        "Control+Quote" => &["insertSingleQuoteIgnoringSubstitution:"],
        "Control+KeyA" => &["moveToBeginningOfParagraph:"],
        "Control+KeyB" => &["moveBackward:"],
        "Control+KeyD" => &["deleteForward:"],
        "Control+KeyE" => &["moveToEndOfParagraph:"],
        "Control+KeyF" => &["moveForward:"],
        "Control+KeyH" => &["deleteBackward:"],
        "Control+KeyK" => &["deleteToEndOfParagraph:"],
        "Control+KeyL" => &["centerSelectionInVisibleArea:"],
        "Control+KeyN" => &["moveDown:"],
        "Control+KeyO" => &["insertNewlineIgnoringFieldEditor:", "moveBackward:"],
        "Control+KeyP" => &["moveUp:"],
        "Control+KeyT" => &["transpose:"],
        "Control+KeyV" => &["pageDown:"],
        "Control+KeyY" => &["yank:"],
        "Control+Backspace" => &["deleteBackwardByDecomposingPreviousCharacter:"],
        "Control+ArrowUp" => &["scrollPageUp:"],
        "Control+ArrowDown" => &["scrollPageDown:"],
        "Control+ArrowLeft" => &["moveToLeftEndOfLine:"],
        "Control+ArrowRight" => &["moveToRightEndOfLine:"],
        "Shift+Control+Enter" | "Shift+Control+NumpadEnter" => &["insertLineBreak:"],
        "Shift+Control+Tab" => &["selectPreviousKeyView:"],
        "Shift+Control+Quote" => &["insertDoubleQuoteIgnoringSubstitution:"],
        "Shift+Control+KeyA" => &["moveToBeginningOfParagraphAndModifySelection:"],
        "Shift+Control+KeyB" => &["moveBackwardAndModifySelection:"],
        "Shift+Control+KeyE" => &["moveToEndOfParagraphAndModifySelection:"],
        "Shift+Control+KeyF" => &["moveForwardAndModifySelection:"],
        "Shift+Control+KeyN" => &["moveDownAndModifySelection:"],
        "Shift+Control+KeyP" => &["moveUpAndModifySelection:"],
        "Shift+Control+KeyV" => &["pageDownAndModifySelection:"],
        "Shift+Control+Backspace" => &["deleteBackwardByDecomposingPreviousCharacter:"],
        "Shift+Control+ArrowUp" => &["scrollPageUp:"],
        "Shift+Control+ArrowDown" => &["scrollPageDown:"],
        "Shift+Control+ArrowLeft" => &["moveToLeftEndOfLineAndModifySelection:"],
        "Shift+Control+ArrowRight" => &["moveToRightEndOfLineAndModifySelection:"],
        "Alt+Backspace" => &["deleteWordBackward:"],
        "Alt+Enter" | "Alt+NumpadEnter" => &["insertNewlineIgnoringFieldEditor:"],
        "Alt+Escape" => &["complete:"],
        "Alt+ArrowUp" => &["moveBackward:", "moveToBeginningOfParagraph:"],
        "Alt+ArrowDown" => &["moveForward:", "moveToEndOfParagraph:"],
        "Alt+ArrowLeft" => &["moveWordLeft:"],
        "Alt+ArrowRight" => &["moveWordRight:"],
        "Alt+Delete" => &["deleteWordForward:"],
        "Alt+PageUp" => &["pageUp:"],
        "Alt+PageDown" => &["pageDown:"],
        "Shift+Alt+Backspace" => &["deleteWordBackward:"],
        "Shift+Alt+Enter" | "Shift+Alt+NumpadEnter" => &["insertNewlineIgnoringFieldEditor:"],
        "Shift+Alt+Escape" => &["complete:"],
        "Shift+Alt+ArrowUp" => &["moveParagraphBackwardAndModifySelection:"],
        "Shift+Alt+ArrowDown" => &["moveParagraphForwardAndModifySelection:"],
        "Shift+Alt+ArrowLeft" => &["moveWordLeftAndModifySelection:"],
        "Shift+Alt+ArrowRight" => &["moveWordRightAndModifySelection:"],
        "Shift+Alt+Delete" => &["deleteWordForward:"],
        "Shift+Alt+PageUp" => &["pageUp:"],
        "Shift+Alt+PageDown" => &["pageDown:"],
        "Control+Alt+KeyB" => &["moveWordBackward:"],
        "Control+Alt+KeyF" => &["moveWordForward:"],
        "Control+Alt+Backspace" => &["deleteWordBackward:"],
        "Shift+Control+Alt+KeyB" => &["moveWordBackwardAndModifySelection:"],
        "Shift+Control+Alt+KeyF" => &["moveWordForwardAndModifySelection:"],
        "Shift+Control+Alt+Backspace" => &["deleteWordBackward:"],
        "Meta+NumpadSubtract" => &["cancel:"],
        "Meta+Backspace" => &["deleteToBeginningOfLine:"],
        "Meta+ArrowUp" => &["moveToBeginningOfDocument:"],
        "Meta+ArrowDown" => &["moveToEndOfDocument:"],
        "Meta+ArrowLeft" => &["moveToLeftEndOfLine:"],
        "Meta+ArrowRight" => &["moveToRightEndOfLine:"],
        "Shift+Meta+NumpadSubtract" => &["cancel:"],
        "Shift+Meta+Backspace" => &["deleteToBeginningOfLine:"],
        "Shift+Meta+ArrowUp" => &["moveToBeginningOfDocumentAndModifySelection:"],
        "Shift+Meta+ArrowDown" => &["moveToEndOfDocumentAndModifySelection:"],
        "Shift+Meta+ArrowLeft" => &["moveToLeftEndOfLineAndModifySelection:"],
        "Shift+Meta+ArrowRight" => &["moveToRightEndOfLineAndModifySelection:"],
        "Meta+KeyA" => &["selectAll:"],
        "Meta+KeyC" => &["copy:"],
        "Meta+KeyX" => &["cut:"],
        "Meta+KeyV" => &["paste:"],
        "Meta+KeyZ" => &["undo:"],
        "Shift+Meta+KeyZ" => &["redo:"],
        _ => &[],
    }
}

macro_rules! key {
    ($code:literal, $key_code:literal, $key_value:literal) => {
        KeyDefinition {
            code: $code,
            key_code: $key_code,
            key_code_without_location: None,
            key: $key_value,
            shift_key: None,
            shift_key_code: None,
            text: None,
            location: 0,
        }
    };
    ($code:literal, $key_code:literal, $key_value:literal, shift = $shift:literal) => {
        KeyDefinition {
            code: $code,
            key_code: $key_code,
            key_code_without_location: None,
            key: $key_value,
            shift_key: Some($shift),
            shift_key_code: None,
            text: None,
            location: 0,
        }
    };
}

const KEYBOARD_LAYOUT: &[KeyDefinition] = &[
    key!("Escape", 27, "Escape"),
    key!("F1", 112, "F1"),
    key!("F2", 113, "F2"),
    key!("F3", 114, "F3"),
    key!("F4", 115, "F4"),
    key!("F5", 116, "F5"),
    key!("F6", 117, "F6"),
    key!("F7", 118, "F7"),
    key!("F8", 119, "F8"),
    key!("F9", 120, "F9"),
    key!("F10", 121, "F10"),
    key!("F11", 122, "F11"),
    key!("F12", 123, "F12"),
    key!("Backquote", 192, "`", shift = "~"),
    key!("Digit1", 49, "1", shift = "!"),
    key!("Digit2", 50, "2", shift = "@"),
    key!("Digit3", 51, "3", shift = "#"),
    key!("Digit4", 52, "4", shift = "$"),
    key!("Digit5", 53, "5", shift = "%"),
    key!("Digit6", 54, "6", shift = "^"),
    key!("Digit7", 55, "7", shift = "&"),
    key!("Digit8", 56, "8", shift = "*"),
    key!("Digit9", 57, "9", shift = "("),
    key!("Digit0", 48, "0", shift = ")"),
    key!("Minus", 189, "-", shift = "_"),
    key!("Equal", 187, "=", shift = "+"),
    key!("Backslash", 220, "\\", shift = "|"),
    key!("Backspace", 8, "Backspace"),
    key!("Tab", 9, "Tab"),
    key!("KeyQ", 81, "q", shift = "Q"),
    key!("KeyW", 87, "w", shift = "W"),
    key!("KeyE", 69, "e", shift = "E"),
    key!("KeyR", 82, "r", shift = "R"),
    key!("KeyT", 84, "t", shift = "T"),
    key!("KeyY", 89, "y", shift = "Y"),
    key!("KeyU", 85, "u", shift = "U"),
    key!("KeyI", 73, "i", shift = "I"),
    key!("KeyO", 79, "o", shift = "O"),
    key!("KeyP", 80, "p", shift = "P"),
    key!("BracketLeft", 219, "[", shift = "{"),
    key!("BracketRight", 221, "]", shift = "}"),
    key!("CapsLock", 20, "CapsLock"),
    key!("KeyA", 65, "a", shift = "A"),
    key!("KeyS", 83, "s", shift = "S"),
    key!("KeyD", 68, "d", shift = "D"),
    key!("KeyF", 70, "f", shift = "F"),
    key!("KeyG", 71, "g", shift = "G"),
    key!("KeyH", 72, "h", shift = "H"),
    key!("KeyJ", 74, "j", shift = "J"),
    key!("KeyK", 75, "k", shift = "K"),
    key!("KeyL", 76, "l", shift = "L"),
    key!("Semicolon", 186, ";", shift = ":"),
    key!("Quote", 222, "'", shift = "\""),
    KeyDefinition {
        code: "Enter",
        key_code: 13,
        key_code_without_location: None,
        key: "Enter",
        shift_key: None,
        shift_key_code: None,
        text: Some("\r"),
        location: 0,
    },
    KeyDefinition {
        code: "ShiftLeft",
        key_code: 160,
        key_code_without_location: Some(16),
        key: "Shift",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 1,
    },
    key!("KeyZ", 90, "z", shift = "Z"),
    key!("KeyX", 88, "x", shift = "X"),
    key!("KeyC", 67, "c", shift = "C"),
    key!("KeyV", 86, "v", shift = "V"),
    key!("KeyB", 66, "b", shift = "B"),
    key!("KeyN", 78, "n", shift = "N"),
    key!("KeyM", 77, "m", shift = "M"),
    key!("Comma", 188, ",", shift = "<"),
    key!("Period", 190, ".", shift = ">"),
    key!("Slash", 191, "/", shift = "?"),
    KeyDefinition {
        code: "ShiftRight",
        key_code: 161,
        key_code_without_location: Some(16),
        key: "Shift",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 2,
    },
    KeyDefinition {
        code: "ControlLeft",
        key_code: 162,
        key_code_without_location: Some(17),
        key: "Control",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 1,
    },
    KeyDefinition {
        code: "MetaLeft",
        key_code: 91,
        key_code_without_location: None,
        key: "Meta",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 1,
    },
    KeyDefinition {
        code: "AltLeft",
        key_code: 164,
        key_code_without_location: Some(18),
        key: "Alt",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 1,
    },
    key!("Space", 32, " "),
    KeyDefinition {
        code: "AltRight",
        key_code: 165,
        key_code_without_location: Some(18),
        key: "Alt",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 2,
    },
    key!("AltGraph", 225, "AltGraph"),
    KeyDefinition {
        code: "MetaRight",
        key_code: 92,
        key_code_without_location: None,
        key: "Meta",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 2,
    },
    key!("ContextMenu", 93, "ContextMenu"),
    KeyDefinition {
        code: "ControlRight",
        key_code: 163,
        key_code_without_location: Some(17),
        key: "Control",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 2,
    },
    key!("PrintScreen", 44, "PrintScreen"),
    key!("ScrollLock", 145, "ScrollLock"),
    key!("Pause", 19, "Pause"),
    key!("PageUp", 33, "PageUp"),
    key!("PageDown", 34, "PageDown"),
    key!("Insert", 45, "Insert"),
    key!("Delete", 46, "Delete"),
    key!("Home", 36, "Home"),
    key!("End", 35, "End"),
    key!("ArrowLeft", 37, "ArrowLeft"),
    key!("ArrowUp", 38, "ArrowUp"),
    key!("ArrowRight", 39, "ArrowRight"),
    key!("ArrowDown", 40, "ArrowDown"),
    key!("AudioVolumeMute", 173, "AudioVolumeMute"),
    key!("AudioVolumeDown", 174, "AudioVolumeDown"),
    key!("AudioVolumeUp", 175, "AudioVolumeUp"),
    key!("MediaTrackNext", 176, "MediaTrackNext"),
    key!("MediaTrackPrevious", 177, "MediaTrackPrevious"),
    key!("MediaPlayPause", 179, "MediaPlayPause"),
    key!("NumLock", 144, "NumLock"),
    KeyDefinition {
        code: "NumpadDivide",
        key_code: 111,
        key_code_without_location: None,
        key: "/",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "NumpadMultiply",
        key_code: 106,
        key_code_without_location: None,
        key: "*",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "NumpadSubtract",
        key_code: 109,
        key_code_without_location: None,
        key: "-",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "Numpad7",
        key_code: 36,
        key_code_without_location: None,
        key: "Home",
        shift_key: Some("7"),
        shift_key_code: Some(103),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "Numpad8",
        key_code: 38,
        key_code_without_location: None,
        key: "ArrowUp",
        shift_key: Some("8"),
        shift_key_code: Some(104),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "Numpad9",
        key_code: 33,
        key_code_without_location: None,
        key: "PageUp",
        shift_key: Some("9"),
        shift_key_code: Some(105),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "Numpad4",
        key_code: 37,
        key_code_without_location: None,
        key: "ArrowLeft",
        shift_key: Some("4"),
        shift_key_code: Some(100),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "Numpad5",
        key_code: 12,
        key_code_without_location: None,
        key: "Clear",
        shift_key: Some("5"),
        shift_key_code: Some(101),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "Numpad6",
        key_code: 39,
        key_code_without_location: None,
        key: "ArrowRight",
        shift_key: Some("6"),
        shift_key_code: Some(102),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "NumpadAdd",
        key_code: 107,
        key_code_without_location: None,
        key: "+",
        shift_key: None,
        shift_key_code: None,
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "Numpad1",
        key_code: 35,
        key_code_without_location: None,
        key: "End",
        shift_key: Some("1"),
        shift_key_code: Some(97),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "Numpad2",
        key_code: 40,
        key_code_without_location: None,
        key: "ArrowDown",
        shift_key: Some("2"),
        shift_key_code: Some(98),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "Numpad3",
        key_code: 34,
        key_code_without_location: None,
        key: "PageDown",
        shift_key: Some("3"),
        shift_key_code: Some(99),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "Numpad0",
        key_code: 45,
        key_code_without_location: None,
        key: "Insert",
        shift_key: Some("0"),
        shift_key_code: Some(96),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "NumpadDecimal",
        key_code: 46,
        key_code_without_location: None,
        key: "\0",
        shift_key: Some("."),
        shift_key_code: Some(110),
        text: None,
        location: 3,
    },
    KeyDefinition {
        code: "NumpadEnter",
        key_code: 13,
        key_code_without_location: None,
        key: "Enter",
        shift_key: None,
        shift_key_code: None,
        text: Some("\r"),
        location: 3,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn state(modifiers: &[&'static str]) -> KeyboardState {
        KeyboardState {
            pressed_modifiers: modifiers.iter().copied().collect(),
            pressed_keys: HashSet::new(),
        }
    }

    #[test]
    fn describes_letters_shifted_symbols_and_aliases() {
        let plain = key_description("a", &state(&[])).unwrap();
        assert_eq!(
            (plain.key, plain.code, plain.key_code, plain.text),
            ("a", "KeyA", 65, "a")
        );

        let shifted = key_description("KeyA", &state(&["Shift"])).unwrap();
        assert_eq!(
            (shifted.key, shifted.code, shifted.key_code, shifted.text),
            ("A", "KeyA", 65, "A")
        );

        let symbol = key_description("!", &state(&[])).unwrap();
        assert_eq!(
            (symbol.key, symbol.code, symbol.key_code, symbol.text),
            ("!", "Digit1", 49, "!")
        );

        let shift = key_description("Shift", &state(&[])).unwrap();
        assert_eq!(
            (
                shift.key,
                shift.code,
                shift.key_code_without_location,
                shift.location
            ),
            ("Shift", "ShiftLeft", 16, 1)
        );
    }

    #[test]
    fn modifiers_suppress_text_except_for_shift() {
        assert_eq!(key_description("a", &state(&["Control"])).unwrap().text, "");
        assert_eq!(key_description("A", &state(&["Shift"])).unwrap().text, "A");
        assert_eq!(
            key_description("A", &state(&["Shift", "Alt"]))
                .unwrap()
                .text,
            ""
        );
    }

    #[test]
    fn describes_enter_and_keypad_location() {
        let enter = key_description("Enter", &state(&[])).unwrap();
        assert_eq!(
            (enter.key, enter.code, enter.key_code, enter.text),
            ("Enter", "Enter", 13, "\r")
        );

        let keypad = key_description("Numpad7", &state(&["Shift"])).unwrap();
        assert_eq!(
            (
                keypad.key,
                keypad.code,
                keypad.key_code,
                keypad.key_code_without_location,
                keypad.location
            ),
            ("7", "Numpad7", 103, 36, 3)
        );
    }

    #[test]
    fn splits_literal_plus_like_playwright() {
        assert_eq!(
            split_key_combination("Control+Shift+K"),
            ["Control", "Shift", "K"]
        );
        assert_eq!(split_key_combination("Control++"), ["Control", "+"]);
        assert_eq!(split_key_combination("+"), ["+"]);
    }
}
