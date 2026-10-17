# Filter lists

Bare's ad and tracker blocker loads these lists from this folder. Refresh them with `scripts/update-blocklist.ps1`.

| File | Contents | Source | License |
|---|---|---|---|
| `easylist.txt` | Ads | [EasyList](https://easylist.to) | GPLv3 or CC BY-SA 3.0 (dual) |
| `easyprivacy.txt` | Trackers | [EasyPrivacy](https://easylist.to) | GPLv3 or CC BY-SA 3.0 (dual) |
| `hagezi-pro.txt` | Ads, trackers, device and app telemetry | [HaGeZi Multi PRO](https://github.com/hagezi/dns-blocklists) | GPLv3 |
| `bare-extra.txt` | Analytics and device telemetry the big lists miss | Bare (hand-curated) | MIT |
| `fanboy-social.txt` | Optional: social media buttons, embeds, trackers | [Fanboy's Social Blocking List](https://easylist.to) | GPLv3 or CC BY-SA 3.0 |
| `bare-social.txt` | Optional: social networks' scripts and widgets on other sites | Bare (hand-curated) | MIT |
| `bare-cookies.txt` | Cookie-consent managers (Cookiebot, OneTrust, Osano, TrustArc, Didomi, Sourcepoint, Usercentrics) | Bare (hand-curated) | MIT |
| `easylist-cookie.txt` | Optional: cookie-consent banners | [EasyList Cookie List](https://easylist.to) | CC BY 3.0 |
| `blocklist.txt` | Ad, tracking and malware domains | [StevenBlack unified hosts](https://github.com/StevenBlack/hosts) | MIT |

The lists are separate data files distributed alongside Bare and keep their own licenses; they are not part of Bare's MIT-licensed source code.
