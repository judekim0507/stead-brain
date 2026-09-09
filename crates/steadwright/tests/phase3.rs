mod support;

use std::time::Duration;

use serde_json::{Value, json};
use steadwright::{
    ActionOptions, AriaSnapshotOptions, ByRoleOptions, GotoOptions, InputFiles, LocatorFilter,
    PageEvent, SelectOptionValue, TextMatch, UrlMatcher, WaitForFunctionOptions, WaitForOptions,
    WaitForSelectorState, World,
};
use support::Fixture;

#[tokio::test]
async fn locators_build_query_filter_and_report_strict_errors() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/roles.html"), GotoOptions::default())
        .await
        .unwrap();

    assert_eq!(
        page.get_by_role(
            "heading",
            ByRoleOptions {
                level: Some(2),
                ..Default::default()
            }
        )
        .inner_text()
        .await
        .unwrap(),
        "Secondary heading"
    );
    assert_eq!(page.get_by_text("heading", false).count().await.unwrap(), 2);
    assert_eq!(
        page.get_by_label("Email address", false)
            .count()
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        page.get_by_placeholder("name@example.test", true)
            .count()
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        page.get_by_title("Destination", true)
            .count()
            .await
            .unwrap(),
        1
    );
    assert_eq!(page.get_by_test_id("toggle").count().await.unwrap(), 1);
    assert_eq!(
        page.locator("h1,h2,h3").first().inner_text().await.unwrap(),
        "Role fixtures"
    );
    assert_eq!(
        page.locator("h1,h2,h3").last().inner_text().await.unwrap(),
        "Third heading"
    );
    assert_eq!(
        page.locator("h1,h2,h3").nth(1).inner_text().await.unwrap(),
        "Secondary heading"
    );
    let filtered = page
        .locator("button")
        .filter(LocatorFilter {
            has_text: Some(TextMatch::from("Submit")),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(filtered.count().await.unwrap(), 1);
    assert_eq!(
        page.locator("body")
            .filter(LocatorFilter {
                has: Some(page.get_by_test_id("toggle")),
                ..Default::default()
            })
            .unwrap()
            .count()
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        page.locator("button")
            .and_(&page.locator("#toggle"))
            .unwrap()
            .count()
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        page.locator("#toggle")
            .or_(&page.locator("#pressed"))
            .unwrap()
            .count()
            .await
            .unwrap(),
        2
    );
    let error = page
        .get_by_role("button", ByRoleOptions::default())
        .element_handle()
        .await
        .unwrap_err();
    assert!(error.to_string().contains("strict mode violation:"));
    assert!(error.to_string().contains("resolved to"));
    page.close().await.unwrap();
}

#[tokio::test]
async fn pointer_actions_wait_hover_and_force_overlay() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/roles.html"), GotoOptions::default())
        .await
        .unwrap();
    page.get_by_test_id("toggle")
        .click(ActionOptions::default())
        .await
        .unwrap();
    assert_eq!(page.locator("#status").inner_text().await.unwrap(), "after");
    page.get_by_test_id("toggle")
        .tap(ActionOptions::default())
        .await
        .unwrap();
    assert_eq!(
        page.locator("#status").inner_text().await.unwrap(),
        "before"
    );

    page.goto(&fixture.url_a("/hover.html"), GotoOptions::default())
        .await
        .unwrap();
    page.locator("#hover")
        .hover(ActionOptions::default())
        .await
        .unwrap();
    assert_eq!(
        page.locator("#status").inner_text().await.unwrap(),
        "hovered"
    );

    page.goto(&fixture.url_a("/overlay.html"), GotoOptions::default())
        .await
        .unwrap();
    let error = page
        .locator("#covered")
        .click(ActionOptions {
            timeout: Some(Duration::from_millis(250)),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("intercepts pointer events"));
    page.locator("#covered")
        .click(ActionOptions {
            force: true,
            ..Default::default()
        })
        .await
        .unwrap();
    page.close().await.unwrap();
}

#[tokio::test]
async fn fill_type_press_reads_and_evaluate_all() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/text.html"), GotoOptions::default())
        .await
        .unwrap();
    page.locator("#area")
        .fill("filled", ActionOptions::default())
        .await
        .unwrap();
    assert_eq!(page.locator("#area").input_value().await.unwrap(), "filled");
    assert_eq!(
        page.locator(".copy").first().inner_text().await.unwrap(),
        "Hello world"
    );
    assert_ne!(
        page.locator(".copy")
            .first()
            .text_content()
            .await
            .unwrap()
            .unwrap(),
        "Hello world"
    );
    page.locator("#editable")
        .fill("content", ActionOptions::default())
        .await
        .unwrap();
    assert_eq!(
        page.locator("#editable").inner_text().await.unwrap(),
        "content"
    );
    let error = page
        .locator("#number")
        .fill("letters", ActionOptions::default())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Cannot type text into input[type=number]")
    );
    page.locator("#query")
        .type_text("term", None)
        .await
        .unwrap();
    page.locator("#query").press("Enter", None).await.unwrap();
    assert_eq!(
        page.locator("#submitted").inner_text().await.unwrap(),
        "yes"
    );
    assert_eq!(
        page.locator(".copy")
            .all_text_contents()
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(!page.locator(".missing").is_visible().await.unwrap());
    assert!(page.locator(".missing").is_hidden().await.unwrap());
    assert_eq!(
        page.locator(".copy")
            .evaluate_all("elements => elements.length", Value::Null)
            .await
            .unwrap()
            .to_json(),
        json!(2)
    );
    page.locator("#area")
        .evaluate(
            "element => { window.__steadwrightMainWorld = element.id; }",
            Value::Null,
        )
        .await
        .unwrap();
    assert_eq!(
        page.evaluate_json("window.__steadwrightMainWorld", Value::Null)
            .await
            .unwrap(),
        json!("area")
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn checkbox_select_upload_and_dispatch() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/checkbox.html"), GotoOptions::default())
        .await
        .unwrap();
    let checkbox = page.locator("#box");
    checkbox.check(ActionOptions::default()).await.unwrap();
    assert!(checkbox.is_checked().await.unwrap());
    checkbox.uncheck(ActionOptions::default()).await.unwrap();
    assert!(!checkbox.is_checked().await.unwrap());

    page.goto(&fixture.url_a("/select.html"), GotoOptions::default())
        .await
        .unwrap();
    assert_eq!(
        page.locator("#single")
            .select_option(
                vec![SelectOptionValue {
                    label: Some("Beta label".into()),
                    ..Default::default()
                }],
                ActionOptions::default()
            )
            .await
            .unwrap(),
        ["b"]
    );
    let option = page
        .locator("#single option")
        .last()
        .element_handle()
        .await
        .unwrap();
    assert_eq!(
        page.locator("#single")
            .select_option(
                vec![SelectOptionValue {
                    element: Some(option),
                    ..Default::default()
                }],
                ActionOptions::default(),
            )
            .await
            .unwrap(),
        ["c"]
    );
    page.locator("#multiple")
        .select_option(
            vec![
                SelectOptionValue {
                    value: Some("a".into()),
                    ..Default::default()
                },
                SelectOptionValue {
                    index: Some(2),
                    ..Default::default()
                },
            ],
            ActionOptions::default(),
        )
        .await
        .unwrap();

    page.goto(&fixture.url_a("/upload.html"), GotoOptions::default())
        .await
        .unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    page.locator("#upload")
        .set_input_files(
            InputFiles::Paths(vec![file.path().to_owned()]),
            ActionOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        page.locator("#upload")
            .evaluate("element => element.files.length", Value::Null)
            .await
            .unwrap()
            .to_json(),
        json!(1)
    );
    page.close().await.unwrap();
}

#[tokio::test]
async fn waits_and_network_registration_are_race_free() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/dynamic.html"), GotoOptions::default())
        .await
        .unwrap();
    page.locator("#late")
        .wait_for(WaitForOptions {
            state: WaitForSelectorState::Visible,
            timeout: Some(Duration::from_secs(2)),
        })
        .await
        .unwrap();
    page.locator("#late-enabled")
        .click(ActionOptions {
            timeout: Some(Duration::from_secs(2)),
            ..Default::default()
        })
        .await
        .unwrap();
    page.locator("#remove")
        .click(ActionOptions::default())
        .await
        .unwrap();
    page.locator("#remove")
        .wait_for(WaitForOptions {
            state: WaitForSelectorState::Hidden,
            timeout: Some(Duration::from_secs(1)),
        })
        .await
        .unwrap();
    page.wait_for_function(
        "() => document.querySelector('#late')",
        Value::Null,
        WaitForFunctionOptions::default(),
    )
    .await
    .unwrap();

    page.goto(&fixture.url_a("/roles.html"), GotoOptions::default())
        .await
        .unwrap();
    let response = page.wait_for_response(
        UrlMatcher::Glob("**/slow?ms=50".into()),
        Some(Duration::from_secs(2)),
    );
    let request = page.wait_for_request(
        UrlMatcher::Glob("**/slow?ms=50".into()),
        Some(Duration::from_secs(2)),
    );
    page.locator("#fetcher")
        .click(ActionOptions::default())
        .await
        .unwrap();
    assert_eq!(request.await.unwrap().method, "GET");
    assert_eq!(response.await.unwrap().status, 200);
    page.close().await.unwrap();
}

