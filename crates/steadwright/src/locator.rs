use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::aria_yaml::render_aria_snapshot_as_yaml;
use crate::selectors::{escape_for_text_selector, json_string};
use crate::{
    ActionOptions, AriaSnapshotMode, AriaSnapshotOptions, BoundingBox, ByRoleOptions, CallArg,
    ClickOptions, Deadline, ElementHandle, Error, FilePayload, Frame, InputFiles, JsHandle,
    JsValue, LocatorFilter, Page, Point, Polling, Result, ScreenshotOptions, SelectOptionValue,
    TextMatch, WaitForFunctionOptions, WaitForOptions, WaitForSelectorState, World,
    get_by_alt_text, get_by_label, get_by_placeholder, get_by_role, get_by_test_id, get_by_text,
    get_by_title,
};

const QUERY_ALL: &str = "(injected, selector) => injected.querySelectorAll(injected.parseSelector(selector), injected.document)";
const QUERY_ONE: &str = "(injected, selector, strict) => injected.querySelector(injected.parseSelector(selector), injected.document, strict)";

#[derive(Clone)]
pub struct Locator {
    pub frame: Frame,
    pub selector: String,
}

impl std::fmt::Debug for Locator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Locator")
            .field("selector", &self.selector)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct FrameLocator {
    frame: Frame,
    selector: String,
}

impl std::fmt::Debug for FrameLocator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrameLocator")
            .field("selector", &self.selector)
            .finish_non_exhaustive()
    }
}

impl Locator {
    pub(crate) fn new(frame: Frame, selector: impl Into<String>) -> Self {
        Self {
            frame,
            selector: selector.into(),
        }
    }

    pub fn locator(&self, selector: &str) -> Self {
        Self::new(
            self.frame.clone(),
            format!("{} >> {selector}", self.selector),
        )
    }

    pub fn filter(&self, options: LocatorFilter) -> Result<Self> {
        let mut selector = self.selector.clone();
        if let Some(text) = options.has_text {
            selector.push_str(" >> internal:has-text=");
            selector.push_str(&escape_for_text_selector(&text, false));
        }
        if let Some(text) = options.has_not_text {
            selector.push_str(" >> internal:has-not-text=");
            selector.push_str(&escape_for_text_selector(&text, false));
        }
        if let Some(locator) = options.has {
            self.ensure_same_frame(
                &locator,
                "Inner \"has\" locator must belong to the same frame.",
            )?;
            selector.push_str(" >> internal:has=");
            selector.push_str(&json_string(&locator.selector));
        }
        if let Some(locator) = options.has_not {
            self.ensure_same_frame(
                &locator,
                "Inner \"hasNot\" locator must belong to the same frame.",
            )?;
            selector.push_str(" >> internal:has-not=");
            selector.push_str(&json_string(&locator.selector));
        }
        if let Some(visible) = options.visible {
            selector.push_str(if visible {
                " >> visible=true"
            } else {
                " >> visible=false"
            });
        }
        Ok(Self::new(self.frame.clone(), selector))
    }

    pub fn first(&self) -> Self {
        self.locator("nth=0")
    }

    pub fn last(&self) -> Self {
        self.locator("nth=-1")
    }

    pub fn nth(&self, index: i32) -> Self {
        self.locator(&format!("nth={index}"))
    }

    pub fn and_(&self, other: &Locator) -> Result<Self> {
        self.ensure_same_frame(other, "Locators must belong to the same frame.")?;
        Ok(self.locator(&format!("internal:and={}", json_string(&other.selector))))
    }

    pub fn or_(&self, other: &Locator) -> Result<Self> {
        self.ensure_same_frame(other, "Locators must belong to the same frame.")?;
        Ok(self.locator(&format!("internal:or={}", json_string(&other.selector))))
    }

    pub fn frame_locator(&self, selector: &str) -> FrameLocator {
        FrameLocator {
            frame: self.frame.clone(),
            selector: format!("{} >> {selector}", self.selector),
        }
    }

    pub fn content_frame(&self) -> FrameLocator {
        FrameLocator {
            frame: self.frame.clone(),
            selector: self.selector.clone(),
        }
    }

    pub fn get_by_role(&self, role: &str, options: ByRoleOptions) -> Self {
        self.locator(&get_by_role(role, &options))
    }

    pub fn get_by_text(&self, text: impl Into<TextMatch>, exact: bool) -> Self {
        self.locator(&get_by_text(&text.into(), exact))
    }

    pub fn get_by_label(&self, text: impl Into<TextMatch>, exact: bool) -> Self {
        self.locator(&get_by_label(&text.into(), exact))
    }

    pub fn get_by_placeholder(&self, text: impl Into<TextMatch>, exact: bool) -> Self {
        self.locator(&get_by_placeholder(&text.into(), exact))
    }

    pub fn get_by_alt_text(&self, text: impl Into<TextMatch>, exact: bool) -> Self {
        self.locator(&get_by_alt_text(&text.into(), exact))
    }

    pub fn get_by_title(&self, text: impl Into<TextMatch>, exact: bool) -> Self {
        self.locator(&get_by_title(&text.into(), exact))
    }

    pub fn get_by_test_id(&self, text: impl Into<TextMatch>) -> Self {
        self.locator(&get_by_test_id(&text.into()))
    }

    fn ensure_same_frame(&self, other: &Locator, message: &str) -> Result<()> {
        if self.frame.same_frame(&other.frame) {
            Ok(())
        } else {
            Err(Error::InvalidArgument(message.to_owned()))
        }
    }

    pub async fn count(&self) -> Result<usize> {
        Ok(self
            .frame
            .query_selector_all_utility(&self.selector)
            .await?
            .len())
    }

    pub async fn all(&self) -> Result<Vec<Self>> {
        Ok((0..self.count().await?)
            .map(|index| self.nth(index as i32))
            .collect())
    }

    pub async fn element_handle(&self) -> Result<ElementHandle> {
        self.wait_for(WaitForOptions {
            state: WaitForSelectorState::Attached,
            timeout: None,
        })
        .await?;
        self.frame
            .query_selector(&self.selector, true)
            .await?
            .ok_or_else(|| {
                Error::Evaluation(format!(
                    "Could not resolve {} to DOM Element",
                    self.selector
                ))
            })
    }

    pub async fn element_handles(&self) -> Result<Vec<ElementHandle>> {
        self.frame.query_selector_all(&self.selector).await
    }

    pub async fn evaluate(&self, expression: &str, arg: impl Into<CallArg>) -> Result<JsValue> {
        self.element_handle()
            .await?
            .0
            .evaluate(expression, arg)
            .await
    }

    pub async fn evaluate_handle(
        &self,
        expression: &str,
        arg: impl Into<CallArg>,
    ) -> Result<JsHandle> {
        self.element_handle()
            .await?
            .0
            .evaluate_handle(expression, arg)
            .await
    }

    pub async fn evaluate_all(&self, expression: &str, arg: impl Into<CallArg>) -> Result<JsValue> {
        self.frame
            .query_array_handle(&self.selector)
            .await?
            .evaluate(expression, arg)
            .await
    }

    pub async fn all_inner_texts(&self) -> Result<Vec<String>> {
        strings(
            self.evaluate_all_utility("elements => elements.map(e => e.innerText)", Value::Null)
                .await?,
        )
    }

    pub async fn all_text_contents(&self) -> Result<Vec<String>> {
        strings(
            self.evaluate_all_utility(
                "elements => elements.map(e => e.textContent || '')",
                Value::Null,
            )
            .await?,
        )
    }

    pub async fn inner_text(&self) -> Result<String> {
        string_value(
            self.evaluate_utility(
                "element => { if (!(element instanceof HTMLElement)) throw new Error('Node is not an HTMLElement'); return element.innerText; }",
                Value::Null,
            )
            .await?,
        )
    }

    pub async fn text_content(&self) -> Result<Option<String>> {
        match self
            .evaluate_utility("element => element.textContent", Value::Null)
            .await?
        {
            JsValue::Null | JsValue::Undefined => Ok(None),
            JsValue::String(value) => Ok(Some(value)),
            value => Err(Error::Evaluation(format!("Expected string, got {value:?}"))),
        }
    }

    pub async fn inner_html(&self) -> Result<String> {
        string_value(
            self.evaluate_utility("element => element.innerHTML", Value::Null)
                .await?,
        )
    }

