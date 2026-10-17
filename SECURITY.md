# Security

## Reporting a vulnerability

**Please report security problems privately, not in public issues.**

Use GitHub's private reporting: go to the [Security tab](https://github.com/sai113/Bare/security) of this repository and click **Report a vulnerability**. Only the maintainer can see the report.

Please include:

- what an attacker can do (for example "a website can read local files", "a page can change Bare's settings");
- how to reproduce it: a page, steps, or a short proof of concept;
- the Bare version (Settings → About) and Windows version.

Bare is maintained by one person, so please allow a few days for a first reply. Fixes for confirmed problems ship as a signed update, which installed copies pick up within a day. With your permission, you'll be credited in the release notes.

## What's in scope

**Bare's own code**, in this repository. For example:

- a website escaping what a normal web page can do through something Bare added, such as built-in pages (`bare://settings`, `bare://passwords`), pop-up handling, reader view, the password vault, or the scripts Bare injects into pages;
- the updater or engine updater installing something that isn't signed with the project's key;
- saved passwords being readable by another Windows account, or filled into the wrong site;
- the ad blocker letting a page affect other sites.

**Not Bare's code** (please report these upstream):

- bugs in Chromium itself: [Chromium's security process](https://www.chromium.org/Home/chromium-security/reporting-security-bugs/);
- bugs in the Chromium Embedded Framework: [CEF's issue tracker](https://github.com/chromiumembedded/cef/issues);
- sites that filter lists fail to block: report them to the list (EasyList, HaGeZi, …) or open a normal issue here.

Bare does ship Chromium, though. If a Chromium fix has been released but Bare still runs the old engine, tell us in a normal issue.

## Supported versions

Only the latest release gets fixes. Bare updates itself, so please check you're on the newest version (Settings → About → Check now) before reporting.

## How Bare is protected

- **Pages run in Chromium's sandbox**, the same as in Chrome.
- **Updates are signed.** Bare only installs an update, or a new Chromium engine, if it's signed with the project's Ed25519 key (the private key never leaves the maintainer's computer) and its SHA-256 matches. See [How updates are signed](README.md#how-updates-are-signed).
- **New Chromium versions are watched daily.** A GitHub Action opens an issue when a newer stable engine is published, so security fixes reach users quickly.
- **Built-in pages can't be driven by websites.** Their actions need a random per-session secret and must come from Bare's own page.
- **Saved passwords** are encrypted with Windows DPAPI, filled only on the exact site they were saved for, and only when you press 🔑. Showing one asks for Windows Hello.

More detail is in the [security model](README.md#security-model) section of the README.
