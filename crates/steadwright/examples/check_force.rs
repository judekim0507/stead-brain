use std::time::Instant;
use steadwright::{ActionOptions, Browser, ByRoleOptions, GotoOptions, TextMatch};
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
    page.goto(
        "https://www.apple.com/ca/shop/buy-mac/macbook-pro",
        GotoOptions::default(),
    )
    .await?;
    let r = page.get_by_role(
        "radio",
        ByRoleOptions {
            name: Some(TextMatch::Regex("^16-inch".into(), String::new())),
            ..Default::default()
        },
    );
    let force = ActionOptions {
        force: true,
        ..Default::default()
    };
    let t = Instant::now();
    match r.check(force.clone()).await {
        Ok(()) => println!(
            "check force: {:?} checked={:?}",
            t.elapsed(),
            r.is_checked().await?
        ),
        Err(e) => println!(
            "check force FAILED: {:?} {}",
            t.elapsed(),
            e.to_string().lines().next().unwrap_or("")
        ),
    }
    let t = Instant::now();
    match r.click(force.clone()).await {
        Ok(()) => println!(
            "click force: {:?} checked={:?}",
            t.elapsed(),
            r.is_checked().await?
        ),
        Err(e) => println!(
            "click force FAILED: {:?} {}",
            t.elapsed(),
            e.to_string().lines().next().unwrap_or("")
        ),
    }
    let t = Instant::now();
    match page
        .locator("label[for=\"_r_b_\"]")
        .click(ActionOptions::default())
        .await
    {
        Ok(()) => println!(
            "label click: {:?} checked={:?}",
            t.elapsed(),
            r.is_checked().await?
        ),
        Err(e) => println!(
            "label click FAILED: {:?} {}",
            t.elapsed(),
            e.to_string().lines().next().unwrap_or("")
        ),
    }
    println!("bbox radio: {:?}", r.bounding_box().await?);
    drop(launched);
    Ok(())
}