    pub async fn input_value(&self) -> Result<String> {
        let handle = self
            .frame
            .query_selector_utility(&self.selector, true)
            .await?
            .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
        let value = handle
            .0
            .frame()
            .call_injected(
                World::Utility,
                "(injected, node) => { const element = injected.retarget(node, 'follow-label'); if (!element || !['INPUT','TEXTAREA','SELECT'].includes(element.nodeName)) throw injected.createStacklessError('Node is not an <input>, <textarea> or <select> element'); return element.value; }",
                vec![CallArg::Handle(handle.0)],
            )
            .await?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::Evaluation("Expected input value string".into()))
    }

    pub async fn get_attribute(&self, name: &str) -> Result<Option<String>> {
        match self
            .evaluate_utility("(element, name) => element.getAttribute(name)", name)
            .await?
        {
            JsValue::Null | JsValue::Undefined => Ok(None),
            JsValue::String(value) => Ok(Some(value)),
            value => Err(Error::Evaluation(format!("Expected string, got {value:?}"))),
        }
    }

    pub async fn bounding_box(&self) -> Result<Option<BoundingBox>> {
        let Some(handle) = self
            .frame
            .query_selector_utility(&self.selector, true)
            .await?
        else {
            return Ok(None);
        };
        element_bounding_box(&handle).await
    }

    pub async fn is_visible(&self) -> Result<bool> {
        self.state_or_missing("visible", false).await
    }

    pub async fn is_hidden(&self) -> Result<bool> {
        self.state_or_missing("hidden", true).await
    }

    pub async fn is_enabled(&self) -> Result<bool> {
        self.state_or_missing("enabled", false).await
    }

    pub async fn is_disabled(&self) -> Result<bool> {
        self.state_or_missing("disabled", false).await
    }

    pub async fn is_checked(&self) -> Result<bool> {
        self.state_or_missing("checked", false).await
    }

    pub async fn is_editable(&self) -> Result<bool> {
        self.state_or_missing("editable", false).await
    }

    async fn state_or_missing(&self, state: &str, missing: bool) -> Result<bool> {
        let Some(handle) = self
            .frame
            .query_selector_utility(&self.selector, true)
            .await?
        else {
            return Ok(missing);
        };
        let value = handle
            .0
            .frame()
            .call_injected(
                World::Utility,
                "(injected, node, state) => injected.elementState(node, state).matches",
                vec![CallArg::Handle(handle.0), state.into()],
            )
            .await?;
        Ok(value.as_bool().unwrap_or(false))
    }

    async fn evaluate_utility(&self, expression: &str, arg: impl Into<CallArg>) -> Result<JsValue> {
        let handle = self
            .frame
            .query_selector_utility(&self.selector, true)
            .await?
            .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
        handle.0.evaluate(expression, arg).await
    }

    async fn evaluate_all_utility(
        &self,
        expression: &str,
        arg: impl Into<CallArg>,
    ) -> Result<JsValue> {
        self.frame
            .query_array_handle_local(&self.selector)
            .await?
            .evaluate(expression, arg)
            .await
    }

    pub async fn click(&self, options: ActionOptions) -> Result<()> {
        self.pointer_action("click", true, options).await
    }

    pub async fn dblclick(&self, mut options: ActionOptions) -> Result<()> {
        options.click_count = 2;
        self.pointer_action("dblclick", true, options).await
    }

    pub async fn hover(&self, options: ActionOptions) -> Result<()> {
        self.pointer_action("hover", false, options).await
    }

    pub async fn tap(&self, options: ActionOptions) -> Result<()> {
        self.pointer_action("tap", true, options).await
    }

    async fn pointer_action(
        &self,
        action: &str,
        wait_for_enabled: bool,
        options: ActionOptions,
    ) -> Result<()> {
        if options.click_count == 0 && action != "hover" {
            return Err(Error::InvalidArgument(
                "click_count must be greater than 0".into(),
            ));
        }
        let deadline = self.frame.page.deadline(options.timeout, false);
        let waits = [0, 20, 100, 100, 500];
        let mut attempt = 0usize;
        let mut last_reason = "waiting for element".to_owned();
        loop {
            if deadline.expired() {
                return Err(action_timeout(action, deadline, &last_reason));
            }
            let delay = waits[attempt.min(waits.len() - 1)];
            if delay != 0 {
                tokio::time::sleep(Duration::from_millis(delay).min(deadline.remaining())).await;
            }
            attempt += 1;
            let handle = match self
                .frame
                .query_selector_utility(&self.selector, true)
                .await
            {
                Ok(Some(handle)) => handle,
                Ok(None) => {
                    last_reason = "waiting for locator to resolve".into();
                    continue;
                }
                Err(Error::FrameDetached) => return Err(Error::FrameDetached),
                Err(error) => return Err(error),
            };
            match pointer_attempt(
                &handle,
                action,
                wait_for_enabled,
                &options,
                deadline,
                attempt - 1,
            )
            .await?
            {
                Attempt::Done => return Ok(()),
                Attempt::Retry(reason) => last_reason = reason,
            }
        }
    }

    pub async fn fill(&self, value: &str, options: ActionOptions) -> Result<()> {
        let deadline = self.frame.page.deadline(options.timeout, false);
        loop {
            let Some(handle) = self
                .frame
                .query_selector_utility(&self.selector, true)
                .await?
            else {
                retry_pause(deadline).await?;
                continue;
            };
            if !options.force {
                match wait_for_states(&handle, &["visible", "enabled", "editable"], deadline)
                    .await?
                {
                    Attempt::Done => {}
                    Attempt::Retry(_) => continue,
                }
            }
            let frame = handle.0.frame();
            let result = frame
                .call_injected(
                    World::Utility,
                    "(injected, node, value) => injected.fill(node, value)",
                    vec![CallArg::Handle(handle.0), value.into()],
                )
                .await?;
            match result.as_str() {
                Some("done") => return Ok(()),
                Some("needsinput") => {
                    if value.is_empty() {
                        frame.page.keyboard().press("Delete").await?;
                    } else {
                        frame.page.keyboard().insert_text(value).await?;
                    }
                    return Ok(());
                }
                Some("error:notconnected") => continue,
                Some(error) => return Err(Error::Evaluation(error.to_owned())),
                None => return Err(Error::Evaluation("Unexpected fill result".into())),
            }
        }
    }

    pub async fn clear(&self, options: ActionOptions) -> Result<()> {
        self.fill("", options).await
    }

    pub async fn focus(&self, timeout: Option<Duration>) -> Result<()> {
        self.injected_element_action(
            "locator.focus",
            timeout,
            "(injected, node) => injected.focusNode(node, true)",
            vec![],
        )
        .await
        .map(|_| ())
    }

    pub async fn blur(&self, timeout: Option<Duration>) -> Result<()> {
        self.injected_element_action(
            "locator.blur",
            timeout,
            "(injected, node) => injected.blurNode(node)",
            vec![],
        )
        .await
        .map(|_| ())
    }

    pub async fn select_text(&self, options: ActionOptions) -> Result<()> {
        let deadline = self.frame.page.deadline(options.timeout, false);
        loop {
            let Some(handle) = self
                .frame
                .query_selector_utility(&self.selector, true)
                .await?
            else {
                retry_pause(deadline).await?;
                continue;
            };
            if !options.force
                && !matches!(
                    wait_for_states(&handle, &["visible"], deadline).await?,
                    Attempt::Done
                )
            {
                continue;
            }
            let result = handle
                .0
                .frame()
                .call_injected(
                    World::Utility,
                    "(injected, node) => injected.selectText(node)",
                    vec![CallArg::Handle(handle.0)],
                )
                .await?;
            if result.as_str() == Some("error:notconnected") {
                continue;
            }
            return Ok(());
        }
    }

    async fn injected_element_action(
        &self,
        api: &str,
        timeout: Option<Duration>,
        declaration: &str,
        args: Vec<CallArg>,
    ) -> Result<Value> {
        let deadline = self.frame.page.deadline(timeout, false);
        loop {
            if deadline.expired() {
                return Err(Error::timeout(api, deadline.timeout()));
            }
            let Some(handle) = self
                .frame
                .query_selector_utility(&self.selector, true)
                .await?
            else {
                retry_pause(deadline).await?;
                continue;
            };
            let frame = handle.0.frame();
            let mut call_args = args.clone();
            call_args.insert(0, CallArg::Handle(handle.0));
            let result = frame
                .call_injected(World::Utility, declaration, call_args)
                .await?;
            if result.as_str() == Some("error:notconnected") {
                retry_pause(deadline).await?;
                continue;
            }
            return Ok(result);
        }
    }

    pub async fn type_text(&self, text: &str, delay: Option<Duration>) -> Result<()> {
        self.focus(None).await?;
        for character in text.chars() {
            self.frame
                .page
                .keyboard()
                .type_text(&character.to_string())
                .await?;
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
        }
        Ok(())
    }

    pub async fn press(&self, key: &str, delay: Option<Duration>) -> Result<()> {
        self.focus(None).await?;
        if let Some(delay) = delay {
            self.frame.page.keyboard().down(key).await?;
            tokio::time::sleep(delay).await;
            self.frame.page.keyboard().up(key).await
        } else {
            self.frame.page.keyboard().press(key).await
        }
    }

    pub async fn check(&self, options: ActionOptions) -> Result<()> {
        self.set_checked(true, options).await
    }

    pub async fn uncheck(&self, options: ActionOptions) -> Result<()> {
        self.set_checked(false, options).await
    }

    pub async fn set_checked(&self, checked: bool, options: ActionOptions) -> Result<()> {
        let state = self.checked_state().await?;
        if state.0 == checked {
            return Ok(());
        }
        if !checked && state.1 {
            return Err(Error::Evaluation("Cannot uncheck radio button. Radio buttons can only be unchecked by selecting another radio button in the same group.".into()));
        }
        self.click(options.clone()).await?;
        if options.trial {
            return Ok(());
        }
        if self.checked_state().await?.0 != checked {
            return Err(Error::Evaluation(
                "Clicking the checkbox did not change its state".into(),
            ));
        }
        Ok(())
    }

    async fn checked_state(&self) -> Result<(bool, bool)> {
        let handle = self
            .frame
            .query_selector_utility(&self.selector, true)
            .await?
            .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
        let value = handle
            .0
            .frame()
            .call_injected(
                World::Utility,
                "(injected, node) => injected.elementState(node, 'checked')",
                vec![CallArg::Handle(handle.0)],
            )
            .await?;
        if value.get("received").and_then(Value::as_str) == Some("error:notconnected") {
            return Err(Error::Evaluation(
                "Element is not attached to the DOM".into(),
            ));
        }
        Ok((
            value
                .get("matches")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            value
                .get("isRadio")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ))
    }

    pub async fn select_option(
        &self,
        values: Vec<SelectOptionValue>,
        options: ActionOptions,
    ) -> Result<Vec<String>> {
        let deadline = self.frame.page.deadline(options.timeout, false);
        loop {
            let Some(handle) = self
                .frame
                .query_selector_utility(&self.selector, true)
                .await?
            else {
                retry_pause(deadline).await?;
                continue;
            };
            if !options.force
                && !matches!(
                    wait_for_states(&handle, &["visible", "enabled"], deadline).await?,
                    Attempt::Done
                )
            {
                continue;
            }
            let mut serialized = Vec::with_capacity(values.len());
            let mut args = vec![CallArg::Handle(handle.0.clone())];
            for value in &values {
                let mut item = serde_json::Map::new();
                if let Some(string) = &value.value {
                    item.insert("value".into(), json!(string));
                }
                if let Some(label) = &value.label {
                    item.insert("label".into(), json!(label));
                }
                if let Some(index) = value.index {
                    item.insert("index".into(), json!(index));
                }
                if let Some(element) = &value.element {
                    let element = if element.0.context_id == handle.0.context_id
                        && element.0.session_id == handle.0.session_id
                    {
                        element.clone()
                    } else {
                        adopt_element_to_utility_in_frame(element, &handle.0.frame()).await?
                    };
                    item.insert("elementIndex".into(), json!(args.len() - 1));
                    args.push(CallArg::Handle(element.0));
                }
                serialized.push(Value::Object(item));
            }
            args.insert(1, Value::Array(serialized).into());
            let result = handle
                .0
                .frame()
                .call_injected(
                    World::Utility,
                    "(injected, node, values, ...elements) => injected.selectOptions(node, values.map(value => value.elementIndex === undefined ? value : elements[value.elementIndex]))",
                    args,
                )
                .await?;
            if let Some(selected) = result.as_array() {
                return Ok(selected
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect());
            }
            match result.as_str() {
                Some("error:notconnected" | "error:optionsnotfound" | "error:optionnotenabled") => {
                    retry_pause(deadline).await?;
                }
                Some(message) => return Err(Error::Evaluation(message.to_owned())),
                None => return Err(Error::Evaluation("Unexpected selectOptions result".into())),
            }
        }
    }

    pub async fn set_input_files(
        &self,
        files: impl Into<InputFiles>,
        options: ActionOptions,
    ) -> Result<()> {
        let files = files.into();
        let handle = self
            .frame
            .query_selector_utility(&self.selector, true)
            .await?
            .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
        let multiple = match &files {
            InputFiles::Paths(paths) => paths.len() > 1,
            InputFiles::Payloads(payloads) => payloads.len() > 1,
        };
        let retargeted = handle
            .0
            .frame()
            .call_injected_handle(
                World::Utility,
                "(injected, node, multiple) => { const element = injected.retarget(node, 'follow-label'); if (!element || !element.isConnected) return null; if (element.tagName !== 'INPUT') throw injected.createStacklessError('Node is not an HTMLInputElement'); if (multiple && !element.multiple && !element.webkitdirectory) throw injected.createStacklessError('Non-multiple file input can only accept single file'); return element; }",
                vec![CallArg::Handle(handle.0), multiple.into()],
            )
            .await?;
        let element = retargeted
            .as_element()
            .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
        match files {
            InputFiles::Paths(paths) => set_file_paths(&element, paths).await,
            InputFiles::Payloads(payloads) => set_file_payloads(&element, payloads).await,
        }?;
        let _ = options;
        Ok(())
    }

    pub async fn dispatch_event(&self, type_: &str, init: Value) -> Result<()> {
        self.injected_element_action(
            "locator.dispatch_event",
            None,
            "(injected, node, type, init) => { injected.dispatchEvent(node, type, init); return 'done'; }",
            vec![type_.into(), init.into()],
        )
        .await?;
        Ok(())
    }

    pub async fn scroll_into_view_if_needed(&self, timeout: Option<Duration>) -> Result<()> {
        let deadline = self.frame.page.deadline(timeout, false);
        loop {
            let Some(handle) = self
                .frame
                .query_selector_utility(&self.selector, true)
                .await?
            else {
                retry_pause(deadline).await?;
                continue;
            };
            if !matches!(
                wait_for_states(&handle, &["stable"], deadline).await?,
                Attempt::Done
            ) {
                continue;
            }
            match scroll_element(&handle, 0).await? {
                Attempt::Done => return Ok(()),
                Attempt::Retry(_) => retry_pause(deadline).await?,
            }
        }
    }

    pub async fn drag_to(&self, target: &Locator, options: ActionOptions) -> Result<()> {
        self.ensure_same_frame(target, "Locators must belong to the same frame.")?;
        let source = self.action_point(true, &options).await?;
        let target_point = target.action_point(false, &options).await?;
        let mouse = self.frame.page.mouse();
        press_modifiers(&self.frame.page, &options.modifiers).await?;
        let result = async {
            mouse.move_to(source.x, source.y, 1).await?;
            mouse.down(options.button, 1).await?;
            mouse.move_to(target_point.x, target_point.y, 5).await?;
            mouse.up(options.button, 1).await
        }
        .await;
        release_modifiers(&self.frame.page, &options.modifiers).await?;
        result
    }

    async fn action_point(&self, enabled: bool, options: &ActionOptions) -> Result<Point> {
        let deadline = self.frame.page.deadline(options.timeout, false);
        let mut attempt = 0usize;
        loop {
            let Some(handle) = self
                .frame
                .query_selector_utility(&self.selector, true)
                .await?
            else {
                retry_pause(deadline).await?;
                continue;
            };
            let states = if enabled {
                &["visible", "enabled", "stable"][..]
            } else {
                &["visible", "stable"][..]
            };
            if !options.force
                && !matches!(
                    wait_for_states(&handle, states, deadline).await?,
                    Attempt::Done
                )
            {
                continue;
            }
            if matches!(scroll_element(&handle, attempt).await?, Attempt::Retry(_)) {
                retry_pause(deadline).await?;
                attempt += 1;
                continue;
            }
            attempt += 1;
            match clickable_point(&handle, options.position).await? {
                Ok(point) => return Ok(point.root),
                Err(_) => retry_pause(deadline).await?,
            }
        }
    }

    pub async fn wait_for(&self, options: WaitForOptions) -> Result<()> {
        self.frame
            .wait_for_selector_internal(&self.selector, options, true)
            .await
            .map(|_| ())
    }

    pub async fn aria_snapshot(&self, mut options: AriaSnapshotOptions) -> Result<String> {
        options.selector = Some(self.selector.clone());
        self.frame.aria_snapshot(options).await
    }

    pub async fn highlight(&self) -> Result<()> {
        let handle = self
            .frame
            .query_selector_utility(&self.selector, true)
            .await?
            .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
        handle
            .0
            .frame()
            .call_injected(
                World::Utility,
                "(injected, node) => { injected.setHighlights([{ element: node, color: 'rgba(255, 0, 0, 0.5)' }]); }",
                vec![CallArg::Handle(handle.0)],
            )
            .await?;
        Ok(())
    }

    pub async fn describe(&self) -> Result<String> {
        let handle = self
            .frame
            .query_selector_utility(&self.selector, true)
            .await?
            .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
        let value = handle
            .0
            .frame()
            .call_injected(
                World::Utility,
                "(injected, node) => injected.generateSelector(node, { testIdAttributeName: 'data-testid' }).selector",
                vec![CallArg::Handle(handle.0)],
            )
            .await?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::Evaluation("generateSelector did not return a selector".into()))
    }

    pub async fn screenshot(&self, mut options: ScreenshotOptions) -> Result<Vec<u8>> {
        self.scroll_into_view_if_needed(options.timeout).await?;
        let box_ = self
            .bounding_box()
            .await?
            .ok_or_else(|| Error::Evaluation("Element is not visible".into()))?;
        options.clip = Some(crate::ScreenshotClip {
            x: box_.x,
            y: box_.y,
            width: box_.width,
            height: box_.height,
        });
        self.frame.page.screenshot(options).await
    }
}

