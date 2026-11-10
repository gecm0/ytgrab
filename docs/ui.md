# UI decision: "Comfortable" layout

Decided 2026-11-08 from a throwaway HTML prototype (two rounds, 7 directions).

## Chosen

- **Header:** `Saving to ~/Downloads` (home shown as `~`), ghost `Change…`, raised `Open folder`. 12px × 16px padding.
- **Rows:** 112×63 thumbnail (radius 6), title 14px/20 medium on one line (queued rows show the URL in muted, normal weight), one 12px status line under it: `Queued` / `Starting…` / `Downloading video 42.1%` / `Downloading audio 42.1%` / `Merging video and audio…` (blue, with a 4px bar under it whose slot is always reserved so text never moves) / `Done` (green) / `Failed: …` (red, up to 2 lines; the full error goes to the status bar's Details). 12px × 16px padding, 14px gap, 1px divider.
- **Actions:** `Play` raised + `Show in folder` ghost + `Remove` ghost on finished rows; `Retry` raised + `Remove` ghost on failed rows. 28px tall. Every button is a Tab stop (Enter/Space click it); the blue focus border shows only after Tab, not after a mouse click.
- **Bottom status bar (32px):** colored dot (blue busy, green ok, amber warning), short message, `Details` toggle that shows the full yt-dlp/OS error above the bar, and a `1 downloading · 2 queued` summary on the right, followed by a `Clear completed` ghost when any row is done.
- **Empty state:** "No downloads yet" + how to start (the extension's Download button).
- **Palette:** bg `#1c1c1e`, raised `#2a2a2c`, raised hover `#353538`, line `#2c2c2e`, text `#ececee`, muted `#9a9aa0`, blue `#6cb6ff`, green `#57d37a`, red `#ff6b6b`, amber `#e8b45a`, warning text `#f0cf96`. Base text 13px.

## Rejected

- **Compact** (64×36 thumbs, one-line rows): liked the bottom status bar, but too dense.
- **Gallery** (3-column cards, progress on the thumbnail): liked the folder header, but fits ~6 items and queued URLs get cut short.
- **Cards** (each row a raised card): more visual noise for the same information.
- **Media** (thumbnail is the Play button): Play is less obvious than a labeled button.
- **Spotlight** (big hero for the active download): wastes ~150px when idle.
- **Status at the top as a full sentence** (the old UI): a long yt-dlp error dominated the window. Now it is one short line with the details one click away.

Not done: tabular numbers for the percentage (gpui 0.2.2 only sets OpenType features through a full `Font`, not worth it for one label).
