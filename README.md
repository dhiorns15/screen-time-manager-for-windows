# Screen Time Manager

A simple Windows app to manage your child's computer time. Set daily limits, and when time's up, the screen is blocked until you enter the passcode.

No complicated setup. No special accounts needed. Just run it and configure your limits.

Confirmed to be effectively blocking Fortnite, Roblox and Minecraft.

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

## Getting Started

1. **Run the app** - Double-click to start. A small timer appears in the top-right corner.

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

**Setup (one-time):**

1. Open Telegram and search for **@BotFather**
2. Send `/newbot` and follow the steps to create your bot
3. Copy the **bot token** you receive
4. Start a chat with your new bot and send `/start`
5. Note your **chat ID** shown in the reply
6. In Screen Time Manager settings, scroll to "Telegram Bot" section
7. Paste your bot token and chat ID, then enable the bot

Once configured, only you can control the bot - it ignores messages from anyone else.

**Note:** the bot token/chat ID - like the passcode and rotating PIN - are stored machine-wide (shared across every Windows account on the PC, unlike time limits, which are per-account) - set it up once and it works no matter which child's account is running the app. If you have multiple children on separate standard (non-admin) Windows accounts, run `install.ps1` as Administrator at least once so those accounts get permission to read/write the shared config; otherwise it silently falls back to per-account storage.

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
- `!help` - See all commands

**Setup (one-time):**

1. Go to [discord.com/developers/applications](https://discord.com/developers/applications) and click **New Application**
2. Open the **Bot** tab, enable **Message Content Intent**, click **Reset Token**, and copy it
3. Under **OAuth2 > URL Generator**, check the **bot** scope plus the **Send Messages**, **View Channels**, and **Read Message History** permissions, then open the generated URL to invite the bot to your server
4. In Discord, enable **Developer Mode** (User Settings > Advanced), then right-click the channel you want to use and choose **Copy Channel ID**, and right-click your own username and choose **Copy User ID**
5. In Screen Time Manager settings, scroll to the "Discord Bot" section (or use the **Setup Wizard**) and paste your bot token, channel ID, and user ID, then enable the bot

Once configured, only you can control the bot in that channel - it ignores commands from anyone else.

**Note:** the bot token/channel/user ID - like the passcode and rotating PIN - are stored machine-wide (shared across every Windows account on the PC, unlike time limits, which are per-account) - set it up once and it works no matter which child's account is running the app. If you have multiple children on separate standard (non-admin) Windows accounts, run `install.ps1` as Administrator at least once so those accounts get permission to read/write the shared config; otherwise it silently falls back to per-account storage. Since the config is shared, `!status`/`!history` (and their Telegram equivalents) show which Windows account is currently logged in, so you can tell whose stats you're looking at. If more than one of your kids' accounts is signed in at once (e.g. via Fast User Switching), only the one currently in the foreground answers bot commands, so you're never talking to two accounts' bots at the same time.

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

If Screen Time Manager is stopped in any way other than the passcode-protected Quit option (e.g. ended via Task Manager), it's automatically relaunched within about a minute, and you'll get a Telegram/Discord alert if either is enabled. This requires running `install.ps1` as Administrator at least once (it registers a Scheduled Task for this) - see "Making It Start Automatically" below.

This also covers auto-starting at logon and the tamper-alert relaunch for **every** Windows account on the PC, not just whichever account you ran `install.ps1` from - so with multiple kids on separate accounts, one Administrator run of `install.ps1` sets all of them up, including if they're signed in at the same time via Fast User Switching.

---

## Tips

- **Change the default passcode** - The default `0000` is easy to guess!
- **Set reasonable limits** - Too strict and kids get frustrated; too loose and they won't learn limits
- **Check stats occasionally** - See if pause mode is being used appropriately
- **The timer survives restarts** - Restarting the computer won't reset the timer

---

## Making It Start Automatically

**Recommended:** right-click `install.ps1` (next to `screen-time-manager.exe`) and choose **Run with PowerShell**, approving the Administrator prompt. Besides starting the app at logon, this also sets up the automatic relaunch/tamper-alert protection described above - and if you have multiple kids on separate Windows accounts, one run sets all of them up, not just the account you ran it from.

**Lighter alternative**, if you'd rather not run a script:

1. Press `Win + R` on your keyboard
2. Type `shell:startup` and press Enter
3. Copy the Screen Time Manager app into this folder (or create a shortcut to it)

This starts the app at logon for the current Windows account only, and skips the watchdog protection - nothing relaunches it if the process is ended (e.g. via Task Manager), and it won't auto-start for any other Windows account on the PC.

---

## Antivirus & Windows SmartScreen Warnings

When you download or first run the app, Windows SmartScreen or your antivirus may warn that it's from an "unknown publisher" or flag it as suspicious. **This is a false positive.**

Here's why it happens: this app does things that look identical to malware when a scanner only looks at *behavior* and not *intent*. It locks the screen, blocks keyboard and mouse input, can shut the computer down, and accepts remote commands over Telegram. That's exactly what a parental-control tool needs to do, and also exactly what a scanner's generic "this looks like a trojan" heuristic is tuned to catch. The app is also unsigned (a code-signing certificate costs money), which removes the one signal that would tell the scanner who built it.

It contains no malware. Nothing is hidden, nothing phones home except the Telegram bot *you* configure with *your own* token.

**If you don't want to take that on trust, don't. Build it yourself from source — then the exe you run is the code you can read.**

### Build It Yourself

1. Install [Rust](https://rustup.rs/) (the installer adds `cargo` to your PATH).
2. Clone or download this repository.
3. From the project folder, run:

   ```
   cargo build --release
   ```

4. The compiled app is at `target\release\screen-time-manager.exe`. Run it directly, or copy it next to the install scripts.

The release binary on GitHub is built this exact way, from this exact source, on a clean GitHub Actions runner. Building locally just lets you verify that for yourself.

---

## Requirements

- Windows 10 or Windows 11
- That's it!
