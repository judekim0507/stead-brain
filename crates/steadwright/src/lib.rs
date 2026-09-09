#![deny(unsafe_code)]

//! A small, transport-agnostic Playwright-style Chromium controller.

mod browser;
mod error;
mod input;
mod types;

pub use browser::{
    Browser, BrowserContext, Dialog, Download, ElementHandle, FileChooser, Frame, JsHandle, Page,
};
pub use error::{Deadline, Error, Result};
pub use input::{Keyboard, Mouse, Touchscreen};
pub use types::*;
