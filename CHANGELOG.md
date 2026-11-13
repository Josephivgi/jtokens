# Changelog

## v1.1.0

**New**

- JTokens now updates itself. When a new version is out, a bar at the top says so. Press Install and restart and it downloads the update, checks it against the release checksum, and restarts into the new version. What's new shows the release notes first.
- It checks when it opens and every 30 minutes. The bottom of the page shows your version and has a Check for updates button.

## v1.0.0

First public release.

**New**

- See how many tokens Claude Code and Codex used and what that would cost at API list prices. JTokens reads the logs already on your computer and sends nothing anywhere.
- Filter by app (Claude Code or Codex), date range (Today, 7, 30, 90 days, All time), and show cost or tokens.
- Daily chart by model, a By model list, a By hour of day chart, a Token mix bar, and Sessions and Projects tables. Click a day, model or session to filter everything to it.
- Live mode refreshes on its own while you work. Refresh rescans by hand.
- API prices tab lists every price used and links to where it came from. Prices include Claude Fable 5.2 and its fast mode.
- Light and dark theme, remembered between launches.
- Forked Codex sessions and repeated Claude log lines are counted once, so totals are not inflated.

**Notes**

- Needs Windows 10 or 11 (64-bit) with the Microsoft Edge WebView2 Runtime, which Windows 11 already has. If it is missing, JTokens tells you where to get it.
- Cost is what the same tokens would cost on the pay-as-you-go API. If you use a Claude or ChatGPT subscription you are not billed this amount.
