# Vendored Playwright in-page scripts

Source: `playwright-core` 1.63.0 (Apache-2.0, see LICENSE), files
`lib/generated/injectedScriptSource` (embedded as `source4`) and `utilityScriptSource` (embedded as `source3`) in
`lib/coreBundle.js`. Extracted verbatim by evaluating the embedded string
literals (`scratchpad/pw/extract.js`); no modifications.

Each file evaluates to a CommonJS-style module: evaluating the source in a
page context yields `module.exports` with `InjectedScript` /
`UtilityScript` constructors. steadwright installs them into an isolated
world per frame and calls them via `Runtime.callFunctionOn`.

Update procedure: download the npm tarball, re-run the extractor, bump the
version here, re-run the steadwright test suite.