impl FrameLocator {
    fn child_selector(&self, selector: &str) -> String {
        format!(
            "{} >> internal:control=enter-frame >> {selector}",
            self.selector
        )
    }

    pub fn locator(&self, selector: &str) -> Locator {
        Locator::new(self.frame.clone(), self.child_selector(selector))
    }

    pub fn frame_locator(&self, selector: &str) -> Self {
        Self {
            frame: self.frame.clone(),
            selector: self.child_selector(selector),
        }
    }

    pub fn get_by_role(&self, role: &str, options: ByRoleOptions) -> Locator {
        self.locator(&get_by_role(role, &options))
    }

    pub fn get_by_text(&self, text: impl Into<TextMatch>, exact: bool) -> Locator {
        self.locator(&get_by_text(&text.into(), exact))
    }

    pub fn get_by_label(&self, text: impl Into<TextMatch>, exact: bool) -> Locator {
        self.locator(&get_by_label(&text.into(), exact))
    }

    pub fn get_by_placeholder(&self, text: impl Into<TextMatch>, exact: bool) -> Locator {
        self.locator(&get_by_placeholder(&text.into(), exact))
    }

    pub fn get_by_alt_text(&self, text: impl Into<TextMatch>, exact: bool) -> Locator {
        self.locator(&get_by_alt_text(&text.into(), exact))
    }

    pub fn get_by_title(&self, text: impl Into<TextMatch>, exact: bool) -> Locator {
        self.locator(&get_by_title(&text.into(), exact))
    }

    pub fn get_by_test_id(&self, text: impl Into<TextMatch>) -> Locator {
        self.locator(&get_by_test_id(&text.into()))
    }
}

macro_rules! locator_surface {
    ($type:ty, $frame:expr) => {
        impl $type {
            pub fn locator(&self, selector: &str) -> Locator {
                Locator::new($frame(self), selector)
            }

            pub fn frame_locator(&self, selector: &str) -> FrameLocator {
                FrameLocator {
                    frame: $frame(self),
                    selector: selector.to_owned(),
                }
            }

            pub fn get_by_role(&self, role: &str, options: ByRoleOptions) -> Locator {
                self.locator(&get_by_role(role, &options))
            }

            pub fn get_by_text(&self, text: impl Into<TextMatch>, exact: bool) -> Locator {
                self.locator(&get_by_text(&text.into(), exact))
            }

            pub fn get_by_label(&self, text: impl Into<TextMatch>, exact: bool) -> Locator {
                self.locator(&get_by_label(&text.into(), exact))
            }

            pub fn get_by_placeholder(&self, text: impl Into<TextMatch>, exact: bool) -> Locator {
                self.locator(&get_by_placeholder(&text.into(), exact))
            }

            pub fn get_by_alt_text(&self, text: impl Into<TextMatch>, exact: bool) -> Locator {
                self.locator(&get_by_alt_text(&text.into(), exact))
            }

            pub fn get_by_title(&self, text: impl Into<TextMatch>, exact: bool) -> Locator {
                self.locator(&get_by_title(&text.into(), exact))
            }

            pub fn get_by_test_id(&self, text: impl Into<TextMatch>) -> Locator {
                self.locator(&get_by_test_id(&text.into()))
            }
        }
    };
}

locator_surface!(Page, |page: &Page| page.main_frame());
locator_surface!(Frame, |frame: &Frame| frame.clone());

impl Page {
    pub async fn wait_for_selector(
        &self,
        selector: &str,
        options: WaitForOptions,
    ) -> Result<Option<ElementHandle>> {
        self.main_frame().wait_for_selector(selector, options).await
    }

    pub async fn wait_for_function(
        &self,
        expression: &str,
        arg: impl Into<CallArg>,
        options: WaitForFunctionOptions,
    ) -> Result<JsHandle> {
        self.main_frame()
            .wait_for_function(expression, arg, options)
            .await
    }

    pub async fn aria_snapshot(&self, options: AriaSnapshotOptions) -> Result<String> {
        self.main_frame().aria_snapshot(options).await
    }
}

impl Frame {
    pub async fn query_selector_all(&self, selector: &str) -> Result<Vec<ElementHandle>> {
        let handles = self.query_selector_all_utility(selector).await?;
        let mut adopted = Vec::with_capacity(handles.len());
        for handle in handles {
            adopted.push(adopt_element_to_main(&handle).await?);
        }
        Ok(adopted)
    }

