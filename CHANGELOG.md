# Changelog

## Unreleased

**New**

- JTokens alerts: a new Alerts tab. Set limits for API cost in an hour, tokens in an hour, and API cost in 24 hours. When usage crosses a limit JTokens shows a Windows notification, flashes the taskbar button, and logs it under Recent alerts. A fired alert stays quiet for a while and re-arms when usage falls back under half the limit. Limits live in settings.txt, and a limit of 0 turns that alert off.
- Keep watching in the background rescans the logs every 30 seconds, even while the window is minimized.
- Usage report on the Alerts tab: this hour and the last 24 hours, top days by cost and by tokens, the peak hour and busiest hours and weekday, and the biggest session. Copy report writes it as text. It uses all kept logs for the selected tool.
- `jtokens --toast-test` shows a test notification, so alerts can be checked without waiting for real usage.
- Qwen Code sessions are read from `~/.qwen/projects`, next to the other tools.
- Devin sessions are read from `%APPDATA%\Devin\cli\sessions.db`, next to the other tools.
- API prices for DeepSeek V4.1 Flash and V4 Pro (peak and off-peak), Kimi K3 and K3.1 Pro, Grok 4.7, 4.6, 4.5, 4.3 and Build 0.1, GLM-5.x and 4.x, and MiniMax M3 and M3.1 Flash. Prices come from each provider's own pricing page.

**Fixed**

- Models with a provider prefix (for example `opencode/glm-5.3-flash`) get their price.
- Price table names and model ids wrap, so the Fast mode column stays in view at 1200px.

## v1.2.0

**New**

- JTokens reads OpenCode, Pi and Droid logs too, next to Claude Code and Codex. The buttons under the header pick All or one tool. A model used in two tools shows the tool name on its label.
- Full screen for any card. The Sessions table shows Tool, Project and Last active when there is room.
- Drag across the daily chart to pick a range of days.
- Model trend shows each model over time. Running total shows the total building up day by day. The hour chart has a Week view.
- Project map shows projects as blocks sized by their share of the total.
- Cache saved shows what your prompt cache saved at API prices.
- API prices: the "In your logs" column marks the models you used.
- From the command line, `jtokens --export file.html` writes the dashboard to a file.

**Improved**

- Arrow keys move through the source and range buttons. Home and End jump to the first and last. Arrow keys also move through the Sessions table and the hour grid.
**Fixed**

- Session names with escaped accented letters show the letters, not gaps.
- Prices match model names whatever their letter case.

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