#[tokio::test]
async fn cross_origin_nested_frames_and_aria_refs_work() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(
        &fixture.url_a("/nested-iframes.html"),
        GotoOptions::default(),
    )
    .await
    .unwrap();
    let child = page.frame_locator("iframe[title=child]");
    child
        .get_by_role(
            "button",
            ByRoleOptions {
                name: Some(TextMatch::from("Child button")),
                ..Default::default()
            },
        )
        .click(ActionOptions::default())
        .await
        .unwrap();
    assert_eq!(
        child.locator("#child").inner_text().await.unwrap(),
        "child clicked"
    );
    let grandchild = child.frame_locator("iframe[title=grandchild]");
    grandchild
        .locator("#grandchild")
        .click(ActionOptions::default())
        .await
        .unwrap();
    assert_eq!(
        grandchild
            .locator("#grandchild")
            .inner_text()
            .await
            .unwrap(),
        "grandchild clicked"
    );
    let snapshot = page
        .aria_snapshot(AriaSnapshotOptions::default())
        .await
        .unwrap();
    assert!(snapshot.contains("Parent content"));
    assert!(snapshot.contains("Child content"));
    assert!(snapshot.contains("Grandchild content"));
    assert!(snapshot.contains("[ref=f"));
    page.close().await.unwrap();
}

