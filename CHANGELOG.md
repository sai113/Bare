# What's new in Bare

Each section becomes the notes of that version's GitHub release (`cargo xtask release` copies it into `dist/release-notes.md`). Write for people using Bare: what they'll notice, not how it was built.

## 0.7.0

- **🔊 on tabs that play sound.** Click it to mute the tab (it turns into 🔇); click again to unmute. The tab stays muted until you unmute it.
- **Windows Hello before showing a password.** On `bare://passwords`, **Show** now asks for your face, fingerprint or PIN first, like Chrome. After that, passwords can be shown for a minute without asking again.
- **Import passwords** from Chrome, Edge, Brave, Firefox, Bitwarden and other password managers: export them as a CSV file there, then use **Import passwords (CSV)…** on `bare://passwords`. Logins you've already saved are kept. Bare then offers to delete the CSV file, which holds your passwords unencrypted.
- **More blocking:**
  - YouTube's ad tracking and telemetry (`s.youtube.com` and others). Videos keep playing, but YouTube no longer records your watch history.
  - Instagram's tracking when other sites load it (instagram.com itself keeps working).
  - Branch link tracking, VK's retargeting pixel, and two malware sites.
- **Hide cookie banners** (Settings → Ad and tracker blocking) now also blocks the consent services behind the banners: Cookiebot, TrustArc, Didomi, Sourcepoint and Usercentrics.

## 0.6.0

- **Chromium engine updates.** Settings → About now shows the Chromium version Bare runs on. When a newer one has been approved, **Update engine** installs it without waiting for a new version of Bare, so security fixes reach you sooner.
- **Fixed:** Settings → About → **Make default** opened File Explorer instead of Windows' Default apps settings.

## 0.5.0

- **New tabs open your search engine's home page** (Google, DuckDuckGo, …) with the cursor in the address bar. Settings → Search can make them blank instead.
- **Reader view.** On articles, press **📖** in the address bar (or F9) for clean, readable text without ads or clutter, in light or dark to match Windows.
- **Password vault.** Bare offers to save passwords when you sign in, and **🔑** in the address bar fills them in. They're encrypted for your Windows account. Manage them at `bare://passwords`.
- **History** (Ctrl+H) with suggestions as you type in the address bar.
- **Bookmarks bar** (Ctrl+D to bookmark, Ctrl+Shift+B to show or hide), with import from Chrome, Edge and Brave.
- **Lots of tabs** now fit: they shrink, and the rest are listed under **▾**. Hover a tab to see how much memory it uses.
- **Zoom indicator** (shows when a page isn't at 100%; click to reset) and **picture-in-picture** for videos (Alt+P).

## 0.3.1

- **Installer** (`BareSetup.exe`): installs Bare for your account, adds Start menu shortcuts (including a private window), and registers Bare as a browser so you can make it your default.

## 0.3.0

- First public release: Chromium with built-in ad and tracker blocking, HTTPS-only browsing, a downloads page, settings, permission prompts, find in page, secure DNS and proxy settings, and signed automatic updates.
