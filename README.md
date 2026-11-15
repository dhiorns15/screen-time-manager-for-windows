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

6. **Change the passcode** - Use the "Change Passcode" section to set something your child won't guess! This passcode is shared machine-wide - if you have multiple kids on separate Windows accounts on the same PC, changing it from any one of their Settings dialogs changes it for all of them.

---

## When Time Runs Out

A "Time's Up!" screen appears that blocks the entire computer.

![Lock screen](images/lock-screen.png)

- **Extend buttons** (+15, +30, +60 min) - Enter passcode to grant more time
- **Unlock button** - Enter passcode to remove the block completely
- **Shut Down button** - Shut down the computer (with confirmation)

---

## The Pause Feature

Your child can pause the timer for meals, homework, or breaks without needing your passcode.

**Built-in limits prevent abuse:**
- 45 minutes total pause time per day
- Each pause auto-resumes after 20 minutes
- Must wait 15 minutes between pauses

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
- `/status` - Check remaining time and pause status
- `/time` - Quick time check
- `/extend 30` - Add extra time (e.g., 30 minutes)
- `/unlock` - Dismiss the lock screen without changing remaining time (only works if there's still time left)
- `/pause` - Pause the timer
- `/resume` - Resume the timer
- `/history` - See today's pause activity
- `/setmessage Time for a break!` - Change the blocking screen message remotely
- `/setpin 1234` - Change the passcode remotely
- `/setlimit 90` or `/setlimit saturday 180` - Change the daily time limit remotely (today, or a specific day)
- `/rotatingpin on` and `/getpin` - Optional auto-rotating daily code (see below)
- `/update` - Install a new version of the app, after the bot tells you one is out (see [Updating](#updating))

**Setup (one-time):**

1. Open Telegram and search for **@BotFather**
2. Send `/newbot` and follow the steps to create your bot
3. Copy the **bot token** you receive
4. Start a chat with your new bot and send `/start`
5. Note your **chat ID** shown in the reply
6. In Screen Time Manager settings, scroll to "Telegram Bot" section
7. Paste your bot token and chat ID, then enable the bot

Once configured, only you can control the bot - it ignores messages from anyone else.

**Note:** the bot token/chat ID - like the passcode and rotating PIN - are stored machine-wide (shared across every Windows account on the PC, unlike time limits, which are per-account) - set it up once and it works no matter which child's account is running the app. Setup gives every account access to the shared settings folder; a copy of the app that wasn't installed with setup silently falls back to per-account storage instead.

---

## Remote Control via Discord (Optional)

You can also monitor and control screen time from a Discord channel, using a Discord bot instead of Telegram.

**What you can do from Discord:**
- `!status` - Check remaining time and pause status
- `!time` - Quick time check
- `!extend 30` - Add extra time (e.g., 30 minutes)
- `!unlock` - Dismiss the lock screen without changing remaining time (only works if there's still time left)
- `!pause` - Pause the timer
- `!resume` - Resume the timer
- `!history` - See today's pause activity
- `!setmessage Time for a break!` - Change the blocking screen message remotely
- `!setpin 1234` - Change the passcode remotely
- `!setlimit 90` or `!setlimit saturday 180` - Change the daily time limit remotely (today, or a specific day)
- `!rotatingpin on` and `!getpin` - Optional auto-rotating daily code (see below)
- `!update` - Install a new version of the app, after the bot tells you one is out (see [Updating](#updating))
- `!help` - See all commands

**Setup (one-time):**

1. Go to [discord.com/developers/applications](https://discord.com/developers/applications) and click **New Application**
2. Open the **Bot** tab, enable **Message Content Intent**, click **Reset Token**, and copy it
3. Under **OAuth2 > URL Generator**, check the **bot** scope plus the **Send Messages**, **View Channels**, and **Read Message History** permissions, then open the generated URL to invite the bot to your server
4. In Discord, enable **Developer Mode** (User Settings > Advanced), then right-click the channel you want to use and choose **Copy Channel ID**, and right-click your own username and choose **Copy User ID**
5. In Screen Time Manager settings, scroll to the "Discord Bot" section (or use the **Setup Wizard**) and paste your bot token, channel ID, and user ID, then enable the bot

Once configured, only you can control the bot in that channel - it ignores commands from anyone else.

**Note:** the bot token/channel/user ID - like the passcode and rotating PIN - are stored machine-wide (shared across every Windows account on the PC, unlike time limits, which are per-account) - set it up once and it works no matter which child's account is running the app. Setup gives every account access to the shared settings folder; a copy of the app that wasn't installed with setup silently falls back to per-account storage instead. Since the config is shared, `!status`/`!history` (and their Telegram equivalents) show which Windows account is currently logged in, so you can tell whose stats you're looking at. If more than one of your kids' accounts is signed in at once (e.g. via Fast User Switching), only the one currently in the foreground answers bot commands, so you're never talking to two accounts' bots at the same time.

---

## Requesting More Time From the Lock Screen

If Telegram and/or Discord is enabled, the lock screen shows a **"Request More Time"** button that doesn't need the passcode. Your child can optionally type a short reason (e.g. "saving my game"), then click it to ping you directly - the message includes their Windows account, remaining time, and the reason if given. Reply `!extend 30` (Discord) or `/extend 30` (Telegram) to grant time, or `!unlock`/`/unlock` to dismiss the screen without changing the timer if there's still time left - either one closes the lock screen automatically. The button has a 5-minute cooldown to prevent spamming.

If both Telegram and Discord are enabled, the request goes to both, but only one reply is needed - whichever one you respond from first, the other gets a short "already handled" notice so you don't have to reply twice.

**Passcode alerts:** if Telegram and/or Discord is enabled, using the passcode to add time - either on the lock screen or via the tray icon's right-click menu - sends you a notification too. This is separate from the request flow above, so you find out any time the passcode grants time, whether or not anyone asked first.

---

## Rotating Daily PIN (Optional)

If you're worried about the passcode being watched and memorized, enable the rotating daily PIN (Settings, or `!rotatingpin on` / `/rotatingpin on`) - a second code that automatically changes every day and works anywhere the regular passcode does. Your regular passcode always keeps working too, so there's no risk of getting locked out if you don't have your phone handy - the rotating code is purely additive. Retrieve today's code any time with `!getpin` / `/getpin`. Off by default. Like the passcode, this is shared machine-wide - enabling it (or fetching today's code) from one Windows account applies to all of them.

---

## Automatic Restart & Tamper Alerts

If Screen Time Manager is stopped in any way other than the passcode-protected Quit option (e.g. ended via Task Manager), it's automatically relaunched within about a minute, and you'll get a Telegram/Discord alert if either is enabled. Setup turns this on for **every** Windows account on the PC, including accounts signed in at the same time via Fast User Switching. Installing an update doesn't count as tampering: the app restarts afterwards without an alert.

---

## Tips

- **Change the default passcode** - The default `0000` is easy to guess!
- **Set reasonable limits** - Too strict and kids get frustrated; too loose and they won't learn limits
- **Check stats occasionally** - See if pause mode is being used appropriately
- **The timer survives restarts** - Restarting the computer won't reset the timer

---

## Updating

**With Telegram or Discord set up:** when a new version comes out, the bot sends you a message. Reply `/update` (Telegram) or `!update` (Discord) to install it. The app downloads the new setup from this project's [Releases page](https://github.com/dhiorns15/screen-time-manager-for-windows/releases), checks that it's exactly the file published there, and installs it. The timer disappears briefly and comes back on its own a minute or so later, on every signed-in account. Your settings, limits and today's remaining time are kept.

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

Here's why it happens: this app does things that look identical to malware when a scanner only looks at *behavior* and not *intent*. It locks the screen, blocks keyboard and mouse input, can shut the computer down, keeps itself running, and accepts remote commands over Telegram and Discord. That's exactly what a parental-control tool needs to do, and also exactly what a scanner's generic "this looks like a trojan" heuristic is tuned to catch. The setup and the app are also unsigned (a code-signing certificate costs money), which removes the one signal that would tell the scanner who built them; SmartScreen also warns about any new download that few people have run yet.

It contains no malware. Nothing is hidden, and it only talks to the internet for the Telegram/Discord bots *you* configure with *your own* tokens, and to this project's GitHub page to check for new versions.

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
