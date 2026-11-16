# Security

Please report security problems privately, not in a public issue: use
[Report a vulnerability](https://github.com/dhiorns15/screen-time-manager-for-windows/security/advisories/new)
on the repository's Security tab. Say what you found, how to make it happen, and which version of
the app and of Windows you're on. You'll hear back within a week, and we'll agree on when it's made
public once it's fixed.

Worth reporting, for example:

- A standard (child's) account getting around the limits or the lock screen, stopping the app or
  its watchdog without the parent being told, or changing its settings or its stored time without
  the passcode.
- A standard account gaining administrator or SYSTEM rights through setup, the updater, the
  scheduled tasks, or the app's folders.
- The updater installing something that isn't a Screen Time Manager release from this repository.
- The passcode or the Telegram or Discord bot tokens ending up in logs or messages, or being sent
  to a server they don't belong to.
- Someone who isn't a configured parent controlling the app through the bots.

Fixes go into the latest release, which the app offers as an update.