    async fn query_selector_all_utility(&self, selector: &str) -> Result<Vec<ElementHandle>> {
        let (frame, selector) = self.resolve_selector_frame(selector, false).await?;
        let array = frame.query_array_handle_local(&selector).await?;
        let object_id = array
            .object_id()
            .ok_or_else(|| Error::Evaluation("querySelectorAll did not return an array".into()))?;
        let properties = frame
            .page
            .send_session(
                &array.session_id,
                "Runtime.getProperties",
                json!({"objectId":object_id,"ownProperties":true}),
            )
            .await?;
        let mut indexed = Vec::new();
        for property in properties
            .get("result")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(index) = property
                .get("name")
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<usize>().ok())
            else {
                continue;
            };
            let Some(remote) = property.get("value") else {
                continue;
            };
            if remote.get("objectId").is_none() {
                continue;
            }
            indexed.push((
                index,
                ElementHandle(frame.make_handle(
                    remote.clone(),
                    array.session_id.clone(),
                    array.context_id,
                )),
            ));
        }
        indexed.sort_by_key(|entry| entry.0);
        let _ = array.dispose().await;
        Ok(indexed.into_iter().map(|entry| entry.1).collect())
    }

    pub async fn query_selector(
        &self,
        selector: &str,
        strict: bool,
    ) -> Result<Option<ElementHandle>> {
        let handle = self.query_selector_utility(selector, strict).await?;
        match handle {
            Some(handle) => adopt_element_to_main(&handle).await.map(Some),
            None => Ok(None),
        }
    }

    async fn query_selector_utility(
        &self,
        selector: &str,
        strict: bool,
    ) -> Result<Option<ElementHandle>> {
        let (frame, selector) = self.resolve_selector_frame(selector, strict).await?;
        let handle = frame
            .call_injected_handle(
                World::Utility,
                QUERY_ONE,
                vec![selector.into(), strict.into()],
            )
            .await?;
        Ok(handle.as_element())
    }

    async fn query_array_handle(&self, selector: &str) -> Result<JsHandle> {
        let (frame, selector) = self.resolve_selector_frame(selector, false).await?;
        frame
            .call_injected_handle(World::Main, QUERY_ALL, vec![selector.into()])
            .await
    }

    async fn query_array_handle_local(&self, selector: &str) -> Result<JsHandle> {
        self.call_injected_handle(World::Utility, QUERY_ALL, vec![selector.into()])
            .await
    }

    async fn resolve_selector_frame(
        &self,
        selector: &str,
        strict: bool,
    ) -> Result<(Frame, String)> {
        let chunks = split_selector_by_frame(selector)?;
        let mut frame = route_aria_frame(self, &chunks[0])?;
        for chunk in chunks.iter().take(chunks.len() - 1) {
            frame = route_aria_frame(&frame, chunk)?;
            let handle = frame
                .call_injected_handle(
                    World::Utility,
                    "(injected, selector, strict) => { const element = injected.querySelector(injected.parseSelector(selector), injected.document, strict); if (element && element.nodeName !== 'IFRAME' && element.nodeName !== 'FRAME') throw injected.createStacklessError(`Selector \"${selector}\" resolved to ${injected.previewNode(element)}, <iframe> was expected`); return element; }",
                    vec![chunk.as_str().into(), strict.into()],
                )
                .await?;
            let element = handle.as_element().ok_or_else(|| {
                Error::Evaluation(format!("Selector \"{chunk}\" did not resolve to an iframe"))
            })?;
            frame = content_frame(&element).await?.ok_or_else(|| {
                Error::Evaluation(format!("Selector \"{chunk}\" did not resolve to an iframe"))
            })?;
        }
        let last = chunks.last().cloned().expect("selector has a chunk");
        frame = route_aria_frame(&frame, &last)?;
        Ok((frame, last))
    }

    pub async fn wait_for_selector(
        &self,
        selector: &str,
        options: WaitForOptions,
    ) -> Result<Option<ElementHandle>> {
        self.wait_for_selector_internal(selector, options, false)
            .await
    }

    async fn wait_for_selector_internal(
        &self,
        selector: &str,
        options: WaitForOptions,
        strict: bool,
    ) -> Result<Option<ElementHandle>> {
        let deadline = self.page.deadline(options.timeout, false);
        loop {
            let (frame, final_selector) = self.resolve_selector_frame(selector, false).await?;
            let budget = deadline
                .remaining()
                .min(Duration::from_millis(500))
                .as_millis() as u64;
            let state = match options.state {
                WaitForSelectorState::Attached => "attached",
                WaitForSelectorState::Detached => "detached",
                WaitForSelectorState::Visible => "visible",
                WaitForSelectorState::Hidden => "hidden",
            };
            let result = frame
                .call_injected(
                    World::Utility,
                    // rAF is intentional: headless Chromium continues ticking it for these tests,
                    // while a backgrounded headed tab may suspend polling until foregrounded.
                    "async (injected, selector, state, budget, strict) => { const parsed = injected.parseSelector(selector); const start = performance.now(); while (true) { const elements = injected.querySelectorAll(parsed, injected.document); if (strict && elements.length > 1) throw injected.strictModeViolationError(parsed, elements); const element = elements[0]; const visible = element ? injected.elementState(element, 'visible').matches : false; const attached = !!element; const matches = state === 'attached' ? attached : state === 'detached' ? !attached : state === 'visible' ? visible : !attached || !visible; if (matches) return { matches: true, attached }; if (performance.now() - start >= budget) return { matches: false }; await new Promise(resolve => injected.utils.builtins.requestAnimationFrame(resolve)); } }",
                    vec![
                        final_selector.clone().into(),
                        state.into(),
                        budget.into(),
                        strict.into(),
                    ],
                )
                .await?;
            if result.get("matches").and_then(Value::as_bool) == Some(true) {
                if matches!(
                    options.state,
                    WaitForSelectorState::Detached | WaitForSelectorState::Hidden
                ) && result.get("attached").and_then(Value::as_bool) != Some(true)
                {
                    return Ok(None);
                }
                return frame.query_selector(&final_selector, strict).await;
            }
            if deadline.expired() {
                return Err(Error::timeout(
                    "frame.wait_for_selector",
                    deadline.timeout(),
                ));
            }
        }
    }

    pub async fn wait_for_function(
        &self,
        expression: &str,
        arg: impl Into<CallArg>,
        options: WaitForFunctionOptions,
    ) -> Result<JsHandle> {
        let deadline = self.page.deadline(options.timeout, false);
        let poll = match options.polling {
            Polling::Raf => Value::Null,
            Polling::Interval(duration) => json!(duration.as_millis() as u64),
        };
        let controller = self
            .evaluate_handle_args(
                "(params, arg) => { let aborted = false; const predicate = globalThis.eval(params.expression); const fn = typeof predicate === 'function' ? predicate : () => predicate; const result = (async () => { while (!aborted) { const value = await fn(arg); if (value) return value; await new Promise(resolve => params.poll === null ? requestAnimationFrame(resolve) : setTimeout(resolve, params.poll)); } return new Promise(() => {}); })(); return { result, abort: () => { aborted = true; } }; }",
                vec![json!({"expression": expression, "poll": poll}).into(), arg.into()],
            )
            .await?;
        let outcome = deadline
            .run(
                "frame.wait_for_function",
                controller.evaluate_handle("controller => controller.result", Value::Null),
            )
            .await;
        if outcome.is_err() {
            let _ = controller
                .evaluate("controller => controller.abort()", Value::Null)
                .await;
        }
        let _ = controller.dispose().await;
        outcome
    }

    pub async fn aria_snapshot(&self, options: AriaSnapshotOptions) -> Result<String> {
        let selector = options
            .selector
            .clone()
            .unwrap_or_else(|| "body,frameset".into());
        let deadline = self.page.deadline(options.timeout, false);
        let snapshot = aria_snapshot_json(self, &selector, &options, deadline).await?;
        let rendered = if options.interactive {
            let (json, kept, total) = filter_interactive_snapshot(&snapshot.json);
            let title = self.page.title().await?;
            let header = interactive_snapshot_header(&title, &self.page.url(), kept, total);
            let yaml = render_aria_snapshot_as_yaml(&json);
            if yaml.is_empty() {
                header
            } else {
                format!("{header}\n{yaml}")
            }
        } else {
            render_aria_snapshot_as_yaml(&snapshot.json)
        };
        let previous = self.page.replace_aria_snapshot(
            &snapshot.frame_id,
            options.interactive,
            rendered.clone(),
        );
        if options.diff {
            Ok(previous
                .map(|previous| render_snapshot_diff(&previous, &rendered))
                .unwrap_or(rendered))
        } else {
            Ok(rendered)
        }
    }
}

impl JsHandle {
    pub(crate) fn frame(&self) -> Frame {
        Frame {
            page: self.page.clone(),
            id: self.frame_id.clone(),
        }
    }
}

enum Attempt {
    Done,
    Retry(String),
}

struct ActionPoint {
    root: Point,
    local: Point,
}

async fn pointer_attempt(
    handle: &ElementHandle,
    action: &str,
    wait_for_enabled: bool,
    options: &ActionOptions,
    deadline: Deadline,
    attempt: usize,
) -> Result<Attempt> {
    if !options.force {
        let states = if wait_for_enabled {
            &["visible", "enabled", "stable"][..]
        } else {
            &["visible", "stable"][..]
        };
        if let Attempt::Retry(reason) = wait_for_states(handle, states, deadline).await? {
            return Ok(Attempt::Retry(reason));
        }
    }
    if let Attempt::Retry(reason) = scroll_element(handle, attempt).await? {
        if options.force {
            return Err(Error::Evaluation(force_error(&reason).into()));
        }
        return Ok(Attempt::Retry(reason));
    }
    let point = match clickable_point(handle, options.position).await? {
        Ok(point) => point,
        Err(reason) if options.force => return Err(Error::Evaluation(force_error(&reason).into())),
        Err(reason) => return Ok(Attempt::Retry(reason)),
    };
    let frame = handle.0.frame();
    let mut interceptor = None;
    if !options.force {
        if let Some(reason) = check_parent_frame_hit_targets(&frame, point.root).await? {
            return Ok(Attempt::Retry(format!(
                "{reason} intercepts pointer events"
            )));
        }
        let result = frame
            .call_injected_handle(
                World::Utility,
                "(injected, node, action, point, trial) => injected.setupHitTargetInterceptor(node, action, point, trial)",
                vec![
                    CallArg::Handle(handle.0.clone()),
                    (if action == "hover" { "hover" } else if action == "tap" { "tap" } else { "mouse" }).into(),
                    json!({"x":point.local.x,"y":point.local.y}).into(),
                    options.trial.into(),
                ],
            )
            .await?;
        if result.object_id().is_none() {
            let value = result.json_value().await?;
            if let JsValue::String(reason) = value {
                if reason == "error:notconnected" {
                    return Ok(Attempt::Retry("Element is not attached to the DOM".into()));
                }
                return Ok(Attempt::Retry(format!(
                    "{reason} intercepts pointer events"
                )));
            }
        } else {
            interceptor = Some(result);
        }
    }

    press_modifiers(&frame.page, &options.modifiers).await?;
    let navigation_before = frame.page.navigation_generation();
    let dispatch = if options.trial {
        Ok(())
    } else if action == "hover" {
        frame
            .page
            .mouse()
            .move_to(point.root.x, point.root.y, 1)
            .await
    } else if action == "tap" {
        frame
            .page
            .touchscreen()
            .tap(point.root.x, point.root.y)
            .await
    } else {
        frame
            .page
            .mouse()
            .click(
                point.root.x,
                point.root.y,
                ClickOptions {
                    button: options.button,
                    click_count: options.click_count,
                    delay: options.delay,
                    steps: 1,
                },
            )
            .await
    };
    let restore = release_modifiers(&frame.page, &options.modifiers).await;
    dispatch?;
    restore?;

    if let Some(interceptor) = interceptor {
        match interceptor
            .evaluate("handle => handle.stop()", Value::Null)
            .await
        {
            Err(_) if frame.page.navigation_generation() > navigation_before => {}
            Err(error) => return Err(error),
            Ok(result) => match result {
                JsValue::String(value) if value == "done" => {}
                JsValue::String(reason) => {
                    return Ok(Attempt::Retry(format!(
                        "{reason} intercepts pointer events"
                    )));
                }
                JsValue::Object(entries) => {
                    if let Some(JsValue::String(reason)) = entries
                        .iter()
                        .find_map(|(name, value)| (name == "hitTargetDescription").then_some(value))
                    {
                        return Ok(Attempt::Retry(format!(
                            "{reason} intercepts pointer events"
                        )));
                    }
                }
                _ => {}
            },
        }
    }
    if !options.no_wait_after && !options.trial && action == "click" {
        frame
            .page
            .wait_for_action_navigation(navigation_before, deadline)
            .await?;
    }
    Ok(Attempt::Done)
}

