---
name: browser-credential-handoff
description: Use Stead's brokered credential and third-party password-manager paths without exposing secrets to the model.
---

Use this skill when a browser task involves sign-in, password managers, TOTP, passkeys, payment fields, or any credential-like form.

Core rule: never ask the user to paste passwords, OTPs, cookies, recovery codes, card numbers, or session tokens into chat.

Preferred native path:

1. In `browser_exec`, identify username/password/TOTP fields with semantic locators such as `page.getByLabel(...)` or `page.getByRole('textbox', ...)`.
2. Call `stead.credentials.list()` for the current tab origin.
3. If the backend returns `credential_backend_unavailable`, explain that native Vault backing is not wired yet and offer a third-party password-manager flow.
4. If a credential handle is available, call `stead.credentials.fill(credential, usernameLocator, passwordLocator)` or `stead.credentials.fillTotp(credential, fieldLocator)`.
5. After any credential fill, assume the frame is tainted. Do not use screenshots, `page.evaluate`, broad snapshots with values, or raw input to inspect that frame.

Third-party manager path:

1. Use only browser-mediated actions inside `browser_exec`. Typical manager shortcuts or extension UI interactions must go through `page.keyboard`, semantic locators, or `page.mouse` after confirmation.
2. Once the manager injects a secret into the page, immediately call `stead.credentials.markInjected(page)`.
3. Continue the login with semantic actions only when possible.
4. Verify by navigation/session state, not by reading secret-bearing fields.

If the browser broker blocks an action as `secret_tainted`, do not work around it. Tell the user that Stead is intentionally preventing post-fill secret readback.