#[tokio::test]
async fn single_frame_aria_renderer_matches_injected_recorder_golden() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/roles.html"), GotoOptions::default())
        .await
        .unwrap();
    let expected = page
        .main_frame()
        .call_injected(
            World::Utility,
            "injected => injected.ariaSnapshotForRecorder().ariaSnapshot",
            vec![],
        )
        .await
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let actual = page
        .aria_snapshot(AriaSnapshotOptions::default())
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert!(actual.contains("[level=2]"));
    assert!(actual.contains("[checked"));
    assert!(actual.contains("[disabled]"));
    assert!(actual.contains("[ref=e"));
    let submit_line = actual
        .lines()
        .find(|line| line.contains("button \"Submit\""))
        .unwrap();
    let reference = submit_line
        .split("[ref=")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    page.locator(&format!("aria-ref={reference}"))
        .click(ActionOptions::default())
        .await
        .unwrap();
    assert_eq!(page.locator("#status").inner_text().await.unwrap(), "after");
    let error = page
        .locator("aria-ref=e999999")
        .click(ActionOptions {
            timeout: Some(Duration::from_millis(100)),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Timeout 100ms exceeded"));
    page.close().await.unwrap();
}

#[tokio::test]
async fn wait_for_event_registers_dialog_and_popup_before_actions() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/index.html"), GotoOptions::default())
        .await
        .unwrap();
    let dialog = page.wait_for_event("dialog", Some(Duration::from_secs(2)));
    page.evaluate_json(
        "() => setTimeout(() => alert('waited dialog'), 0)",
        Value::Null,
    )
    .await
    .unwrap();
    let PageEvent::Dialog(dialog) = dialog.await.unwrap() else {
        panic!("expected dialog event")
    };
    assert_eq!(dialog.message, "waited dialog");
    dialog.accept(None).await.unwrap();

    let download_dir = tempfile::tempdir().unwrap();
    fixture
        .browser
        .default_context()
        .set_download_behavior(download_dir.path())
        .await
        .unwrap();
    let download = page.wait_for_event("download", Some(Duration::from_secs(2)));
    page.evaluate_json(
        "url => { const link = document.createElement('a'); link.href = url; link.click(); }",
        json!(fixture.url_a("/download")),
    )
    .await
    .unwrap();
    let PageEvent::Download(download) = download.await.unwrap() else {
        panic!("expected download event")
    };
    assert_eq!(download.suggested_filename, "steadwright.txt");
    download.path().await.unwrap();

    let popup = page.wait_for_event("popup", Some(Duration::from_secs(2)));
    page.evaluate_json("() => window.open('/text.html')", Value::Null)
        .await
        .unwrap();
    let PageEvent::Popup(popup) = popup.await.unwrap() else {
        panic!("expected popup event")
    };
    popup
        .wait_for_url(
            UrlMatcher::Glob("**/text.html".into()),
            steadwright::WaitForUrlOptions {
                timeout: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    popup.close().await.unwrap();

    page.goto(&fixture.url_a("/upload.html"), GotoOptions::default())
        .await
        .unwrap();
    let chooser = page.wait_for_event("filechooser", Some(Duration::from_secs(2)));
    page.locator("#upload")
        .click(ActionOptions::default())
        .await
        .unwrap();
    let PageEvent::FileChooser(chooser) = chooser.await.unwrap() else {
        panic!("expected file chooser event")
    };
    let file = tempfile::NamedTempFile::new().unwrap();
    chooser
        .set_files(vec![file.path().to_owned()])
        .await
        .unwrap();
    page.close().await.unwrap();
}

#[tokio::test]
async fn click_waits_for_scheduled_navigation() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/roles.html"), GotoOptions::default())
        .await
        .unwrap();
    page.evaluate_json(
        "() => { const button = document.createElement('button'); button.id = 'navigate'; button.textContent = 'Navigate'; button.onclick = () => location.href = '/text.html'; document.body.append(button); }",
        Value::Null,
    )
    .await
    .unwrap();
    page.locator("#navigate")
        .click(ActionOptions::default())
        .await
        .unwrap();
    assert!(page.url().ends_with("/text.html"));
    assert_eq!(page.locator("#area").count().await.unwrap(), 1);
    page.close().await.unwrap();
}

#[tokio::test]
async fn drag_highlight_describe_and_screenshot_work() {
    let Some(fixture) = Fixture::get().await else {
        return;
    };
    let page = fixture.new_page().await.unwrap();
    page.goto(&fixture.url_a("/drag.html"), GotoOptions::default())
        .await
        .unwrap();
    let source = page.locator("#source");
    source.highlight().await.unwrap();
    assert!(!source.describe().await.unwrap().is_empty());
    assert!(
        !source
            .screenshot(Default::default())
            .await
            .unwrap()
            .is_empty()
    );
    source
        .drag_to(&page.locator("#target"), ActionOptions::default())
        .await
        .unwrap();
    assert_eq!(
        page.locator("#status").inner_text().await.unwrap(),
        "dropped"
    );
    page.locator("#target")
        .dispatch_event("click", json!({}))
        .await
        .unwrap();
    page.close().await.unwrap();
}