async fn wait_for_states(
    handle: &ElementHandle,
    states: &[&str],
    deadline: Deadline,
) -> Result<Attempt> {
    let budget = deadline
        .remaining()
        .min(Duration::from_millis(500))
        .as_millis() as u64;
    let result = handle
        .0
        .frame()
        .call_injected(
            World::Utility,
            "async (injected, node, states, budget) => { const start = performance.now(); let last; while (true) { last = await injected.checkElementStates(node, states); if (!last) return 'done'; if (last === 'error:notconnected' || performance.now() - start >= budget) return last; await new Promise(resolve => injected.utils.builtins.requestAnimationFrame(resolve)); } }",
            vec![
                CallArg::Handle(handle.0.clone()),
                json!(states).into(),
                budget.into(),
            ],
        )
        .await?;
    if result.as_str() == Some("done") {
        return Ok(Attempt::Done);
    }
    if result.as_str() == Some("error:notconnected") {
        return Ok(Attempt::Retry("Element is not attached to the DOM".into()));
    }
    let missing = result
        .get("missingState")
        .and_then(Value::as_str)
        .unwrap_or("actionable");
    Ok(Attempt::Retry(format!("element is not {missing}")))
}

async fn scroll_element(handle: &ElementHandle, attempt: usize) -> Result<Attempt> {
    let object = handle
        .0
        .object_id()
        .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
    if attempt % 4 == 0 {
        let result = handle
            .0
            .page
            .send_session(
                &handle.0.session_id,
                "DOM.scrollIntoViewIfNeeded",
                json!({"objectId":object}),
            )
            .await;
        if let Err(error) = result {
            let message = error.to_string();
            if message.contains("layout object") {
                return Ok(Attempt::Retry("Element is not visible".into()));
            }
            if message.contains("detached") || message.contains("Could not find node") {
                return Ok(Attempt::Retry("Element is not attached to the DOM".into()));
            }
            return Err(error);
        }
    } else {
        let alignments = [
            json!({"block":"end","inline":"end"}),
            json!({"block":"center","inline":"center"}),
            json!({"block":"start","inline":"start"}),
        ];
        handle
            .0
            .frame()
            .call_injected(
                World::Utility,
                "(injected, node, options) => { if (!node.isConnected) return 'error:notconnected'; if (node.nodeType === 1) node.scrollIntoView(options); return 'done'; }",
                vec![
                    CallArg::Handle(handle.0.clone()),
                    alignments[(attempt - 1) % alignments.len()].clone().into(),
                ],
            )
            .await?;
    }
    Ok(Attempt::Done)
}

async fn clickable_point(
    handle: &ElementHandle,
    position: Option<Point>,
) -> Result<std::result::Result<ActionPoint, String>> {
    let frame = handle.0.frame();
    let session_offset = session_root_offset(&frame).await?;
    let viewport = frame
        .page
        .main_frame()
        .evaluate_utility_json(
            "() => ({width: innerWidth, height: innerHeight})",
            Value::Null,
        )
        .await?;
    let viewport_width = viewport.get("width").and_then(Value::as_f64).unwrap_or(0.0);
    let viewport_height = viewport
        .get("height")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let root = if let Some(position) = position {
        let Some(box_) = raw_bounding_box(handle).await? else {
            return Ok(Err("Element is not visible".into()));
        };
        let border = frame
            .call_injected(
                World::Utility,
                "(injected, node) => injected.getElementBorderWidth(node)",
                vec![CallArg::Handle(handle.0.clone())],
            )
            .await?;
        Point {
            x: session_offset.x
                + box_.x
                + border.get("left").and_then(Value::as_f64).unwrap_or(0.0)
                + position.x,
            y: session_offset.y
                + box_.y
                + border.get("top").and_then(Value::as_f64).unwrap_or(0.0)
                + position.y,
        }
    } else {
        let object = handle
            .0
            .object_id()
            .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
        let quads = handle
            .0
            .page
            .send_session(
                &handle.0.session_id,
                "DOM.getContentQuads",
                json!({"objectId":object}),
            )
            .await;
        let quads = match quads {
            Ok(value) => value,
            Err(error) if error.to_string().contains("layout object") => {
                return Ok(Err("Element is not visible".into()));
            }
            Err(error) => return Err(error),
        };
        let mut selected = None;
        for quad in quads
            .get("quads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_array)
        {
            if quad.len() < 8 {
                continue;
            }
            let mut points = Vec::with_capacity(4);
            for index in (0..8).step_by(2) {
                points.push(Point {
                    x: (quad[index].as_f64().unwrap_or(0.0) + session_offset.x)
                        .clamp(0.0, viewport_width),
                    y: (quad[index + 1].as_f64().unwrap_or(0.0) + session_offset.y)
                        .clamp(0.0, viewport_height),
                });
            }
            if quad_area(&points) > 0.99 {
                selected = Some(Point {
                    x: points.iter().map(|point| point.x).sum::<f64>() / 4.0,
                    y: points.iter().map(|point| point.y).sum::<f64>() / 4.0,
                });
                break;
            }
        }
        let Some(point) = selected else {
            return Ok(Err("Element is outside of the viewport".into()));
        };
        point
    };
    if root.x < 0.0 || root.y < 0.0 || root.x > viewport_width || root.y > viewport_height {
        return Ok(Err("Element is outside of the viewport".into()));
    }
    let origin = document_root_offset(&frame).await?;
    Ok(Ok(ActionPoint {
        root: Point {
            x: root.x.round(),
            y: root.y.round(),
        },
        local: Point {
            x: (root.x - origin.x).round(),
            y: (root.y - origin.y).round(),
        },
    }))
}

fn quad_area(points: &[Point]) -> f64 {
    let mut area = 0.0;
    for index in 0..points.len() {
        let current = points[index];
        let next = points[(index + 1) % points.len()];
        area += (current.x * next.y - next.x * current.y) / 2.0;
    }
    area.abs()
}

async fn raw_bounding_box(handle: &ElementHandle) -> Result<Option<BoundingBox>> {
    let Some(object) = handle.0.object_id() else {
        return Ok(None);
    };
    let result = handle
        .0
        .page
        .send_session(
            &handle.0.session_id,
            "DOM.getBoxModel",
            json!({"objectId":object}),
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) if error.to_string().contains("layout object") => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(quad) = result
        .get("model")
        .and_then(|model| model.get("border"))
        .and_then(Value::as_array)
    else {
        return Ok(None);
    };
    Ok(quad_box(quad))
}

async fn adopt_element_to_main(handle: &ElementHandle) -> Result<ElementHandle> {
    let object = handle
        .0
        .object_id()
        .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
    let frame = handle.0.frame();
    let described = handle
        .0
        .page
        .send_session(
            &handle.0.session_id,
            "DOM.describeNode",
            json!({"objectId":object}),
        )
        .await?;
    let backend_node_id = described
        .get("node")
        .and_then(|node| node.get("backendNodeId"))
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            Error::Evaluation("Unable to adopt element handle from a different document".into())
        })?;
    let context = frame.context(World::Main).await?;
    let session = frame.owner_session()?;
    let resolved = frame
        .page
        .send_session(
            &session,
            "DOM.resolveNode",
            json!({"backendNodeId":backend_node_id,"executionContextId":context}),
        )
        .await?;
    let remote = resolved
        .get("object")
        .filter(|object| object.get("subtype").and_then(Value::as_str) != Some("null"))
        .cloned()
        .ok_or_else(|| {
            Error::Evaluation("Unable to adopt element handle from a different document".into())
        })?;
    let adopted = ElementHandle(frame.make_handle(remote, session, context));
    let _ = handle.0.dispose().await;
    Ok(adopted)
}

async fn adopt_element_to_utility_in_frame(
    handle: &ElementHandle,
    target_frame: &Frame,
) -> Result<ElementHandle> {
    let object = handle
        .0
        .object_id()
        .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
    let described = handle
        .0
        .page
        .send_session(
            &handle.0.session_id,
            "DOM.describeNode",
            json!({"objectId":object}),
        )
        .await?;
    let backend_node_id = described
        .get("node")
        .and_then(|node| node.get("backendNodeId"))
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            Error::Evaluation("Unable to adopt element handle from a different document".into())
        })?;
    let context = target_frame.context(World::Utility).await?;
    let session = target_frame.owner_session()?;
    let resolved = target_frame
        .page
        .send_session(
            &session,
            "DOM.resolveNode",
            json!({"backendNodeId":backend_node_id,"executionContextId":context}),
        )
        .await?;
    let remote = resolved
        .get("object")
        .filter(|object| object.get("subtype").and_then(Value::as_str) != Some("null"))
        .cloned()
        .ok_or_else(|| {
            Error::Evaluation("Unable to adopt element handle from a different document".into())
        })?;
    Ok(ElementHandle(
        target_frame.make_handle(remote, session, context),
    ))
}

