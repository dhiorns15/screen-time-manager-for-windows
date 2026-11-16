# Screen Time Manager

[![CI](https://github.com/dhiorns15/screen-time-manager-for-windows/actions/workflows/ci.yml/badge.svg)](https://github.com/dhiorns15/screen-time-manager-for-windows/actions/workflows/ci.yml)

A simple Windows app to manage your child's computer time. Set daily limits, and when time's up, the screen is blocked until you enter the passcode.

No complicated setup. No special accounts needed. Just install it and configure your limits.

Confirmed to be effectively blocking Fortnite, Roblox and Minecraft.

Originally created by Simon Pamies ([spamsch/screen-time-manager-for-windows](https://github.com/spamsch/screen-time-manager-for-windows)); this project continues from his work.

---

## What It Does

- **Daily Time Limits** - Set different limits for each day (e.g., 2 hours on school days, 4 hours on weekends)
- **Timer Display** - A small timer in the corner shows remaining time
- **Warnings** - Alerts your child before time runs out ("10 minutes left!")
- **Screen Block** - When time's up, the screen is blocked until you enter the passcode
- **Extra Time** - Grant +15, +30, or +60 minutes when needed
- **Pause Feature** - Kids can pause the timer for breaks (with built-in limits to prevent abuse)
- **Shut Down Option** - Shut down the computer directly from the lock screen
- **Works on All Monitors** - Blocks all connected screens
- **Pauses at the Lock Screen** - Time isn't counted while Windows is locked, so your child's account being signed out doesn't burn through their daily limit

---

## Installing

You don't need to know anything about programming to install it.

1. Go to the [latest release](https://github.com/dhiorns15/screen-time-manager-for-windows/releases/latest) and, under **Assets**, download `screen-time-manager-vX.Y.Z-windows-x64-setup.exe` (X.Y.Z is the version number).
2. Run the downloaded file. Your browser or Windows may warn you about it first - see [Antivirus & Windows SmartScreen Warnings](#antivirus--windows-smartscreen-warnings) below.
3. Click **Yes** when Windows asks whether to let it make changes. Setup needs Administrator rights, so if you run it from your child's (standard) account, Windows asks for an Administrator's password.
4. Follow the steps in the setup window.

That's it. Setup does everything the app needs, for every Windows account on the PC:

- **Installs the app** to `C:\Program Files\ScreenTimeManager`, where only an Administrator can change or remove it.
- **Starts it automatically** whenever anyone signs in to Windows - every account, not just the one you installed from. With several kids on separate accounts, one install covers all of them, even when they're signed in at the same time (Fast User Switching).
- **Sets up the watchdog**, which relaunches the app if it's stopped and alerts you (see [Automatic Restart & Tamper Alerts](#automatic-restart--tamper-alerts)).
- **Creates the shared settings folder** (`C:\ProgramData\ScreenTimeManager`) that every account can use, so the passcode and the Telegram/Discord settings are set up once for the whole PC.

**Upgrading from an older version that you installed with `install.ps1`?** Just run the new setup. It takes over the scheduled tasks the old script made and keeps your settings; afterwards you can delete the folder you unzipped the old version into.

---

## Getting Started

1. **Find the timer** - While the app is running, a small timer shows in the top-right corner. If you don't see it right after installing, sign out of Windows and back in.

2. **Find the tray icon** - Look for the clock icon in the system tray (bottom-right of your screen, near the clock).

   ![System tray icon](images/tray.png)

3. **Open the menu** - Right-click the tray icon to see all options.

   ![Tray menu](images/menu.png)

4. **Open Settings** - Click "Settings..." and enter the passcode (default is `0000`).

   ![Passcode entry](images/passcode-entry.png)

5. **Configure your limits** - Set daily time limits, warning messages, and optionally set up Telegram or Discord remote control.

   ![Settings dialog](images/settings.png)

6. **Change the passcode right away** - Use the "Change Passcode" section to set something your child won't guess! Until you do, anyone who has read this page knows it's `0000`, and Settings shows a red reminder. This passcode is shared machine-wide - if you have multiple kids on separate Windows accounts on the same PC, changing it from any one of their Settings dialogs changes it for all of them.

---

## When Time Runs Out

A "Time's Up!" screen appears that blocks the entire computer.

![Lock screen](images/lock-screen.png)

- **Extend buttons** (+15, +30, +60 min) - Enter passcode to grant more time
- **Unlock button** - Enter passcode to remove the block completely
- **Shut Down button** - Shut down the computer (with confirmation)

The screen also counts down (10 minutes by default; change it in Settings, from 1 minute up to 24 hours) and then shuts the computer down. If another Windows account is signed in at the same time (Fast User Switching), it signs your child's account out instead, so nobody else's work is lost. The countdown only runs while the blocked account is the one on screen and Windows isn't locked.

Wrong passcodes are limited here just like in the tray menu: after five in a row, the screen shows how long to wait before the next try, and you get an alert (see [Automatic Restart & Tamper Alerts](#automatic-restart--tamper-alerts)).

---

## The Pause Feature

Your child can pause the timer for meals, homework, or breaks without needing your passcode.

**Built-in limits prevent abuse:**
- 45 minutes total pause time per day
- Each pause auto-resumes after 20 minutes
- Must wait 15 minutes between pauses

You can change all three in Settings, under "Pause Timer". Set the daily pause to 0 to turn pausing off (a pause that's running then ends straight away).

**Auto-pause when idle** stops the timer after a few minutes without keyboard or mouse input. It's off by default on new installs, because playing with a controller or watching a video doesn't count as input either; turn it on in Settings, under "Idle Detection", if you want it.

You can view pause usage in "Today's Stats..." from the tray menu.

---

## Viewing Stats

Right-click the tray icon and select "Today's Stats..." to see:
- Time used today
- Time remaining
- Pause usage
- Option to reset the timer

---

## Remote Control via Telegram (Optional)

You can monitor and control screen time from your phone using Telegram. This is useful when you're not near the computer.

**What you can do from Telegram:**
- `/status` - Check remaining time and whether the timer is running, paused or stopped
- `/time` - Quick time check
- `/extend 30` - Add extra time (e.g., 30 minutes; up to 120 at a time). `/e30`, `/e60` and `/e120` are shortcuts
- `/reduce 30` - Take time away (up to 120 minutes at a time)
- `/reset` - Set the remaining time back to today's full limit
- `/lock` (or `/stop`) - Show the lock screen now
- `/unlock` - Dismiss the lock screen without changing remaining time (only works if there's still time left)
- `/pause` - Pause the timer
- `/resume` - Resume the timer
- `/history` - See today's pause activity
- `/weekly` - A table of the last two weeks' use, for every account on the PC
- `/msg Dinner is ready!` - Show a message on the screen for a few seconds. Any text you send that isn't a command does the same
- `/setmessage Time for a break!` - Change the blocking screen message remotely
- `/setpin 1234` - Change the passcode remotely
- `/setlimit 90` or `/setlimit saturday 180` - Change the daily time limit remotely (today, or a specific day)
- `/rotatingpin on` and `/getpin` - Optional auto-rotating daily code (see below)
- `/update` - Install a new version of the app, after the bot tells you one is out (see [Updating](#updating))
- `/clearlockout` - Let the next passcode be checked straight away after too many wrong codes locked passcode entry
- `/chatid` - Show this chat's ID (works before the bot is set up)
- `/help` - See all commands

Command names don't care about upper or lower case (`/Extend 30` works too). Replies to commands that change or read the timer, limits or lock screen say which Windows account they acted on.

**Setup (one-time):**

1. Open Telegram and search for **@BotFather**
2. Send `/newbot` and follow the steps to create your bot
3. Copy the **bot token** you receive
4. Start a chat with your new bot and send `/start`
5. Note your **chat ID** shown in the reply
6. In Screen Time Manager settings, scroll to "Telegram Bot" section (or use the **Setup Wizard**, which finds the chat ID for you)
7. Paste your bot token and chat ID, then enable the bot

The bot starts as soon as you save. Once configured, only you can control the bot - it ignores messages from anyone else. If you set it up in a **group** (for example a family group, so both parents get the alerts), only the group's owner and administrators can send it commands; ordinary members, such as a child you added to the group, can't. The bot never posts a PIN in a group, where everyone in it could read it: `/getpin` sent in a group is answered in your private chat with the bot (start one first, since Telegram doesn't let a bot message you until you have), and a `/setpin` message is deleted from the group if the bot is a group admin allowed to delete messages.

The Setup Wizard takes the chat from the first message the bot receives while the wizard is waiting for it, so send that message after you reach that step - anything sent to the bot earlier is ignored.

Commands are acted on only if they reach the PC within a few minutes. Anything you sent while the PC was off, asleep or offline is dropped when it comes back, rather than suddenly running then - for whichever account happens to be signed in by that time. Send the command again once the PC is on. The same goes for a command sent in the few seconds while the app restarts or the PC switches to another account (on Telegram and Discord alike): it gets no reply, so if you don't get one, send it again.

**Note:** the bot token/chat ID - like the passcode and rotating PIN - are stored machine-wide (shared across every Windows account on the PC, unlike time limits, which are per-account) - set it up once and it works no matter which child's account is running the app. Setup gives every account access to the shared settings folder; a copy of the app that wasn't installed with setup silently falls back to per-account storage instead.

---

## Remote Control via Discord (Optional)

You can also monitor and control screen time from a Discord channel, using a Discord bot instead of Telegram.

**What you can do from Discord:**
- `!status` - Check remaining time and whether the timer is running, paused or stopped
- `!time` - Quick time check
- `!extend 30` - Add extra time (e.g., 30 minutes; up to 120 at a time). `!e30`, `!e60` and `!e120` are shortcuts
- `!reduce 30` - Take time away (up to 120 minutes at a time)
- `!reset` - Set the remaining time back to today's full limit
- `!lock` (or `!stop`) - Show the lock screen now
- `!unlock` - Dismiss the lock screen without changing remaining time (only works if there's still time left)
- `!pause` - Pause the timer
- `!resume` - Resume the timer
- `!history` - See today's pause activity
- `!weekly` - A table of the last two weeks' use, for every account on the PC
- `!msg Dinner is ready!` - Show a message on the screen for a few seconds
- `!setmessage Time for a break!` - Change the blocking screen message remotely
- `!setpin 1234` - Change the passcode remotely
- `!setlimit 90` or `!setlimit saturday 180` - Change the daily time limit remotely (today, or a specific day)
- `!rotatingpin on` and `!getpin` - Optional auto-rotating daily code (see below)
- `!update` - Install a new version of the app, after the bot tells you one is out (see [Updating](#updating))
- `!clearlockout` - Let the next passcode be checked straight away after too many wrong codes locked passcode entry
- `!whoami` - Show your Discord user ID (works before the bot is set up)
- `!help` - See all commands

As on Telegram, command names don't care about case, and replies to commands that change or read the timer, limits or lock screen say which Windows account they acted on.

**Setup (one-time):**

1. Go to [discord.com/developers/applications](https://discord.com/developers/applications) and click **New Application**
2. Open the **Bot** tab, enable **Message Content Intent**, click **Reset Token**, and copy it
3. Under **OAuth2 > URL Generator**, check the **bot** scope plus the **Send Messages**, **View Channels**, **Read Message History** and **Manage Messages** permissions, then open the generated URL to invite the bot to your server (Manage Messages lets the bot delete your `!setpin` messages - see below)
4. In Discord, enable **Developer Mode** (User Settings > Advanced), then right-click the channel you want to use and choose **Copy Channel ID**, and right-click your own username and choose **Copy User ID**
5. In Screen Time Manager settings, scroll to the "Discord Bot" section (or use the **Setup Wizard**) and paste your bot token, channel ID, and user ID, then enable the bot

The bot starts as soon as you save. Once configured, only you can control the bot in that channel - it ignores commands from anyone else.

**Use a channel only parents can see.** Anyone who can read the channel sees everything the bot posts there: alerts, your commands and its replies. Codes are kept out of it where possible: `!getpin` sends today's rotating PIN to you by direct message (allow direct messages from server members if it says it couldn't), and the bot deletes your `!setpin` message so the new passcode doesn't stay in the channel (if it can't, because it lacks the Manage Messages permission, it tells you to delete it yourself). Still, don't use a channel your child (or their friends) can open.

**Note:** the bot token/channel/user ID - like the passcode and rotating PIN - are stored machine-wide (shared across every Windows account on the PC, unlike time limits, which are per-account) - set it up once and it works no matter which child's account is running the app. Setup gives every account access to the shared settings folder; a copy of the app that wasn't installed with setup silently falls back to per-account storage instead. Since the config is shared, `!status`/`!history` (and their Telegram equivalents) show which Windows account is currently logged in, so you can tell whose stats you're looking at. If more than one of your kids' accounts is signed in at once (e.g. via Fast User Switching), only the one currently in the foreground answers bot commands, so you're never talking to two accounts' bots at the same time.

---

## Requesting More Time From the Lock Screen

If Telegram and/or Discord is enabled, the lock screen shows a **"Request More Time"** button that doesn't need the passcode. Your child can optionally type a short reason (e.g. "saving my game"), then click it to ping you directly - the message includes their Windows account, remaining time, and the reason if given. Reply `!extend 30` (Discord) or `/extend 30` (Telegram) to grant time, or `!unlock`/`/unlock` to dismiss the screen without changing the timer if there's still time left - either one closes the lock screen automatically. The button has a 5-minute cooldown to prevent spamming, and while a request is still unanswered, pressing it again sends you a reminder at most every 15 minutes. If the PC is offline at that moment, the request is sent once it's back online, like the alerts below. The reason is shown exactly as typed: it can't ping anyone or show links or formatting on Discord.

If both Telegram and Discord are enabled, the request goes to both, but only one reply is needed - whichever one you respond from first, the other gets a short "already handled" notice so you don't have to reply twice. A request counts as unanswered for an hour (and not past midnight); time you grant after that doesn't send the notice.

**Passcode alerts:** if Telegram and/or Discord is enabled, using the passcode to add time - either on the lock screen or via the tray icon's right-click menu - sends you a notification too. This is separate from the request flow above, so you find out any time the passcode grants time, whether or not anyone asked first.

---

## Rotating Daily PIN (Optional)

If you're worried about the passcode being watched and memorized, enable the rotating daily PIN (Settings, or `!rotatingpin on` / `/rotatingpin on`) - a second code that automatically changes every day and works anywhere the regular passcode does. Your regular passcode always keeps working too, so there's no risk of getting locked out if you don't have your phone handy - the rotating code is purely additive. Retrieve today's code any time with `!getpin` / `/getpin`. Off by default. Like the passcode, this is shared machine-wide - enabling it (or fetching today's code) from one Windows account applies to all of them.

---

## Automatic Restart & Tamper Alerts

If Screen Time Manager is stopped in any way other than the passcode-protected Quit option (e.g. ended via Task Manager), it's automatically relaunched within about a minute, and you'll get a Telegram/Discord alert if either is enabled. Setup turns this on for **every** Windows account on the PC, including accounts signed in at the same time via Fast User Switching. Installing an update doesn't count as tampering: the app restarts afterwards without an alert.

The watchdog also notices when the app is still there but has stopped working - for example frozen, or suspended with a process tool - after a few minutes. It restarts the app and tells you once. It only does this while someone is actually using the PC, so sleep, hibernation, restarts, a locked screen, switching users and a PC left alone don't trigger it. If the app keeps freezing again straight after being restarted, the watchdog stops restarting it after a few times in an hour and tells you so once, so you can check the PC.

Other alerts you may get:

- **Wrong passcodes** - after five wrong codes in a row (on the lock screen or in the tray's passcode prompt), each further wrong code makes passcode entry wait before it accepts another try (up to 15 minutes), and you get an alert, since someone may be guessing (and another, at most once an hour, while the wait keeps being pushed to its maximum). While it waits, even the right code is refused, on every account. Send `/clearlockout` (Telegram) or `!clearlockout` (Discord) to clear the wait so the next code is checked straight away.
- **Clock or time zone changed** - changing the PC's clock or time zone doesn't start a new day with a fresh allowance: the app stays on the day it was on, and tells you. If you changed the time zone on purpose (say, you're travelling with the laptop), open Settings and click Save to accept it.
- **Damaged settings** - if an account's settings file can't be opened, the app doesn't stop: a damaged file is set aside and replaced with default settings (re-check the limits for that account), and if the file can't be opened at all the app runs on default settings until it can. Either way you're told. The time already used today is carried over, so this doesn't start the day's allowance over.

If the PC is offline when an alert is raised, it's kept and sent once the connection is back (anything more than a day old is dropped). The same goes for the passcode alerts above, the "out of screen time" notice, a time request and a stalled update. Notices that only matter at the moment they happen - low time, idle, the screen being locked or unlocked - are only sent if the bot is connected right then.

---

## What It Protects Against (and What It Doesn't)

Screen Time Manager is built to enforce limits for a child who uses the computer normally, and to make the obvious ways around it - ending it in Task Manager, signing out, restarting, guessing the passcode - either fail or get reported to you.

It is not a lock that a determined child can't pick. It runs as your child's own Windows account, so its files are in places that account can open. A child who goes looking through the file system can read the passcode and the Telegram/Discord settings, and can change or delete the app's files. The tamper alerts are the backstop: most ways of interfering with the app end up as an alert on your phone. A hardened design that keeps the passcode and bot settings out of the child's reach is planned.

A few more things worth knowing:

- The lock screen covers every monitor with a window that keeps itself on top. It does not block the keyboard or mouse, so Windows shortcuts still reach Windows.
- Your child needs a **standard** Windows account. On an Administrator account they can simply uninstall the app.
- Change the passcode from `0000` as soon as you've installed it.
- If you use Discord, keep the bot's channel visible only to parents (see above).

---

## Tips

- **Change the default passcode** - The default `0000` is easy to guess!
- **Set reasonable limits** - Too strict and kids get frustrated; too loose and they won't learn limits
- **Check stats occasionally** - See if pause mode is being used appropriately
- **The timer survives restarts** - Restarting the computer won't reset the timer

---

## Updating

**With Telegram or Discord set up:** when a new version comes out, the bot sends you a message. Reply `/update` (Telegram) or `!update` (Discord) to install it. Until you do, the bot reminds you once a day (from 7am, on any day the PC is on); on Discord the message mentions the admin user you set up, so it pings you. The app downloads the new setup from this project's [Releases page](https://github.com/dhiorns15/screen-time-manager-for-windows/releases), checks that it's exactly the file published there, and installs it. The timer disappears briefly and comes back on its own a minute or so later, on every signed-in account. Your settings, limits and today's remaining time are kept.

**Without a bot:** download the new `...-windows-x64-setup.exe` from the [latest release](https://github.com/dhiorns15/screen-time-manager-for-windows/releases/latest) and run it, just like the first time. It installs over the old version and keeps your settings.

---

## Uninstalling

Open **Settings > Apps > Installed apps** (on Windows 10, **Settings > Apps > Apps & features**), find **Screen Time Manager**, and choose **Uninstall**. Windows asks for Administrator approval, so a child on a standard account can't remove it.

Uninstalling removes the app, its automatic start and the watchdog. Your settings and history are left behind in case you reinstall; delete these folders yourself for a clean slate:

- `C:\ProgramData\ScreenTimeManager` - the shared settings: passcode, rotating PIN and Telegram/Discord setup
- `C:\Users\<name>\AppData\Local\.screen-time-manager` in each child's account - that account's time limits, usage history and logs (paste `%LOCALAPPDATA%\.screen-time-manager` into File Explorer while signed in as them)

---

## Antivirus & Windows SmartScreen Warnings

When you download or run the setup, your browser may say the file "isn't commonly downloaded", Windows SmartScreen may show "Windows protected your PC", and the Administrator prompt shows "Unknown publisher". Your antivirus may also flag it as suspicious. **This is a false positive.**

To get past them: in the browser, choose **Keep** (in Edge, under **...** next to the download); on the SmartScreen screen, click **More info** and then **Run anyway**; at the Administrator prompt, click **Yes**.

Here's why it happens: this app does things that look identical to malware when a scanner only looks at *behavior* and not *intent*. It covers the screen with a window that stays on top, can shut the computer down, keeps itself running, and accepts remote commands over Telegram and Discord. That's exactly what a parental-control tool needs to do, and also exactly what a scanner's generic "this looks like a trojan" heuristic is tuned to catch. The setup and the app are also unsigned (a code-signing certificate costs money), which removes the one signal that would tell the scanner who built them; SmartScreen also warns about any new download that few people have run yet.

It contains no malware. Nothing is hidden, and it only talks to the internet to reach Telegram's servers if you enable the Telegram bot, Discord's servers if you enable the Discord bot (both with *your own* tokens), and this project's GitHub page to check for new versions.

**If you don't want to take that on trust, don't. Build it yourself from source — then the exe you run is the code you can read.**

### Build It Yourself

1. Install [Rust](https://rustup.rs/) (the installer adds `cargo` to your PATH, and offers to install the Visual Studio C++ build tools Rust needs on Windows). The Rust version this project uses is pinned in `rust-toolchain.toml`; `cargo` downloads it the first time you build.
2. Clone or download this repository.
3. From the project folder, run:

   ```
   cargo build --release
   ```

4. The compiled app is at `target\release\screen-time-manager.exe`.
5. To install it the same way as a release, build the setup as well: install [Inno Setup 6](https://jrsoftware.org/isinfo.php) (`winget install JRSoftware.InnoSetup`), then run

   ```
   powershell -File installer\build.ps1
   ```

   and run the setup it writes to `target\installer\`. Running `screen-time-manager.exe` directly also works, but then nothing starts it at sign-in, there's no watchdog, and settings aren't shared between accounts.

The setup on the Releases page is built this same way, from the tagged source, on a clean GitHub Actions runner by this repository's [release workflow](.github/workflows/release.yml), whose logs are public. Building locally just lets you verify that for yourself.

Pull requests and every push are checked by the [CI workflow](.github/workflows/ci.yml): `cargo clippy --all-targets -- -D warnings`, `cargo test`, and a build of the setup that's installed, upgraded, updated and uninstalled on a Windows runner.

### Releasing

For maintainers:

1. Bump `version` in `Cargo.toml` (versions are `X.Y.Z`), and run `cargo check` so `Cargo.lock` picks up the new version too - the release builds with `--locked` and fails if it's out of date.
2. Commit both as `Release X.Y.Z`.
3. Tag the commit `vX.Y.Z` and push the tag: `git tag vX.Y.Z` then `git push origin vX.Y.Z` (push the commit to `main` too).

The [release workflow](.github/workflows/release.yml) refuses a tag that doesn't match the version in `Cargo.toml`. It tests and builds the release, publishes the setup with release notes listing every commit since the previous release (`ci/release-notes.sh`, so write commit subjects that make sense to people reading them there), and then checks that the previous release updates itself to the new one. Installed copies offer the new version to their admin from then on.

---

## Requirements

- Windows 10 or Windows 11 (64-bit)
- That's it!

---

## License

MIT - see [LICENSE](LICENSE). Originally written by Simon Pamies.
