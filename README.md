# Spanish localization preview — PR #1372

Captured on Linux x86_64 from the pinned Nix debug build, in a private Xvfb
session with `MAPLE_ENABLE_LOCALIZATION=1`. The build contains the Spanish
catalog and locale registration changes on top of `1319d580`.

- `english.png`: General settings with the saved English preference.
- `spanish.png`: General settings after selecting Español with the keyboard
  and restarting the desktop. The label, description and saved preference
  are visibly Spanish; unconverted settings remain English.
- `spanish-language-menu.png`: Spanish picker, including Sistema (English).
  The system locale is English; language names are displayed in their own language.

This is offline UI evidence using a copy of a disposable Local profile.
No backend services, new sign-in, inference requests or production services
were used. A cold host handshake timed out initially; reopening the desktop
without stopping that host succeeded. No clean-shutdown claim is made.
These images do not exercise the live updater dialog or macOS/Windows.