async fn element_bounding_box(handle: &ElementHandle) -> Result<Option<BoundingBox>> {
    let Some(mut box_) = raw_bounding_box(handle).await? else {
        return Ok(None);
    };
    let offset = session_root_offset(&handle.0.frame()).await?;
    box_.x += offset.x;
    box_.y += offset.y;
    Ok(Some(box_))
}

fn quad_box(quad: &[Value]) -> Option<BoundingBox> {
    if quad.len() < 8 {
        return None;
    }
    let xs: Vec<f64> = quad.iter().step_by(2).filter_map(Value::as_f64).collect();
    let ys: Vec<f64> = quad
        .iter()
        .skip(1)
        .step_by(2)
        .filter_map(Value::as_f64)
        .collect();
    let x = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let y = ys.iter().copied().fold(f64::INFINITY, f64::min);
    Some(BoundingBox {
        x,
        y,
        width: xs.iter().copied().fold(f64::NEG_INFINITY, f64::max) - x,
        height: ys.iter().copied().fold(f64::NEG_INFINITY, f64::max) - y,
    })
}

async fn content_frame(element: &ElementHandle) -> Result<Option<Frame>> {
    let object = element
        .0
        .object_id()
        .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
    let result = element
        .0
        .page
        .send_session(
            &element.0.session_id,
            "DOM.describeNode",
            json!({"objectId":object}),
        )
        .await?;
    let frame_id = result
        .get("node")
        .and_then(|node| node.get("frameId"))
        .and_then(Value::as_str);
    Ok(frame_id.and_then(|id| element.0.frame().frame_by_id(id)))
}

async fn frame_element(frame: &Frame) -> Result<Option<ElementHandle>> {
    let Some(parent) = frame.parent_frame() else {
        return Ok(None);
    };
    for element in parent.query_selector_all_utility("iframe,frame").await? {
        if content_frame(&element)
            .await?
            .is_some_and(|candidate| candidate.same_frame(frame))
        {
            return Ok(Some(element));
        }
    }
    Ok(None)
}

async fn session_root_offset(frame: &Frame) -> Result<Point> {
    let session = frame.owner_session()?;
    if session == frame.page.main_session_id() {
        return Ok(Point::default());
    }
    let mut boundary = frame.clone();
    while let Some(parent) = boundary.parent_frame() {
        if parent.owner_session()? != session {
            let element = frame_element(&boundary)
                .await?
                .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
            let parent_offset = Box::pin(session_root_offset(&parent)).await?;
            let box_ = raw_bounding_box(&element)
                .await?
                .ok_or_else(|| Error::Evaluation("Element is not visible".into()))?;
            let style = parent
                .call_injected(
                    World::Utility,
                    "(injected, iframe) => injected.describeIFrameStyle(iframe)",
                    vec![CallArg::Handle(element.0)],
                )
                .await?;
            return Ok(Point {
                x: parent_offset.x
                    + box_.x
                    + style.get("left").and_then(Value::as_f64).unwrap_or(0.0),
                y: parent_offset.y
                    + box_.y
                    + style.get("top").and_then(Value::as_f64).unwrap_or(0.0),
            });
        }
        boundary = parent;
    }
    Ok(Point::default())
}

async fn document_root_offset(frame: &Frame) -> Result<Point> {
    let session_offset = session_root_offset(frame).await?;
    let Some(parent) = frame.parent_frame() else {
        return Ok(session_offset);
    };
    if parent.owner_session()? != frame.owner_session()? {
        return Ok(session_offset);
    }
    let Some(element) = frame_element(frame).await? else {
        return Ok(session_offset);
    };
    let box_ = raw_bounding_box(&element)
        .await?
        .ok_or_else(|| Error::Evaluation("Element is not visible".into()))?;
    let style = parent
        .call_injected(
            World::Utility,
            "(injected, iframe) => injected.describeIFrameStyle(iframe)",
            vec![CallArg::Handle(element.0)],
        )
        .await?;
    Ok(Point {
        x: session_offset.x + box_.x + style.get("left").and_then(Value::as_f64).unwrap_or(0.0),
        y: session_offset.y + box_.y + style.get("top").and_then(Value::as_f64).unwrap_or(0.0),
    })
}

async fn check_parent_frame_hit_targets(
    frame: &Frame,
    root_point: Point,
) -> Result<Option<String>> {
    let mut current = frame.clone();
    while let Some(parent) = current.parent_frame() {
        let Some(element) = frame_element(&current).await? else {
            return Ok(Some("Element is not attached to the DOM".into()));
        };
        let origin = document_root_offset(&parent).await?;
        let result = parent
            .call_injected(
                World::Utility,
                "(injected, iframe, point) => injected.expectHitTarget(point, iframe)",
                vec![
                    CallArg::Handle(element.0),
                    json!({"x":root_point.x-origin.x,"y":root_point.y-origin.y}).into(),
                ],
            )
            .await?;
        if result.as_str() != Some("done") {
            return Ok(Some(
                result
                    .get("hitTargetDescription")
                    .and_then(Value::as_str)
                    .unwrap_or("Element")
                    .to_owned(),
            ));
        }
        current = parent;
    }
    Ok(None)
}

async fn press_modifiers(page: &Page, modifiers: &[crate::KeyboardModifier]) -> Result<()> {
    for modifier in modifiers {
        page.keyboard().down(modifier.key()).await?;
    }
    Ok(())
}

async fn release_modifiers(page: &Page, modifiers: &[crate::KeyboardModifier]) -> Result<()> {
    for modifier in modifiers.iter().rev() {
        page.keyboard().up(modifier.key()).await?;
    }
    Ok(())
}

async fn retry_pause(deadline: Deadline) -> Result<()> {
    if deadline.expired() {
        return Err(Error::timeout("locator action", deadline.timeout()));
    }
    tokio::time::sleep(Duration::from_millis(20).min(deadline.remaining())).await;
    Ok(())
}

fn force_error(reason: &str) -> &str {
    if reason.contains("viewport") {
        "Element is outside of the viewport"
    } else if reason.contains("attached") {
        "Element is not attached to the DOM"
    } else {
        "Element is not visible"
    }
}

fn action_timeout(action: &str, deadline: Deadline, reason: &str) -> Error {
    Error::Timeout {
        message: format!(
            "locator.{action}: Timeout {}ms exceeded.\nCall log:\n  - waiting for {}\n  - {reason}",
            deadline.timeout().as_millis(),
            "element to be visible, enabled and stable"
        ),
    }
}

fn route_aria_frame(frame: &Frame, selector: &str) -> Result<Frame> {
    let Some(reference) = selector.strip_prefix("aria-ref=") else {
        return Ok(frame.clone());
    };
    let reference = reference
        .split_whitespace()
        .next()
        .unwrap_or(reference)
        .trim();
    let Some(rest) = reference.strip_prefix('f') else {
        return Ok(frame.clone());
    };
    let Some((sequence, _)) = rest.split_once('e') else {
        return Ok(frame.clone());
    };
    let sequence = sequence.parse::<u32>().map_err(|_| {
        Error::InvalidArgument(format!("Invalid frame in aria-ref selector \"{selector}\""))
    })?;
    frame.frame_by_sequence(sequence).ok_or_else(|| {
        Error::InvalidArgument(format!("Invalid frame in aria-ref selector \"{selector}\""))
    })
}

fn split_selector_by_frame(selector: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let bytes = selector.as_bytes();
    let mut index = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let mut bracket_depth = 0usize;
    while index < bytes.len() {
        let character = bytes[index] as char;
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if character == '\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if let Some(current_quote) = quote {
            if character == current_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(character, '\'' | '"' | '`') {
            quote = Some(character);
            index += 1;
            continue;
        }
        match character {
            '[' | '(' | '{' => bracket_depth += 1,
            ']' | ')' | '}' => bracket_depth = bracket_depth.saturating_sub(1),
            _ => {}
        }
        const TOKEN: &str = ">> internal:control=enter-frame";
        if bracket_depth == 0 && selector[index..].starts_with(TOKEN) {
            let chunk = selector[start..index].trim();
            if chunk.is_empty() {
                return Err(Error::InvalidArgument(
                    "Selector cannot start with entering frame, select the iframe first".into(),
                ));
            }
            parts.push(chunk.to_owned());
            index += TOKEN.len();
            while index < bytes.len() && (bytes[index] as char).is_whitespace() {
                index += 1;
            }
            if selector[index..].starts_with(">>") {
                index += 2;
            }
            while index < bytes.len() && (bytes[index] as char).is_whitespace() {
                index += 1;
            }
            start = index;
            continue;
        }
        index += 1;
    }
    let final_chunk = selector[start..].trim();
    if final_chunk.is_empty() {
        return Err(Error::InvalidArgument(format!(
            "Selector cannot end with entering frame, while parsing selector {selector}"
        )));
    }
    parts.push(final_chunk.to_owned());
    Ok(parts)
}

async fn set_file_paths(element: &ElementHandle, paths: Vec<PathBuf>) -> Result<()> {
    for path in &paths {
        if !path.exists() {
            return Err(Error::InvalidArgument(format!(
                "File not found: {}",
                path.display()
            )));
        }
    }
    let object = element
        .0
        .object_id()
        .ok_or_else(|| Error::Evaluation("Element is not attached to the DOM".into()))?;
    element
        .0
        .page
        .send_session(
            &element.0.session_id,
            "DOM.setFileInputFiles",
            json!({
                "objectId":object,
                "files":paths.iter().map(|path| path.to_string_lossy().into_owned()).collect::<Vec<_>>()
            }),
        )
        .await?;
    Ok(())
}

async fn set_file_payloads(element: &ElementHandle, payloads: Vec<FilePayload>) -> Result<()> {
    let payloads: Vec<Value> = payloads
        .into_iter()
        .map(|payload| {
            json!({
                "name":payload.name,
                "mimeType":payload.mime_type,
                "buffer":base64::engine::general_purpose::STANDARD.encode(payload.buffer),
                "lastModifiedMs":payload.last_modified_ms,
            })
        })
        .collect();
    let result = element
        .0
        .frame()
        .call_injected(
            World::Utility,
            "(injected, node, payloads) => injected.setInputFiles(node, payloads)",
            vec![
                CallArg::Handle(element.0.clone()),
                Value::Array(payloads).into(),
            ],
        )
        .await?;
    if let Some(error) = result.as_str() {
        return Err(Error::Evaluation(error.to_owned()));
    }
    Ok(())
}

fn string_value(value: JsValue) -> Result<String> {
    if let JsValue::String(value) = value {
        Ok(value)
    } else {
        Err(Error::Evaluation(format!("Expected string, got {value:?}")))
    }
}

fn strings(value: JsValue) -> Result<Vec<String>> {
    let JsValue::Array(values) = value else {
        return Err(Error::Evaluation(format!("Expected array, got {value:?}")));
    };
    values.into_iter().map(string_value).collect()
}

