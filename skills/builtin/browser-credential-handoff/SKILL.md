---
name: browser-credential-handoff
description: Use Stead's brokered credential and third-party password-manager paths without exposing secrets to the model.
---

Use this skill when a browser task involves sign-in, password managers, TOTP, passkeys, payment fields, or any credential-like form.

Core rule: never ask the user to paste passwords, OTPs, cookies, recovery codes, card numbers, or session tokens into chat.

Preferred native path:

1. Use `browser.snapshot` to identify username/password/TOTP fields as `NodeRef`s.
2. Call `browser.list_credentials` for the current tab and tuple origin.
3. If the backend returns `credential_backend_unavailable`, explain that native Vault backing is not wired yet and offer a third-party password-manager flow.
4. If a credential handle is available, call `browser.fill_credential` or `browser.fill_totp`.
5. After any credential fill, assume the frame is tainted. Do not use screenshots, `browser.eval`, broad probes, or raw input to inspect that frame.

Third-party manager path:

1. Use only browser-mediated actions. Typical manager shortcuts or extension UI interactions must go through brokered `browser.key`, semantic clicks, or raw input after confirmation.
2. Once the manager injects a secret into the page, immediately call `browser.mark_credential_injection` for the target frame.
3. Continue the login with semantic actions only when possible.
4. Verify by navigation/session state, not by reading secret-bearing fields.

If the browser broker blocks an action as `secret_tainted`, do not work around it. Tell the user that Stead is intentionally preventing post-fill secret readback.
