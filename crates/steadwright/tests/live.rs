mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use steadwright::{
    ActionOptions, Browser, ByRoleOptions, ClickOptions, GotoOptions, JsValue, LoadState,
    MouseButton, ScreenshotFormat, ScreenshotOptions, UrlMatcher, ViewportSize, WaitForUrlOptions,
};
use steadwright_cdp::transport::Incoming;
use steadwright_cdp::{Transport, TransportError};
use support::Fixture;
use tokio::sync::mpsc;

struct FakeBrowserTransport {
    incoming: Option<Incoming>,
    responses: mpsc::Sender<Result<String, TransportError>>,
    sent_attachments: AtomicBool,
}

impl FakeBrowserTransport {
    async fn emit(&self, message: Value) -> Result<(), TransportError> {
        self.responses
            .send(Ok(message.to_string()))
            .await
            .map_err(|_| TransportError::Closed)
    }
}

#[async_trait]
impl Transport for FakeBrowserTransport {
    async fn send(&self, message: String) -> Result<(), TransportError> {
        let command: Value = serde_json::from_str(&message)
            .map_err(|error| TransportError::InvalidMessage(error.to_string()))?;
        let id = command["id"].clone();
        let method = command["method"].as_str().unwrap_or_default();
        let session = command.get("sessionId").and_then(Value::as_str);
        let result = match method {
            "Target.getTargets" => json!({
                "targetInfos": [
                    {"targetId":"denied","type":"page","url":"https://denied.example/","attached":true},
                    {"targetId":"usable","type":"page","url":"https://usable.example/","attached":true}
                ]
            }),
            "Page.getFrameTree" => {
                let frame_id = if session == Some("denied-session") {
                    "denied-frame"
                } else {
                    "usable-frame"
                };
                json!({"frameTree":{"frame":{"id":frame_id,"url":format!("https://{}.example/", if frame_id == "denied-frame" { "denied" } else { "usable" }),"name":"","loaderId":"loader"}}})
            }
            _ => json!({}),
        };
        self.emit(json!({"id":id,"result":result})).await?;

        if method == "Target.setAutoAttach"
            && session.is_none()
            && !self.sent_attachments.swap(true, Ordering::SeqCst)
        {
            for (target_id, session_id, url) in [
                ("denied", "denied-session", "https://denied.example/"),
                ("usable", "usable-session", "https://usable.example/"),
            ] {
                self.emit(json!({
                    "method":"Target.attachedToTarget",
                    "params":{
                        "sessionId":session_id,
                        "targetInfo":{"targetId":target_id,"type":"page","url":url,"attached":true},
                        "waitingForDebugger":true
                    }
                }))
                .await?;
            }
        }
        Ok(())
    }

    fn incoming(&mut self) -> Incoming {
        self.incoming.take().unwrap()
    }

    async fn close(&self) {}
}

struct RejectFrameTreeTransport<T> {
    inner: T,
    responses: mpsc::Sender<Result<String, TransportError>>,
    rejected: AtomicBool,
}

#[async_trait]
impl<T: Transport + Sync> Transport for RejectFrameTreeTransport<T> {
    async fn send(&self, message: String) -> Result<(), TransportError> {
        let command: Value = serde_json::from_str(&message)
            .map_err(|error| TransportError::InvalidMessage(error.to_string()))?;
        if command["method"] == "Page.getFrameTree"
            && command.get("sessionId").and_then(Value::as_str) == Some("denied-session")
            && !self.rejected.swap(true, Ordering::SeqCst)
        {
            return self
                .responses
                .send(Ok(json!({
                    "id":command["id"],
                    "error":{"code":-32000,"message":"stead: policy denied command"}
                })
                .to_string()))
                .await
                .map_err(|_| TransportError::Closed);
        }
        self.inner.send(message).await
    }

    fn incoming(&mut self) -> Incoming {
        self.inner.incoming()
    }

    async fn close(&self) {
        self.inner.close().await;
    }
}