struct AriaSnapshotJson {
    json: Value,
    frame_id: String,
}

async fn aria_snapshot_json(
    frame: &Frame,
    selector: &str,
    options: &AriaSnapshotOptions,
    deadline: Deadline,
) -> Result<AriaSnapshotJson> {
    let mode = match options.mode {
        AriaSnapshotMode::Ai => "ai",
        AriaSnapshotMode::Default => "default",
    };
    let (resolved, selector) = frame
        .resolve_selector_frame(selector, options.selector.is_some())
        .await?;
    let mut result = loop {
        let value = resolved
            .call_injected(
                World::Utility,
                "(injected, selector, options) => { const element = injected.querySelector(injected.parseSelector(selector), injected.document, options.strict); return element ? injected.ariaSnapshotJSON(element, options) : null; }",
                vec![
                    selector.as_str().into(),
                    json!({"mode":mode,"depth":options.depth,"boxes":options.boxes,"strict":options.selector.is_some()}).into(),
                ],
            )
            .await?;
        if !value.is_null() {
            break value;
        }
        retry_pause(deadline).await?;
    };
    let refs: Vec<String> = result
        .get("iframeRefs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|reference| {
            result
                .get("iframeDepths")
                .and_then(|v| v.get(reference))
                .is_some()
        })
        .map(str::to_owned)
        .collect();
    let depths = result.get("iframeDepths").cloned().unwrap_or_default();
    let metadata = if options.interactive {
        Some(
            fetch_interactive_metadata(&resolved, result.get("json").unwrap_or(&Value::Null))
                .await?,
        )
    } else {
        None
    };
    let mut children = Vec::with_capacity(refs.len());
    for reference in &refs {
        let child_depth = options.depth.map(|depth| {
            depth.saturating_sub(
                depths.get(reference).and_then(Value::as_u64).unwrap_or(0) as u32 + 1,
            )
        });
        let mut child_options = options.clone();
        child_options.depth = child_depth;
        child_options.selector = Some(format!(
            "aria-ref={reference} >> internal:control=enter-frame >> body,frameset"
        ));
        let child_selector = child_options.selector.clone().unwrap();
        children.push(
            Box::pin(aria_snapshot_json(
                frame,
                &child_selector,
                &child_options,
                deadline,
            ))
            .await
            .map(|snapshot| snapshot.json)
            .unwrap_or_else(|_| json!([])),
        );
    }
    let mut json = result.get_mut("json").cloned().unwrap_or_else(|| json!([]));
    if let Some(metadata) = &metadata {
        apply_interactive_metadata(&mut json, metadata);
    }
    merge_iframe_children(&mut json, &refs, &children);
    Ok(AriaSnapshotJson {
        json,
        frame_id: resolved.id.clone(),
    })
}

fn merge_iframe_children(node: &mut Value, refs: &[String], children: &[Value]) {
    if let Some(nodes) = node.as_array_mut() {
        for child in nodes {
            merge_iframe_children(child, refs, children);
        }
        return;
    }
    let Some(object) = node.as_object_mut() else {
        return;
    };
    if object.get("role").and_then(Value::as_str) == Some("iframe") {
        if let Some(index) = object
            .get("ref")
            .and_then(Value::as_str)
            .and_then(|reference| refs.iter().position(|item| item == reference))
        {
            if children
                .get(index)
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty())
            {
                object.insert("children".into(), children[index].clone());
            }
        }
        return;
    }
    if let Some(node_children) = object.get_mut("children") {
        merge_iframe_children(node_children, refs, children);
    }
}

async fn fetch_interactive_metadata(frame: &Frame, snapshot: &Value) -> Result<Value> {
    let mut refs = Vec::new();
    collect_interactive_metadata_refs(snapshot, &mut refs);
    if refs.is_empty() {
        return Ok(json!({}));
    }
    frame
        .call_injected(
            World::Utility,
            "(injected, refs) => { const result = {}; for (const ref of refs) { const element = injected.querySelector(injected.parseSelector(`aria-ref=${ref}`), injected.document, true); if (!element) continue; const tag = element.nodeName; const nativeFocusable = ['BUTTON', 'DETAILS', 'SELECT', 'TEXTAREA'].includes(tag) || ((tag === 'A' || tag === 'AREA') && element.hasAttribute('href')) || (tag === 'INPUT' && !element.hidden); const hasTabIndex = !Number.isNaN(Number(String(element.getAttribute('tabindex')))); const focusable = !element.matches(':disabled') && (nativeFocusable || hasTabIndex); const metadata = { focusable }; if ('value' in element && typeof element.value === 'string') metadata.value = [...element.value].slice(0, 60).join(''); result[ref] = metadata; } return result; }",
            vec![json!(refs).into()],
        )
        .await
}

fn collect_interactive_metadata_refs(node: &Value, refs: &mut Vec<String>) {
    if let Some(nodes) = node.as_array() {
        for node in nodes {
            collect_interactive_metadata_refs(node, refs);
        }
        return;
    }
    let Some(object) = node.as_object() else {
        return;
    };
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let needs_metadata = role == "gridcell"
        || (matches!(role, "textbox" | "searchbox" | "combobox") && !object.contains_key("value"));
    if needs_metadata
        && let Some(reference) = object.get("ref").and_then(Value::as_str)
        && !refs.iter().any(|item| item == reference)
    {
        refs.push(reference.to_owned());
    }
    if let Some(children) = object.get("children") {
        collect_interactive_metadata_refs(children, refs);
    }
}

fn apply_interactive_metadata(node: &mut Value, metadata: &Value) {
    if let Some(nodes) = node.as_array_mut() {
        for node in nodes {
            apply_interactive_metadata(node, metadata);
        }
        return;
    }
    let Some(object) = node.as_object_mut() else {
        return;
    };
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let reference = object.get("ref").and_then(Value::as_str).map(str::to_owned);
    let node_metadata = reference
        .as_deref()
        .and_then(|reference| metadata.get(reference));
    if role == "gridcell" {
        object.insert(
            "_interactiveFocusable".into(),
            Value::Bool(
                node_metadata
                    .and_then(|value| value.get("focusable"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
        );
    }
    if matches!(role.as_str(), "textbox" | "searchbox" | "combobox") {
        if let Some(value) = object.get("value").and_then(Value::as_str) {
            object.insert(
                "_interactiveValue".into(),
                Value::String(truncate_chars(value, 60)),
            );
        } else if let Some(value) = node_metadata
            .and_then(|value| value.get("value"))
            .and_then(Value::as_str)
        {
            object.insert(
                "_interactiveValue".into(),
                Value::String(truncate_chars(value, 60)),
            );
        }
    }
    if let Some(children) = object.get_mut("children") {
        apply_interactive_metadata(children, metadata);
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn interactive_snapshot_header(title: &str, url: &str, kept: usize, total: usize) -> String {
    format!(
        "# interactive snapshot: {kept} of {total} nodes; title: {}; url: {url}",
        serde_json::to_string(title).expect("strings always serialize as JSON")
    )
}

fn filter_interactive_snapshot(snapshot: &Value) -> (Value, usize, usize) {
    let total = count_snapshot_nodes(snapshot);
    let mut nodes = Vec::new();
    if let Some(input) = snapshot.as_array() {
        for node in input {
            nodes.extend(
                filter_interactive_node(node)
                    .into_iter()
                    .filter(Value::is_object),
            );
        }
    }
    let filtered = Value::Array(nodes);
    let kept = count_snapshot_nodes(&filtered);
    (filtered, kept, total)
}

fn filter_interactive_node(node: &Value) -> Vec<Value> {
    if let Some(text) = node.as_str() {
        return nonempty_text(text).into_iter().map(Value::String).collect();
    }
    let Some(object) = node.as_object() else {
        return Vec::new();
    };
    if object.get("role").and_then(Value::as_str) == Some("text") {
        return object
            .get("text")
            .and_then(Value::as_str)
            .and_then(nonempty_text)
            .into_iter()
            .map(Value::String)
            .collect();
    }

    let mut children = Vec::new();
    if let Some(text) = object.get("text").and_then(Value::as_str) {
        children.extend(nonempty_text(text).map(Value::String));
    }
    if let Some(raw_children) = object.get("children").and_then(Value::as_array) {
        for child in raw_children {
            children.extend(filter_interactive_node(child));
        }
    }

    let role = object
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let object_children = children.iter().filter(|child| child.is_object()).count();
    let region_has_text = has_useful_text(node);
    let is_region = matches!(role, "dialog" | "alertdialog" | "alert" | "status");
    let keep = is_interactive_role(role)
        || (role == "gridcell"
            && object
                .get("_interactiveFocusable")
                .and_then(Value::as_bool)
                .unwrap_or(false))
        || (role == "img"
            && object.get("ref").and_then(Value::as_str).is_some()
            && object.get("cursor").and_then(Value::as_str) == Some("pointer"))
        || (role != "img" && object.get("cursor").and_then(Value::as_str) == Some("pointer"))
        || object.get("active").and_then(Value::as_bool) == Some(true)
        || (role == "heading"
            && object
                .get("level")
                .and_then(Value::as_u64)
                .is_some_and(|level| (1..=3).contains(&level)))
        || is_region
        || (matches!(role, "form" | "navigation" | "main" | "list") && object_children >= 2)
        || (role == "iframe" && object_children != 0);
    if !keep {
        return children;
    }

    let mut kept = object.clone();
    kept.remove("_interactiveFocusable");
    kept.remove("text");
    kept.remove("children");
    let has_name = kept
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| !name.is_empty());
    let include_text = (is_region && region_has_text)
        || (!has_name && !matches!(role, "form" | "navigation" | "main" | "list" | "iframe"));
    if !include_text {
        children.retain(Value::is_object);
    }
    if children.len() == 1 && children[0].is_string() {
        kept.insert("text".into(), children.remove(0));
    } else if !children.is_empty() {
        kept.insert("children".into(), Value::Array(children));
    }
    vec![Value::Object(kept)]
}

fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "listbox"
            | "option"
            | "checkbox"
            | "radio"
            | "switch"
            | "slider"
            | "spinbutton"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "tab"
            | "treeitem"
    )
}

fn nonempty_text(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| text.to_owned())
}

fn has_useful_text(node: &Value) -> bool {
    if let Some(text) = node.as_str() {
        return !text.trim().is_empty();
    }
    if let Some(nodes) = node.as_array() {
        return nodes.iter().any(has_useful_text);
    }
    let Some(object) = node.as_object() else {
        return false;
    };
    object
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
        || object.get("children").is_some_and(has_useful_text)
}

fn count_snapshot_nodes(node: &Value) -> usize {
    if let Some(text) = node.as_str() {
        return usize::from(!text.trim().is_empty());
    }
    if let Some(nodes) = node.as_array() {
        return nodes.iter().map(count_snapshot_nodes).sum();
    }
    let Some(object) = node.as_object() else {
        return 0;
    };
    if object.get("role").and_then(Value::as_str) == Some("text") {
        return usize::from(
            object
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty()),
        );
    }
    let own = usize::from(object.get("role").is_some());
    let text = object
        .get("text")
        .and_then(Value::as_str)
        .map(|text| usize::from(!text.trim().is_empty()))
        .unwrap_or(0);
    own + text
        + object
            .get("children")
            .map(count_snapshot_nodes)
            .unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Equal,
    Remove,
    Add,
}

