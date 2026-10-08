# Installing winhands on a Mac

winhands lets an AI agent (Claude Code, Claude Desktop) see your screen and use the mouse and keyboard.
On a Mac it is installed as a small app, `winhands.app`, so macOS can remember in one place that you allowed
it. You run one command, switch on two permissions, and tell Claude about it.

## 1. What you need first (once)

You need a Mac on macOS 14 (Sonoma) or newer, Apple Silicon or Intel. Then, in Terminal:

1. Xcode command line tools (compilers): `xcode-select --install`
2. Rust (to build winhands): `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`, then open a new Terminal window.
3. GitHub access (the repo is private): `brew install gh` if you do not have it, then `gh auth login`.

## 2. Install: one command

```
gh repo clone LettuceEatKansas/winhands-rs ~/winhands-rs && ~/winhands-rs/packaging/macos/install.sh
```

It tells you what it is doing. In order, it:

1. Builds winhands for both kinds of Mac and joins them into one app. The first build is slow (several
   minutes). Later updates are much faster.
2. Asks to create a signing certificate called `winhands-local` in your login keychain. Say yes. It is made
   on your Mac and never leaves it. It then asks for your Mac login password so the build can use it without
   a pop-up; press Enter to skip, and click "Always Allow" on the one pop-up instead.
3. Signs the app, puts it in `~/Applications/winhands.app`, and registers a start-up entry with macOS. winhands
   does not run in the background; it starts when an AI client first uses it.
4. Asks macOS to show winhands in the two permission lists, opens System Settings for each, and waits for you.
5. Checks the permissions and prints the lines to connect Claude.

## 3. The two permission boxes

macOS blocks every app from controlling your computer until you say yes. Both switches are in System
Settings, under Privacy & Security. The script opens each one.

| Box | What it lets winhands do | Why |
| --- | --- | --- |
| Accessibility | Read the buttons, menus and text of other apps, and click and type | How the agent understands a window and acts on it |
| Screen Recording (called "Screen & System Audio Recording" on macOS 15 and later) | Take screenshots | How the agent sees the screen |

In each list, find `winhands` and switch it on. macOS may first show a pop-up from winhands: click "Open
System Settings". If `winhands` is not in a list, click `+`, choose `winhands` in the `Applications` folder
inside your home folder (a Finder window showing it opens for you), and click Open. If macOS says winhands
must quit and reopen, choose Later; nothing is running yet. You do this once.

You may also see "Background Items Added". That is winhands' start-up entry. Leave it on (System Settings,
General, Login Items & Extensions).

## 4. Connect Claude and acknowledge

The script prints these with your real paths. In short:

1. Acknowledge the safety notice, once: `~/Applications/winhands.app/Contents/MacOS/winhands acknowledge`
2. Claude Code: `claude mcp add winhands -- ~/Applications/winhands.app/Contents/MacOS/winhands mcp`
3. Claude Desktop: add this inside `mcpServers` in `~/Library/Application Support/Claude/claude_desktop_config.json`
   (use your real home path, not `~`), then quit and reopen Claude Desktop:

```
"winhands": {
  "command": "/Users/YOU/Applications/winhands.app/Contents/MacOS/winhands",
  "args": ["mcp"]
}
```

## 5. Test it

Start a new Claude chat and send: `use winhands to take a screenshot and list my open windows`

The first request takes a few seconds longer while macOS starts winhands. You should get a screenshot and a
list of windows. If you do not, see Troubleshooting. You can also check from Terminal:
`~/Applications/winhands.app/Contents/MacOS/winhands doctor` (every line should say OK; a daemon that is not
running yet is normal).

## 6. Update without losing the permissions

```
cd ~/winhands-rs && git pull && packaging/macos/install.sh
```

macOS ties your permissions to three things: the app's id (`com.lettuceatkansas.winhands`), where it lives
(`~/Applications`), and which certificate signed it. An update keeps all three, so you do not tick the boxes
again. macOS will ask again if you delete the `winhands-local` certificate (Keychain Access), move or rename
the app, reinstall macOS, or move to a new Mac. The install script warns "AD HOC SIGNATURE" if it had to build
without the certificate.

## Troubleshooting

**Claude says winhands cannot start, or nothing happens.** Check the start-up entry is loaded:
`launchctl print gui/$(id -u)/com.lettuceatkansas.winhands`. If it says it could not find the service, run the
install script again. Read the log: `~/Library/Logs/winhands/daemon.log`.

**"not allowed" or a permission error.** Run `winhands doctor` (the path in step 4) and look for WARN lines.
Switch the named box on in System Settings. If it is already on, switch it off and on, then restart winhands:
`launchctl kickstart -k gui/$(id -u)/com.lettuceatkansas.winhands`.

**Permissions reset after an update.** The signature changed. Run `security find-identity -p codesigning` and
look for `winhands-local`. If it is missing, run `~/winhands-rs/packaging/macos/make-signing-identity.sh`,
install again, and tick the two boxes one last time.

**`winhands` is not in a permission list.** Click `+` and add `~/Applications/winhands.app` by hand, or run
`~/winhands-rs/packaging/macos/install.sh` again, which asks macOS to list it.

**Screenshots are blank, black, or only the wallpaper.** Screen Recording is off (see above), or the Mac has no
display to capture (a Mac mini with no monitor, or a closed laptop with no external screen: plug in a monitor or
an HDMI dummy plug), or the screen is locked or asleep.

**Typing does nothing in a password box.** macOS Secure Input blocks other programs from typing there, on
purpose. winhands will not type passwords; type them yourself. Password prompts, privacy panes and macOS
authorisation dialogs cannot be clicked by software either.

**"winhands is damaged" or "cannot be opened".** Only happens to a copy downloaded in a browser:
`xattr -dr com.apple.quarantine ~/Applications/winhands.app`.

## Removing winhands

```
~/winhands-rs/packaging/macos/uninstall.sh
```

This deletes the app and its start-up entry. It leaves the permission entries and the certificate, and tells
you how to remove them (`--reset-permissions` removes the permission entries too).