#[tokio::test]
async fn connect_ignores_one_page_initialization_protocol_error() {
    let (responses, incoming) = mpsc::channel(128);
    let transport = RejectFrameTreeTransport {
        responses: responses.clone(),
        rejected: AtomicBool::new(false),
        inner: FakeBrowserTransport {
            incoming: Some(incoming),
            responses,
            sent_attachments: AtomicBool::new(false),
        },
    };

    let browser = Browser::connect(transport)
        .await
        .expect("one denied target must not make the browser connection fail");
    let pages = browser.default_context().pages();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].url(), "https://usable.example/");

    let retried = browser
        .default_context()
        .page_for_url("https://denied.example/")
        .await
        .expect("a later URL lookup should retry page initialization")
        .expect("the denied target should still be attached");
    assert_eq!(retried.url(), "https://denied.example/");
    assert_eq!(browser.default_context().pages().len(), 2);
}

fn null() -> Value {
    Value::Null
}

fn property<'a>(value: &'a JsValue, name: &str) -> &'a JsValue {
    let JsValue::Object(entries) = value else {
        panic!("expected an object, got {value:?}");
    };
    entries
        .iter()
        .find_map(|(key, value)| (key == name).then_some(value))
        .unwrap_or_else(|| panic!("missing property {name:?} in {value:?}"))
}

#[tokio::test]
async fn goto_returns_response_and_updates_url() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    let url = fixture.url_a("/index.html");
    let response = page
        .goto(&url, GotoOptions::default())
        .await
        .unwrap()
        .expect("http navigation should have a response");

    assert_eq!(response.status, 200);
    assert!(response.ok);
    assert_eq!(response.url, url);
    assert_eq!(page.url(), url);
    page.close().await.unwrap();
}

#[tokio::test]
async fn goto_reports_dns_failure() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    let url = "https://steadwright-does-not-exist.invalid/";
    let error = page
        .goto(url, GotoOptions::default())
        .await
        .expect_err("reserved .invalid domain must fail");

    let message = error.to_string();
    assert!(message.contains("page.goto: net::ERR_NAME_NOT_RESOLVED at"));
    assert!(message.contains(url));
    page.close().await.unwrap();
}

#[tokio::test]
async fn network_idle_waits_for_the_slow_document_request() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    let started = Instant::now();
    page.goto(
        &fixture.url_a("/slow?ms=800"),
        GotoOptions {
            wait_until: LoadState::NetworkIdle,
            ..GotoOptions::default()
        },
    )
    .await
    .unwrap();

    assert!(
        started.elapsed() >= Duration::from_millis(800),
        "network-idle navigation returned before the delayed response"
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn title_and_content_reflect_the_document() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/index.html"), GotoOptions::default())
        .await
        .unwrap();

    assert_eq!(page.title().await.unwrap(), "Steadwright fixtures");
    let content = page.content().await.unwrap();
    assert!(content.contains("<!DOCTYPE html>"));
    assert!(content.contains("Steadwright fixture index"));
    page.close().await.unwrap();
}