struct DiffOp<'a> {
    kind: DiffKind,
    line: &'a str,
}

fn render_snapshot_diff(previous: &str, current: &str) -> String {
    if previous == current {
        return "# no changes since previous snapshot".into();
    }
    let previous_lines: Vec<_> = previous.lines().collect();
    let current_lines: Vec<_> = current.lines().collect();
    if previous_lines.len() > 5_000 || current_lines.len() > 5_000 {
        let note = "# diff unavailable: snapshot exceeds 5000 lines; full current snapshot follows";
        return if current.is_empty() {
            note.into()
        } else {
            format!("{note}\n{current}")
        };
    }

    let mut operations = Vec::new();
    lcs_diff(&previous_lines, &current_lines, &mut operations);
    let changed = operations
        .iter()
        .filter(|operation| operation.kind != DiffKind::Equal)
        .count();
    let mut ranges = Vec::<(usize, usize)>::new();
    for index in operations
        .iter()
        .enumerate()
        .filter_map(|(index, operation)| (operation.kind != DiffKind::Equal).then_some(index))
    {
        let mut start = index;
        let mut context = 0;
        while start != 0 && context != 2 && operations[start - 1].kind == DiffKind::Equal {
            start -= 1;
            context += 1;
        }
        let mut end = index + 1;
        context = 0;
        while end < operations.len() && context != 2 && operations[end].kind == DiffKind::Equal {
            end += 1;
            context += 1;
        }
        if let Some(last) = ranges.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            ranges.push((start, end));
        }
    }

    let mut output = vec![format!(
        "# diff vs previous snapshot ({changed} lines changed); use aria-ref=eN from either side"
    )];
    for (start, end) in ranges {
        let old_before = operations[..start]
            .iter()
            .filter(|operation| operation.kind != DiffKind::Add)
            .count();
        let new_before = operations[..start]
            .iter()
            .filter(|operation| operation.kind != DiffKind::Remove)
            .count();
        let old_count = operations[start..end]
            .iter()
            .filter(|operation| operation.kind != DiffKind::Add)
            .count();
        let new_count = operations[start..end]
            .iter()
            .filter(|operation| operation.kind != DiffKind::Remove)
            .count();
        let old_start = if old_count == 0 {
            old_before
        } else {
            old_before + 1
        };
        let new_start = if new_count == 0 {
            new_before
        } else {
            new_before + 1
        };
        output.push(format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
        ));
        output.extend(operations[start..end].iter().map(|operation| {
            let prefix = match operation.kind {
                DiffKind::Equal => ' ',
                DiffKind::Remove => '-',
                DiffKind::Add => '+',
            };
            format!("{prefix}{}", operation.line)
        }));
    }
    output.join("\n")
}

fn lcs_diff<'a>(old: &[&'a str], new: &[&'a str], output: &mut Vec<DiffOp<'a>>) {
    if old.is_empty() {
        output.extend(new.iter().map(|line| DiffOp {
            kind: DiffKind::Add,
            line,
        }));
        return;
    }
    if new.is_empty() {
        output.extend(old.iter().map(|line| DiffOp {
            kind: DiffKind::Remove,
            line,
        }));
        return;
    }
    if old.len() == 1 {
        if let Some(index) = new.iter().position(|line| *line == old[0]) {
            output.extend(new[..index].iter().map(|line| DiffOp {
                kind: DiffKind::Add,
                line,
            }));
            output.push(DiffOp {
                kind: DiffKind::Equal,
                line: old[0],
            });
            output.extend(new[index + 1..].iter().map(|line| DiffOp {
                kind: DiffKind::Add,
                line,
            }));
        } else {
            output.push(DiffOp {
                kind: DiffKind::Remove,
                line: old[0],
            });
            output.extend(new.iter().map(|line| DiffOp {
                kind: DiffKind::Add,
                line,
            }));
        }
        return;
    }

    let middle = old.len() / 2;
    let left = lcs_lengths(&old[..middle], new);
    let reversed_old: Vec<_> = old[middle..].iter().rev().copied().collect();
    let reversed_new: Vec<_> = new.iter().rev().copied().collect();
    let right = lcs_lengths(&reversed_old, &reversed_new);
    let mut split = 0;
    let mut best = 0;
    for index in 0..=new.len() {
        let length = left[index] + right[new.len() - index];
        if length > best {
            best = length;
            split = index;
        }
    }
    lcs_diff(&old[..middle], &new[..split], output);
    lcs_diff(&old[middle..], &new[split..], output);
}

fn lcs_lengths(old: &[&str], new: &[&str]) -> Vec<usize> {
    let mut previous = vec![0; new.len() + 1];
    for old_line in old {
        let mut current = vec![0; new.len() + 1];
        for (index, new_line) in new.iter().enumerate() {
            current[index + 1] = if old_line == new_line {
                previous[index] + 1
            } else {
                current[index].max(previous[index + 1])
            };
        }
        previous = current;
    }
    previous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_split_ignores_control_text_inside_quotes() {
        assert_eq!(
            split_selector_by_frame("iframe >> internal:control=enter-frame >> button").unwrap(),
            ["iframe", "button"]
        );
        assert_eq!(
            split_selector_by_frame("internal:text=\">> internal:control=enter-frame\"i").unwrap(),
            ["internal:text=\">> internal:control=enter-frame\"i"]
        );
    }

    #[test]
    fn interactive_filter_collapses_structure_and_counts_aria_and_text_nodes() {
        let snapshot = json!([
            {"role":"paragraph","children":[{"role":"text","text":"discarded article copy"}]},
            {"role":"button","name":"Save","ref":"e1","children":["Save"]},
            {"role":"generic","children":[{"role":"link","name":"Docs","ref":"e2"}]},
            {"role":"status","children":[{"role":"paragraph","children":["Upload complete"]}]},
            {"role":"gridcell","name":"Editable","ref":"e3","_interactiveFocusable":true},
            {"role":"gridcell","name":"Static","ref":"e4","_interactiveFocusable":false}
        ]);
        let (filtered, kept, total) = filter_interactive_snapshot(&snapshot);
        assert_eq!(
            total, 11,
            "objects and their rendered text leaves count as nodes"
        );
        assert_eq!(
            kept, 5,
            "kept nodes include useful text retained by a region"
        );
        assert_eq!(
            render_aria_snapshot_as_yaml(&filtered),
            "- button \"Save\" [ref=e1]\n- link \"Docs\" [ref=e2]\n- status: Upload complete\n- gridcell \"Editable\" [ref=e3]"
        );
    }

    #[test]
    fn interactive_metadata_adds_truncated_values_and_gridcell_focusability() {
        let mut snapshot = json!([
            {"role":"textbox","name":"Query","text":"old child value","ref":"e1"},
            {"role":"combobox","name":"Kind","ref":"e2"},
            {"role":"gridcell","name":"Focusable","ref":"e3"},
            {"role":"gridcell","name":"Static","ref":"e4"}
        ]);
        let long_value = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        apply_interactive_metadata(
            &mut snapshot,
            &json!({
                "e1":{"focusable":true,"value":long_value},
                "e2":{"focusable":true,"value":"second"},
                "e3":{"focusable":true},
                "e4":{"focusable":false}
            }),
        );
        let (filtered, _, _) = filter_interactive_snapshot(&snapshot);
        let rendered = render_aria_snapshot_as_yaml(&filtered);
        assert!(rendered.contains(&format!(
            "[value={}]",
            serde_json::to_string(&long_value.chars().take(60).collect::<String>()).unwrap()
        )));
        assert!(rendered.contains("combobox \"Kind\" [value=\"second\"]"));
        assert!(!rendered.contains("old child value"));
        assert!(rendered.contains("gridcell \"Focusable\""));
        assert!(!rendered.contains("gridcell \"Static\""));
    }

    #[test]
    fn interactive_filter_keeps_exact_region_roles_and_unnamed_control_text() {
        let snapshot = json!([
            {"role":"dialog","name":"Information","children":[{"role":"paragraph","children":["Dialog details"]}]},
            {"role":"region","name":"Generic region","children":["Discarded region copy"]},
            {"role":"textbox","_interactiveValue":"typed","children":["Visible fallback"],"ref":"e1"}
        ]);
        let (filtered, _, _) = filter_interactive_snapshot(&snapshot);
        assert_eq!(
            render_aria_snapshot_as_yaml(&filtered),
            "- dialog \"Information\": Dialog details\n- textbox [value=\"typed\"] [ref=e1]: Visible fallback"
        );
    }

    #[test]
    fn interactive_header_escapes_title_quotes_backslashes_and_controls() {
        assert_eq!(
            interactive_snapshot_header(
                "A \"quoted\" \\ title\nnext",
                "https://example.test/path",
                4,
                10
            ),
            "# interactive snapshot: 4 of 10 nodes; title: \"A \\\"quoted\\\" \\\\ title\\nnext\"; url: https://example.test/path"
        );
    }

    #[test]
    fn snapshot_diff_uses_two_context_lines_and_counts_additions_and_removals() {
        let previous = "zero\none\nold [ref=e1]\nthree\nfour\nfive";
        let current = "zero\none\nnew [ref=e2]\nthree\nfour\nfive";
        assert_eq!(
            render_snapshot_diff(previous, current),
            "# diff vs previous snapshot (2 lines changed); use aria-ref=eN from either side\n@@ -1,5 +1,5 @@\n zero\n one\n-old [ref=e1]\n+new [ref=e2]\n three\n four"
        );
        assert_eq!(
            render_snapshot_diff(current, current),
            "# no changes since previous snapshot"
        );
    }

    #[test]
    fn snapshot_diff_falls_back_for_large_inputs() {
        let previous = std::iter::repeat_n("old", 5_001)
            .collect::<Vec<_>>()
            .join("\n");
        let current = "current";
        assert_eq!(
            render_snapshot_diff(&previous, current),
            "# diff unavailable: snapshot exceeds 5000 lines; full current snapshot follows\ncurrent"
        );
    }
}
