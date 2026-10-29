# Native release checks

The ordinary build workflow verifies shared logic on hosted Windows, macOS, and Linux runners. Its exploratory macOS app test remains nonblocking because hosted runners do not guarantee an unlocked desktop or previously granted accessibility and screen-recording permissions. Green ordinary CI is not evidence of successful native desktop interaction.

`native-acceptance.yml` adds a separate manually dispatched workflow. It has no push or pull-request trigger. No workflow has been dispatched as part of this source change.

## Runner requirements

Register dedicated self-hosted runners with the `hands-native` label and the appropriate `Windows` or `macOS` operating-system label. Use disposable desktops without personal documents, accounts, active games, or other people working. The desktop must be logged in and unlocked. On macOS, grant Accessibility and Screen Recording permissions to the runner's actual test execution identity before accepting its results. The Mac tests open, modify, and quit Calculator and TextEdit.

Configure the `hands-native-windows` and `hands-native-macos` repository environments with an owner approval requirement. The workflow's explicit confirmation checkbox is an additional guard, not a replacement for physical isolation. Never attach an untrusted pull-request runner to a personal desktop. Workflow dispatch selects one platform or both. A runner must be present for every selected platform.

## What currently counts

Windows runs the ignored passive capture and accessibility suites. These check native observation paths. They do not demonstrate a full Windows click, type, save, and verify journey. That interactive Windows acceptance scenario remains required before calling the release fully validated.

macOS runs smoke checks and the ignored Calculator/TextEdit scenarios with `HANDS_REQUIRE_NATIVE=1`. Strict tests fail on operation errors, missing grants, early exits, and their required behavioral assertions. The workflow additionally rejects logged `FAIL` operations and known early-return messages, requires typing and window-restoration operation markers, and requires both strict scenario PASS markers. These checks cover their explicit assertions; they do not establish every application or display configuration.

Each selected job must pass on the exact source revision intended for release. Preserve its logs with the artifact hashes and source identity. A missing runner, missing permissions, skipped scenario, stale revision, or incomplete test coverage means native release verification remains OPEN. No release-publication workflow currently consumes this result automatically. The release owner must check it before publishing.

## Local commands for a disposable session

Run only when the machine is reserved for testing:

```text
cargo test --locked -p wh-win --test passive -- --ignored --nocapture --test-threads=1
cargo test --locked -p wh-win --test uia_passive -- --ignored --nocapture --test-threads=1
cargo test --locked -p wh-mac --test macos_smoke -- --nocapture --test-threads=1
cargo test --locked -p wh-mac --test macos_e2e -- --ignored --nocapture --test-threads=1
```

Only use commands for the current operating system. Set `HANDS_REQUIRE_NATIVE=1` in the macOS test environment for the strict native lane. Run results, timestamps, hardware/display configuration, permission state, and unresolved failures belong in the release evidence. Do not infer a pass from the presence of this workflow file.