#[tokio::test]
async fn internal_reads_and_role_click_stay_in_the_utility_world() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    fixture.cdp_trace.clear_commands();

    page.goto(&fixture.url_a("/index.html"), GotoOptions::default())
        .await
        .unwrap();
    assert_eq!(page.title().await.unwrap(), "Steadwright fixtures");
    page.get_by_role(
        "button",
        ByRoleOptions {
            name: Some("before click".into()),
            ..ByRoleOptions::default()
        },
    )
    .click(ActionOptions::default())
    .await
    .unwrap();

    let main_world = fixture.cdp_trace.runtime_main_world_commands();
    assert!(
        main_world.is_empty(),
        "steadwright internal operations sent main-world Runtime commands: {main_world:#?}"
    );

    fixture.cdp_trace.clear_commands();
    assert_eq!(
        page.evaluate_json("document.title", Value::Null)
            .await
            .unwrap(),
        json!("Steadwright fixtures")
    );
    assert!(
        !fixture.cdp_trace.runtime_main_world_commands().is_empty(),
        "the trace must distinguish an explicit main-world evaluation"
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn evaluate_runs_expressions_and_functions() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/index.html"), GotoOptions::default())
        .await
        .unwrap();

    assert_eq!(page.evaluate_json("1 + 1", null()).await.unwrap(), json!(2));
    assert_eq!(
        page.evaluate_json("value => value.nested[1]", json!({ "nested": [3, 7] }))
            .await
            .unwrap(),
        json!(7)
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn evaluate_serializes_special_javascript_values() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/index.html"), GotoOptions::default())
        .await
        .unwrap();

    let argument = JsValue::Object(vec![
        (
            "date".into(),
            JsValue::Date("2020-05-06T07:08:09.000Z".into()),
        ),
        (
            "regexp".into(),
            JsValue::RegExp {
                source: "stead.+wright".into(),
                flags: "gi".into(),
            },
        ),
        (
            "map".into(),
            JsValue::Map(vec![(JsValue::String("one".into()), JsValue::Number(1.0))]),
        ),
        (
            "set".into(),
            JsValue::Set(vec![JsValue::String("two".into())]),
        ),
        ("nullValue".into(), JsValue::Null),
        ("undefinedValue".into(), JsValue::Undefined),
        ("nan".into(), JsValue::Number(f64::NAN)),
        ("positiveInfinity".into(), JsValue::Number(f64::INFINITY)),
        (
            "negativeInfinity".into(),
            JsValue::Number(f64::NEG_INFINITY),
        ),
        ("negativeZero".into(), JsValue::Number(-0.0)),
        (
            "bigint".into(),
            JsValue::BigInt("12345678901234567890".into()),
        ),
    ]);
    let value = page
        .evaluate(
            "value => {\n\
               if (!(value.date instanceof Date)) throw new Error('date was not revived');\n\
               if (!(value.regexp instanceof RegExp)) throw new Error('regexp was not revived');\n\
               if (!(value.map instanceof Map)) throw new Error('map was not revived');\n\
               if (!(value.set instanceof Set)) throw new Error('set was not revived');\n\
               if (value.undefinedValue !== undefined) throw new Error('undefined was not revived');\n\
               if (!Number.isNaN(value.nan)) throw new Error('NaN was not revived');\n\
               if (value.positiveInfinity !== Infinity) throw new Error('Infinity was not revived');\n\
               if (value.negativeInfinity !== -Infinity) throw new Error('-Infinity was not revived');\n\
               if (!Object.is(value.negativeZero, -0)) throw new Error('-0 was not revived');\n\
               if (value.bigint !== 12345678901234567890n) throw new Error('bigint was not revived');\n\
               return value;\n\
             }",
            argument,
        )
        .await
        .unwrap();

    assert_eq!(
        property(&value, "date"),
        &JsValue::Date("2020-05-06T07:08:09.000Z".into())
    );
    assert_eq!(
        property(&value, "regexp"),
        &JsValue::RegExp {
            source: "stead.+wright".into(),
            flags: "gi".into(),
        }
    );
    assert_eq!(
        property(&value, "map"),
        &JsValue::Map(vec![(JsValue::String("one".into()), JsValue::Number(1.0),)])
    );
    assert_eq!(
        property(&value, "set"),
        &JsValue::Set(vec![JsValue::String("two".into())])
    );
    assert_eq!(property(&value, "nullValue"), &JsValue::Null);
    assert_eq!(property(&value, "undefinedValue"), &JsValue::Undefined);
    assert!(matches!(property(&value, "nan"), JsValue::Number(v) if v.is_nan()));
    assert_eq!(
        property(&value, "positiveInfinity"),
        &JsValue::Number(f64::INFINITY)
    );
    assert_eq!(
        property(&value, "negativeInfinity"),
        &JsValue::Number(f64::NEG_INFINITY)
    );
    assert!(
        matches!(property(&value, "negativeZero"), JsValue::Number(v) if *v == 0.0 && v.is_sign_negative())
    );
    assert_eq!(
        property(&value, "bigint"),
        &JsValue::BigInt("12345678901234567890".into())
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn evaluate_handle_and_json_value_round_trip() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    let handle = page
        .evaluate_handle(
            "() => ({ answer: 42, nested: ['stead', 'wright'], map: new Map([['one', 1]]), set: new Set(['two']) })",
            null(),
        )
        .await
        .unwrap();

    assert_eq!(
        handle.json_value().await.unwrap(),
        JsValue::Object(vec![
            ("answer".into(), JsValue::Number(42.0)),
            (
                "nested".into(),
                JsValue::Array(vec![
                    JsValue::String("stead".into()),
                    JsValue::String("wright".into()),
                ]),
            ),
            (
                "map".into(),
                JsValue::Map(vec![(JsValue::String("one".into()), JsValue::Number(1.0),)]),
            ),
            (
                "set".into(),
                JsValue::Set(vec![JsValue::String("two".into())]),
            ),
        ])
    );
    assert_eq!(
        handle
            .evaluate_json("object => object.answer + 1", null())
            .await
            .unwrap(),
        json!(43)
    );
    handle.dispose().await.unwrap();

    let primitive = page.evaluate_handle("() => -0", null()).await.unwrap();
    assert!(
        matches!(primitive.json_value().await.unwrap(), JsValue::Number(v) if v == 0.0 && v.is_sign_negative())
    );
    assert_eq!(
        page.evaluate_json("value => Object.is(value, -0)", &primitive)
            .await
            .unwrap(),
        json!(true)
    );
    primitive.dispose().await.unwrap();

    let undefined = page
        .evaluate_handle("() => undefined", null())
        .await
        .unwrap();
    assert_eq!(undefined.json_value().await.unwrap(), JsValue::Undefined);
    assert_eq!(
        page.evaluate_json("value => value === undefined", &undefined)
            .await
            .unwrap(),
        json!(true)
    );
    undefined.dispose().await.unwrap();
    page.close().await.unwrap();
}

#[tokio::test]
async fn cross_origin_iframe_has_its_own_evaluable_frame() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(
        &fixture.url_a("/iframe-parent.html"),
        GotoOptions::default(),
    )
    .await
    .unwrap();

    let frames = page.frames();
    assert_eq!(frames.len(), 2);
    let child = frames
        .into_iter()
        .find(|frame| frame.url().contains("iframe-child.html"))
        .expect("child frame");
    assert_eq!(child.url(), fixture.url_b("/iframe-child.html"));
    assert_eq!(
        child
            .evaluate_json("location.origin", null())
            .await
            .unwrap(),
        json!(fixture.origin_b)
    );
    assert_eq!(
        child
            .evaluate_json("document.querySelector('#child-btn').textContent", null())
            .await
            .unwrap(),
        json!("child button")
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn same_document_navigation_updates_frame_url() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/spa.html"), GotoOptions::default())
        .await
        .unwrap();
    page.evaluate_json("document.querySelector('#push-state').click()", null())
        .await
        .unwrap();
    page.wait_for_url(
        UrlMatcher::Glob("**/spa.html?navigated=yes".to_owned()),
        WaitForUrlOptions::default(),
    )
    .await
    .unwrap();

    assert!(page.main_frame().url().ends_with("/spa.html?navigated=yes"));
    page.close().await.unwrap();
}

#[tokio::test]
async fn history_back_forward_and_edges() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    let first = fixture.url_a("/index.html");
    let second = fixture.url_a("/spa.html");
    page.goto(&first, GotoOptions::default()).await.unwrap();
    page.goto(&second, GotoOptions::default()).await.unwrap();

    assert!(
        page.go_back(GotoOptions::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(page.url(), first);
    assert!(
        page.go_forward(GotoOptions::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(page.url(), second);
    page.evaluate_json("document.querySelector('#push-state').click()", null())
        .await
        .unwrap();
    page.wait_for_url(
        UrlMatcher::Glob("**/spa.html?navigated=yes".to_owned()),
        WaitForUrlOptions::default(),
    )
    .await
    .unwrap();
    assert!(
        page.go_back(GotoOptions::default())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(page.url(), second);
    assert!(
        page.go_forward(GotoOptions::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(page.url().ends_with("/spa.html?navigated=yes"));
    assert!(
        page.go_forward(GotoOptions::default())
            .await
            .unwrap()
            .is_none()
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn reload_keeps_the_current_url() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    let url = fixture.url_a("/index.html");
    page.goto(&url, GotoOptions::default()).await.unwrap();
    page.reload(GotoOptions::default()).await.unwrap();

    assert_eq!(page.url(), url);
    assert_eq!(page.title().await.unwrap(), "Steadwright fixtures");
    page.close().await.unwrap();
}

#[tokio::test]
async fn screenshot_has_png_signature_and_full_page_dimensions() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.set_viewport_size(800, 600).await.unwrap();
    page.goto(&fixture.url_a("/long.html"), GotoOptions::default())
        .await
        .unwrap();

    let image = page
        .screenshot(ScreenshotOptions {
            full_page: true,
            format: ScreenshotFormat::Png,
            ..ScreenshotOptions::default()
        })
        .await
        .unwrap();
    assert_eq!(&image[..8], b"\x89PNG\r\n\x1a\n");
    assert!(png_height(&image).expect("PNG IHDR") >= 5000);
    page.close().await.unwrap();
}

#[tokio::test]
async fn viewport_size_is_reflected_in_window_inner_width() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.set_viewport_size(713, 503).await.unwrap();

    assert_eq!(
        page.viewport_size(),
        Some(ViewportSize {
            width: 713,
            height: 503
        })
    );
    assert_eq!(
        page.evaluate_json("innerWidth", null()).await.unwrap(),
        json!(713)
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn keyboard_types_and_inserts_text() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/input.html"), GotoOptions::default())
        .await
        .unwrap();
    page.evaluate_json("document.querySelector('#text').focus()", null())
        .await
        .unwrap();
    page.keyboard().type_text("Hello").await.unwrap();
    page.keyboard().insert_text(", world!").await.unwrap();

    assert_eq!(
        page.evaluate_json("document.querySelector('#text').value", null())
            .await
            .unwrap(),
        json!("Hello, world!")
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn keyboard_press_dispatches_playwright_compatible_key_fields() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/keys.html"), GotoOptions::default())
        .await
        .unwrap();
    page.keyboard().press("Shift+A").await.unwrap();
    page.keyboard().press("Enter").await.unwrap();
    let log = page
        .evaluate_json("document.querySelector('#log').textContent", null())
        .await
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();

    assert!(log.contains("keydown|Shift|ShiftLeft|16"));
    assert!(log.contains("keydown|A|KeyA|65"));
    assert!(log.contains("keypress|A|KeyA|65"));
    assert!(log.contains("keyup|A|KeyA|65"));
    assert!(log.contains("keydown|Enter|Enter|13"));
    assert!(log.contains("keyup|Enter|Enter|13"));
    page.close().await.unwrap();
}

#[tokio::test]
async fn mouse_click_targets_the_requested_coordinates() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/wheel.html"), GotoOptions::default())
        .await
        .unwrap();
    page.mouse()
        .click(
            170.0,
            110.0,
            ClickOptions {
                button: MouseButton::Left,
                ..ClickOptions::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        page.evaluate_json("document.querySelector('#toggle').textContent", null())
            .await
            .unwrap(),
        json!("after click")
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn mouse_wheel_scrolls_the_page() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/wheel.html"), GotoOptions::default())
        .await
        .unwrap();
    page.mouse().wheel(0.0, 900.0).await.unwrap();
    page.wait_for_timeout(100).await.unwrap();

    assert!(
        page.evaluate_json("scrollY", null())
            .await
            .unwrap()
            .as_f64()
            .unwrap_or(0.0)
            > 0.0
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn touchscreen_tap_clicks_an_element() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/wheel.html"), GotoOptions::default())
        .await
        .unwrap();
    page.touchscreen().tap(170.0, 110.0).await.unwrap();

    assert_eq!(
        page.evaluate_json("document.querySelector('#toggle').textContent", null())
            .await
            .unwrap(),
        json!("after click")
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn dialog_handler_receives_and_accepts_alert() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let sender = std::sync::Mutex::new(Some(sender));
    page.on_dialog(move |dialog| {
        let message = dialog.message.clone();
        if let Some(sender) = sender.lock().unwrap().take() {
            tokio::spawn(async move {
                dialog.accept(None).await.unwrap();
                let _ = sender.send(message);
            });
        }
    });
    page.goto(&fixture.url_a("/dialog.html"), GotoOptions::default())
        .await
        .unwrap();

    assert_eq!(receiver.await.unwrap(), "steadwright alert");
    page.close().await.unwrap();
}

#[tokio::test]
async fn dialog_without_handler_is_dismissed() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/dialog.html"), GotoOptions::default())
        .await
        .unwrap();

    assert_eq!(page.evaluate_json("1 + 1", null()).await.unwrap(), json!(2));
    page.close().await.unwrap();
}

#[tokio::test]
async fn console_message_is_reported() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let sender = std::sync::Mutex::new(Some(sender));
    page.on_console(move |message| {
        if let Some(sender) = sender.lock().unwrap().take() {
            let _ = sender.send((message.type_.clone(), message.text.clone()));
        }
    });
    page.evaluate_json("console.log('hello', 17)", null())
        .await
        .unwrap();

    assert_eq!(
        receiver.await.unwrap(),
        ("log".to_owned(), "hello 17".to_owned())
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn close_removes_page_and_new_page_adds_one() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    assert_eq!(fixture.browser.contexts().len(), 1);
    let context = fixture.browser.default_context();
    let before = context.pages().len();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let sender = std::sync::Mutex::new(Some(sender));
    fixture.browser.on_target(move |page| {
        if let Some(sender) = sender.lock().unwrap().take() {
            let _ = sender.send(page);
        }
    });
    let page = context.new_page().await.unwrap();
    let reported = tokio::time::timeout(Duration::from_secs(2), receiver)
        .await
        .expect("on_target should report a newly initialized page")
        .unwrap();
    assert_eq!(format!("{reported:?}"), format!("{page:?}"));
    assert_eq!(context.pages().len(), before + 1);
    page.close().await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        while context.pages().len() != before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("closed page should be removed from its context");
    assert!(page.is_closed());
}

#[tokio::test]
async fn load_state_bring_to_front_and_page_scoped_download_complete() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/index.html"), GotoOptions::default())
        .await
        .unwrap();
    page.wait_for_load_state(LoadState::Load, Some(Duration::from_secs(2)))
        .await
        .unwrap();
    page.bring_to_front().await.unwrap();

    let download_dir = tempfile::tempdir().unwrap();
    fixture
        .browser
        .default_context()
        .set_download_behavior(download_dir.path())
        .await
        .unwrap();
    let other_page = fixture.new_page().await.unwrap();
    let (download_sender, download_receiver) = tokio::sync::oneshot::channel();
    let download_sender = std::sync::Mutex::new(Some(download_sender));
    page.on_download(move |download| {
        if let Some(sender) = download_sender.lock().unwrap().take() {
            let _ = sender.send(download);
        }
    });
    let (other_sender, other_receiver) = tokio::sync::oneshot::channel();
    let other_sender = std::sync::Mutex::new(Some(other_sender));
    other_page.on_download(move |download| {
        if let Some(sender) = other_sender.lock().unwrap().take() {
            let _ = sender.send(download);
        }
    });
    page.evaluate_json(
        "url => { const link = document.createElement('a'); link.href = url; link.click(); }",
        json!(fixture.url_a("/download")),
    )
    .await
    .unwrap();
    let download = tokio::time::timeout(Duration::from_secs(2), download_receiver)
        .await
        .expect("initiating page should receive its download")
        .unwrap();
    assert_eq!(download.suggested_filename, "steadwright.txt");
    let path = tokio::time::timeout(Duration::from_secs(2), download.path())
        .await
        .expect("download should complete")
        .unwrap();
    assert_eq!(
        tokio::fs::read(path).await.unwrap(),
        b"steadwright download"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), other_receiver)
            .await
            .is_err(),
        "another page must not receive the download"
    );
    other_page.close().await.unwrap();
    page.close().await.unwrap();
}

fn png_height(image: &[u8]) -> Option<u32> {
    (image.len() >= 24 && &image[12..16] == b"IHDR")
        .then(|| u32::from_be_bytes(image[20..24].try_into().unwrap()))
}
