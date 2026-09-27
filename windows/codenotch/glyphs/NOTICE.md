# Provider marks

The SVG files in this directory come from the npm package `@lobehub/icons-static-svg` 1.95.0
(https://github.com/lobehub/lobe-icons, MIT License) and are unmodified:

| File | Original file in the package | Shown in |
|---|---|---|
| codex.svg | icons/openai.svg | Codex cell (the OpenAI mark, matching upstream Codenotch's glyph choice) |
| codex-alt.svg | icons/codex.svg | alternative: Codex's own mark |
| cursor.svg | icons/cursor.svg | Cursor cell |
| gemini.svg | icons/antigravity.svg | Antigravity cell |
| gemini-alt.svg | icons/gemini.svg | alternative: the Gemini spark |
| opencode.svg | icons/opencode.svg | OpenCode cell |

The two marks for the slots this build replaces are not in that package. They were taken from
Token Monitor (https://github.com/Javis603/token-monitor, `assets/icons/`), which redistributes
the same LobeHub set:

| File | Original file in Token Monitor | Shown in |
|---|---|---|
| dsh.svg | assets/icons/dsh.svg | DeepSeek Harness cell |
| workbuddy.svg | assets/icons/workbuddy.svg | WorkBuddy cell |

Both are byte-identical to their source apart from the UTF-8 byte-order mark stripped from
`dsh.svg`.

MIT License — Copyright (c) LobeHub. See that repository's LICENSE.

**Trademarks**: these marks are trademarks of OpenAI, Anysphere (Cursor), Google, Tencent
(WorkBuddy) and DeepSeek respectively, and are used here only to identify the product whose usage
is displayed. Whether they stay in a distributed build is the repository owner's call under each
brand's guidelines; they can be swapped for generated glyphs without touching any code.

**Overrides**: a file of the same name (`.svg` or `.png`) in `%APPDATA%\codenotch\glyphs\` takes
precedence over the built-in mark; it is picked up after "Refresh usage now" in the tray menu.
