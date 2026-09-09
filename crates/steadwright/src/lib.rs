#![deny(unsafe_code)]

//! A small, transport-agnostic Playwright-style Chromium controller.

mod aria_yaml;
mod browser;
mod error;
mod input;
mod locator;
mod selectors;
mod types;

pub use browser::{
    Browser, BrowserContext, Dialog, Download, ElementHandle, FileChooser, Frame, JsHandle, Page,
};
pub use error::{Deadline, Error, Result};
pub use input::{Keyboard, Mouse, Touchscreen};
pub use locator::{FrameLocator, Locator};
pub use selectors::*;
pub use types::*;
