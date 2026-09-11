//! Timing probe: how long do the primitives the model uses take on a heavy
//! real page? Launches headless Stead, drives the Apple configurator.
use std::time::Instant;
use steadwright::{
    ActionOptions, AriaSnapshotOptions, Browser, ByRoleOptions, GotoOptions, TextMatch,
};
use steadwright_cdp::chromium::{self, LaunchOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let exe = chromium::find_executable().expect("chromium");
    let launched = chromium::launch(LaunchOptions {
        executable: Some(exe),
        ..Default::default()
    })
    .await?;
    let browser = Browser::connect_ws(&launched.ws_url).await?;
    let page = browser.new_page().await?;
    let t = Instant::now();
    page.goto(
        "https://www.apple.com/ca/shop/buy-mac/macbook-pro",
        GotoOptions::default(),
    )
    .await?;
    println!("goto: {:?}", t.elapsed());
    for i in 0..3 {
        let t = Instant::now();
        let s = page.aria_snapshot(AriaSnapshotOptions::default()).await?;
        println!(
            "ariaSnapshot #{i}: {:?} ({} bytes, {} lines)",
            t.elapsed(),
            s.len(),
            s.lines().count()
        );
    }
    let t = Instant::now();
    let n = page
        .get_by_role("radio", ByRoleOptions::default())
        .count()
        .await?;
    println!("count radios: {:?} -> {n}", t.elapsed());
    let t = Instant::now();
    let r = page
        .get_by_role(
            "radio",
            ByRoleOptions {
                name: Some(TextMatch::Regex("16-inch".into(), String::new())),
                ..Default::default()
            },
        )
        .first()
        .click(ActionOptions::default())
        .await;
    println!(
        "click 16-inch radio by role/name: {:?} -> {:?}",
        t.elapsed(),
        r.as_ref().err()
    );
    let t = Instant::now();
    let s = page.aria_snapshot(AriaSnapshotOptions::default()).await?;
    println!(
        "ariaSnapshot after click: {:?} ({} bytes)",
        t.elapsed(),
        s.len()
    );
    let t = Instant::now();
    let r = page
        .get_by_text(TextMatch::Str("Space Black".into()), true)
        .last()
        .click(ActionOptions::default())
        .await;
    println!(
        "click getByText('Space Black').last(): {:?} -> {:?}",
        t.elapsed(),
        r.err()
            .map(|e| e.to_string().lines().next().unwrap_or("").to_string())
    );
    let t = Instant::now();
    let r = page
        .get_by_role(
            "radio",
            ByRoleOptions {
                name: Some(TextMatch::Str("Space Black".into())),
                ..Default::default()
            },
        )
        .click(ActionOptions::default())
        .await;
    println!(
        "click radio name='Space Black': {:?} -> {:?}",
        t.elapsed(),
        r.err()
            .map(|e| e.to_string().lines().next().unwrap_or("").to_string())
    );
    let t = Instant::now();
    let r = page
        .get_by_role(
            "radio",
            ByRoleOptions {
                name: Some(TextMatch::Str("Nope Missing".into())),
                ..Default::default()
            },
        )
        .click(ActionOptions {
            timeout: Some(std::time::Duration::from_secs(5)),
            ..Default::default()
        })
        .await;
    println!(
        "missing locator w/ 5s timeout: {:?} -> {:?}",
        t.elapsed(),
        r.err()
            .map(|e| e.to_string().lines().next().unwrap_or("").to_string())
    );
    drop(launched);
    Ok(())
}
