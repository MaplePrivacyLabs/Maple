# Language restart notice — PR #1372

Captured from the rebuilt Linux desktop with MAPLE_ENABLE_LOCALIZATION=1.
The follow-up patch is based on 66df7da8.

- restart-notice-en.png: choose Español while the running UI is English.
- restart-notice-es.png: choose English while the running UI is Spanish.

Pointer/keyboard language selection, Later dismissal, the reminder returning
when Settings reopens, and Restart now relaunching the app into Spanish were
exercised in the native app. The final accessibility tree names both actions
in the current UI language. Restart now was exercised again on the final build.

This used a private copy of a disposable Local profile, with backend services
stopped and no new account login or inference requests. It is UI/lifecycle
evidence, not a backend integration or native macOS/Windows check.
