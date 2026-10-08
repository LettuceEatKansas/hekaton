# Installing winhands on a Mac

winhands lets an AI agent (Claude Code, Claude Desktop) see your screen and use the mouse and keyboard.
On a Mac it is installed as a small app called `winhands.app`, so macOS can remember, in one place, that
you allowed it. This guide gets you from nothing to working with one install script and two tick boxes.

## What you need first (once)

- A Mac running macOS 14 (Sonoma) or newer, Apple Silicon or Intel.
- The Xcode command line tools. In Terminal: `xcode-select --install`
- Rust. In Terminal: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh` then open a new Terminal window.
- Access to the private GitHub repo. In Terminal: `gh auth login` (install the GitHub CLI first with
  `brew install gh` if you do not have it).

No Rust and no wish to install it? See "Install without Rust" below.

## Install: one command

Paste this into Terminal:

```
gh repo clone LettuceEatKansas/winhands-rs ~/winhands-rs -- --branch mac && ~/winhands-rs/packaging/macos/install.sh
```

(`mac` is the branch to use until it is merged. After that, replace `mac` with `main`.)

The script does this, in order, and tells you what it is doing:

1. Builds winhands for both kinds of Mac (Apple Silicon and Intel) and joins them into one app. The first
   build compiles everything from scratch, so it is the slow one. Later updates reuse most of that work.
2. Asks once to create a small signing certificate called `winhands-local` in your login keychain. Say yes.
   See "Why a signing certificate" below. It then asks for your Mac login password so macOS lets the build
   use that certificate without a pop-up each time. You can press Enter to skip; you will then see one
   pop-up on the build, where you click "Always Allow".
3. Signs the app, copies it to `~/Applications/winhands.app`, and registers it with macOS so it can be started
   when an AI client needs it. It does not run in the background until then.
4. Opens System Settings twice so you can switch on the two permissions (next section).
5. Checks the permissions and prints the exact lines to connect Claude Code and Claude Desktop.

## The two permission boxes

macOS will not let any app control your computer until you say so. winhands needs two permissions. Both
live in System Settings, under Privacy & Security. The script opens each one for you.

| Permission | What it lets winhands do | Why it needs it |
| --- | --- | --- |
| Accessibility | Read the buttons, menus and text of other apps, and click and type | This is how the agent understands a window and acts on it |
| Screen Recording | Take screenshots | This is how the agent sees what is on screen |

For each one: find `winhands` in the list and switch it on. If `winhands` is not in the list, click the `+`
button under the list, choose `winhands` from `Applications` in your home folder (a Finder window showing it
opens for you), and click Open. If macOS says winhands must quit and reopen, choose Later. Nothing is
running yet, so there is nothing to restart.

You do this once. macOS may also show a "Background Items Added" notice. That is the winhands start-up entry;
leave it on (System Settings, General, Login Items & Extensions).

## Finish: acknowledge and connect

The install script prints these lines with your real paths filled in. They are:

1. Acknowledge the safety notice, once, in Terminal:
   `~/Applications/winhands.app/Contents/MacOS/winhands acknowledge`
2. Connect Claude Code:
   `claude mcp add winhands -- ~/Applications/winhands.app/Contents/MacOS/winhands mcp`
3. Or connect Claude Desktop: add this to `~/Library/Application Support/Claude/claude_desktop_config.json`
   inside `mcpServers`, then quit and reopen Claude Desktop (use your real home folder path, not `~`):

```
"winhands": {
  "command": "/Users/YOU/Applications/winhands.app/Contents/MacOS/winhands",
  "args": ["mcp"]
}
```

## Updating without losing the permissions

```
cd ~/winhands-rs && git pull && packaging/macos/install.sh
```

macOS ties your permissions to three things: the app's name (`com.lettuceatkansas.winhands`), where it lives
(`~/Applications`), and which certificate signed it. An update keeps all three, so the permissions stay.
You do not need to tick the boxes again.

Things that make macOS ask again (just tick the two boxes once more):

- Building without the `winhands-local` certificate (the script warns loudly when it falls back to this).
- Deleting that certificate from Keychain Access, or reinstalling macOS, or moving to a new Mac.
- Moving or renaming `winhands.app`.

## Install without Rust

The GitHub build server makes a ready-built copy on every push. Download the newest one, unzip it, and
install it. The install script signs it with your local certificate, so permissions still stay put.

```
gh repo clone LettuceEatKansas/winhands-rs ~/winhands-rs -- --branch mac
cd ~/winhands-rs
run=$(gh run list --repo LettuceEatKansas/winhands-rs --workflow mac-app.yml --branch mac --status success --limit 1 --json databaseId --jq '.[0].databaseId')
gh run download "$run" --repo LettuceEatKansas/winhands-rs --name winhands-app --dir /tmp/winhands-ci
unzip -q -o /tmp/winhands-ci/winhands.app.zip -d /tmp/winhands-ci
packaging/macos/install.sh --app /tmp/winhands-ci/winhands.app
```

## Why a signing certificate

macOS remembers a permission against the app's digital signature. An unsigned or "ad hoc" signed app gets a
different signature on every build, so macOS thinks each update is a stranger and drops the permissions.
Signing every build with the same certificate keeps the signature, and so the permissions, the same. The
certificate is made on your Mac, lives in your login keychain, is only used to sign winhands, and never
leaves the machine. (An Apple Developer ID certificate works too: pass `--identity "Your Cert Name"`.)

## Checking it works

- `~/Applications/winhands.app/Contents/MacOS/winhands doctor` prints the permission status. The install
  script runs this for you as the app itself, which is the check that counts.
- `launchctl print gui/$(id -u)/com.lettuceatkansas.winhands` shows macOS knows about the start-up entry.
- Logs: `~/Library/Logs/winhands/daemon.log`.

## Troubleshooting

**macOS asks for the permissions again after an update.** The signature changed. Run
`security find-identity -p codesigning` and look for `winhands-local`. If it is missing, run
`packaging/macos/make-signing-identity.sh`, then install again, then tick the two boxes one last time. If
the build said "AD HOC SIGNATURE" near the top, that was the cause.

**Permissions still reset even though `winhands-local` exists.** Not seen in testing, but if it happens, tell
macOS to trust the certificate for code signing: open Keychain Access, find `winhands-local` in the login
keychain, double-click it, expand Trust, set Code Signing to Always Trust, then tick the two boxes once more.

**Screenshots are blank, black, or only show the wallpaper.**
1. Screen Recording is off or off for a different copy of winhands. In System Settings, Privacy & Security,
   Screen Recording: switch `winhands` off and on again, then restart it with
   `launchctl kickstart -k gui/$(id -u)/com.lettuceatkansas.winhands`.
2. The Mac has no display to capture: a Mac mini with no monitor, or a laptop with the lid closed and no
   external screen. Plug in a monitor or an HDMI "dummy plug", or open the lid.
3. The screen is locked or the display is asleep. macOS only shows the lock screen then.

**Typing does nothing in a password box, or only some apps ignore typing.** That is macOS Secure Input.
While a password field is focused, or while an app has "Secure Keyboard Entry" switched on (Terminal has it
in its menu; some password managers turn it on), macOS blocks other programs from typing there. This is on
purpose, and winhands cannot and will not type passwords. To find the app that is holding Secure Input on:
`ioreg -l -w 0 | grep SecureInput` (the number after `kCGSSessionSecureInputPID` is the process), then
`ps -p THAT_NUMBER`.

**`winhands` is missing from the Accessibility or Screen Recording list.** Click `+` and add
`~/Applications/winhands.app` by hand. An app only appears in these lists after it has asked once or been added.

**The agent cannot start winhands.** Check the start-up entry is on: System Settings, General, Login Items &
Extensions, find `winhands`, switch it on. Then run the `launchctl print` line above, and read
`~/Library/Logs/winhands/daemon.log`.

**"winhands is damaged" or "cannot be opened".** Only happens to a copy downloaded through a browser. The
install script clears this for you. By hand: `xattr -dr com.apple.quarantine ~/Applications/winhands.app`.

**Windows that cannot be controlled.** Password prompts, System Settings privacy panes and macOS
authorisation dialogs are protected by macOS itself. winhands will not be able to click inside them.

## Removing winhands

```
~/winhands-rs/packaging/macos/uninstall.sh
```

This stops winhands and deletes the app and its start-up entry. It leaves the two permission entries and
the certificate, and tells you how to remove them. Add `--reset-permissions` to remove the permission
entries too.
