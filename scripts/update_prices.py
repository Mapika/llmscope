"""Regenerate src/prices.rs from LiteLLM's community pricing table.

LiteLLM maintains the de-facto community price list for LLM APIs
(model_prices_and_context_window.json). This script extracts a curated set
of model families from it and emits a prefix-matched Rust table with
per-model cache read/write prices.

Usage: python scripts/update_prices.py [path-to-local-json]
Without an argument the table is fetched from GitHub. The script fails
loudly if a wanted key disappears upstream, so a rename can't silently
drop a model to $0.
"""

import datetime
import json
import pathlib
import sys
import urllib.request

SOURCE_URL = (
    "https://raw.githubusercontent.com/BerriAI/litellm/main/"
    "model_prices_and_context_window.json"
)
OUT_PATH = pathlib.Path(__file__).resolve().parent.parent / "src" / "prices.rs"

# (rust table prefix, litellm key). Prefixes are matched longest-first at
# runtime, so families and their variants can coexist ("gpt-5.2-pro" wins
# over "gpt-5.2" wins over "gpt-5"). Aliases map gateway spellings
# ("claude-4.5-sonnet") onto the canonical entry's prices.
WANTED = [
    # Anthropic
    ("claude-fable-5", "claude-fable-5"),
    ("claude-sonnet-5", "claude-sonnet-5"),
    ("claude-sonnet-4-6", "claude-sonnet-4-6"),
    ("claude-sonnet-4-5", "claude-sonnet-4-5"),
    ("claude-sonnet-4", "claude-sonnet-4-20250514"),
    ("claude-opus-4-8", "claude-opus-4-8"),
    ("claude-opus-4-7", "claude-opus-4-7"),
    ("claude-opus-4-6", "claude-opus-4-6"),
    ("claude-opus-4-5", "claude-opus-4-5"),
    ("claude-opus-4-1", "claude-opus-4-1"),
    ("claude-opus-4", "claude-opus-4-20250514"),
    ("claude-haiku-4-5", "claude-haiku-4-5"),
    ("claude-4-sonnet", "claude-4-sonnet-20250514"),
    ("claude-4-opus", "claude-4-opus-20250514"),
    ("claude-3-7-sonnet", "claude-3-7-sonnet-20250219"),
    ("claude-3-haiku", "claude-3-haiku-20240307"),
    # Gateway spellings seen in the wild (version-first slugs).
    ("claude-5-fable", "claude-fable-5"),
    ("claude-5-sonnet", "claude-sonnet-5"),
    ("claude-4.6-sonnet", "claude-sonnet-4-6"),
    ("claude-4.5-sonnet", "claude-sonnet-4-5"),
    ("claude-4.5-haiku", "claude-haiku-4-5"),
    # OpenAI
    ("gpt-5.5-pro", "gpt-5.5-pro"),
    ("gpt-5.5", "gpt-5.5"),
    ("gpt-5.4-pro", "gpt-5.4-pro"),
    ("gpt-5.4-nano", "gpt-5.4-nano"),
    ("gpt-5.4-mini", "gpt-5.4-mini"),
    ("gpt-5.4", "gpt-5.4"),
    ("gpt-5.3", "gpt-5.3-codex"),
    ("gpt-5.2-pro", "gpt-5.2-pro"),
    ("gpt-5.2", "gpt-5.2"),
    ("gpt-5.1-codex-mini", "gpt-5.1-codex-mini"),
    ("gpt-5.1", "gpt-5.1"),
    ("gpt-5-pro", "gpt-5-pro"),
    ("gpt-5-nano", "gpt-5-nano"),
    ("gpt-5-mini", "gpt-5-mini"),
    ("gpt-5", "gpt-5"),
    ("gpt-4.1-nano", "gpt-4.1-nano"),
    ("gpt-4.1-mini", "gpt-4.1-mini"),
    ("gpt-4.1", "gpt-4.1"),
    ("gpt-4o-mini", "gpt-4o-mini"),
    ("gpt-4o", "gpt-4o"),
    ("o4-mini", "o4-mini"),
    ("o3-deep-research", "o3-deep-research"),
    ("o3-pro", "o3-pro"),
    ("o3-mini", "o3-mini"),
    ("o3", "o3"),
    # Common gateway-served families (reached via the OpenAI protocol).
    ("gemini-3.5-flash", "gemini-3.5-flash"),
    ("gemini-3.1-pro", "gemini-3.1-pro-preview"),
    ("gemini-3.1-flash-lite", "gemini-3.1-flash-lite"),
    ("gemini-3-pro", "gemini-3-pro-preview"),
    ("gemini-3-flash", "gemini-3-flash-preview"),
    ("gemini-2.5-pro", "gemini-2.5-pro"),
    ("gemini-2.5-flash-lite", "gemini-2.5-flash-lite"),
    ("gemini-2.5-flash", "gemini-2.5-flash"),
    ("deepseek-v4-flash", "deepseek-v4-flash"),
    ("deepseek-v4-pro", "deepseek-v4-pro"),
    ("deepseek-reasoner", "deepseek-reasoner"),
    ("deepseek-chat", "deepseek-chat"),
    ("grok-4.3", "xai/grok-4.3"),
    ("grok-4-1-fast", "xai/grok-4-1-fast"),
    ("grok-4-fast", "xai/grok-4-fast-reasoning"),
    ("grok-4", "xai/grok-4"),
    ("kimi-k2.7", "cloudflare/@cf/moonshotai/kimi-k2.7-code"),
    ("kimi-k2.6", "moonshot/kimi-k2.6"),
    ("kimi-k2.5", "moonshot/kimi-k2.5"),
    ("kimi-k2-thinking", "moonshot/kimi-k2-thinking"),
    ("kimi-k2", "moonshot/kimi-k2-0905-preview"),
]

# Models LiteLLM no longer carries: (prefix, input, output, cache_read,
# cache_write) in $/Mtok, from the providers' historical price sheets.
EXTRA = [
    ("claude-3-5-sonnet", 3.0, 15.0, 0.3, 3.75),
    ("claude-3-5-haiku", 0.8, 4.0, 0.08, 1.0),
]


def mtok(per_token):
    return round(per_token * 1_000_000, 6)


def main() -> None:
    if len(sys.argv) > 1:
        data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
    else:
        with urllib.request.urlopen(SOURCE_URL) as resp:
            data = json.load(resp)

    rows = []
    for prefix, key in WANTED:
        m = data.get(key)
        if m is None:
            sys.exit(f"error: litellm key {key!r} not found — renamed upstream?")
        inp = mtok(m.get("input_cost_per_token") or 0)
        out = mtok(m.get("output_cost_per_token") or 0)
        if inp <= 0 or out <= 0:
            sys.exit(f"error: litellm key {key!r} has zero pricing")
        # No listed cache price means no discount: bill cached reads and
        # writes at the plain input rate.
        cr = m.get("cache_read_input_token_cost")
        cw = m.get("cache_creation_input_token_cost")
        rows.append(
            (prefix, inp, out, mtok(cr) if cr else inp, mtok(cw) if cw else inp)
        )
    for prefix, inp, out, cr, cw in EXTRA:
        rows.append((prefix, inp, out, cr, cw))

    # Longest prefix first — the runtime lookup takes the first match, and
    # equal-length prefixes can never both match one model name.
    rows.sort(key=lambda r: (-len(r[0]), r[0]))

    today = datetime.date.today().isoformat()
    lines = [
        "//! Generated by scripts/update_prices.py — do not edit by hand.",
        f"//! Source: LiteLLM's model_prices_and_context_window.json ({today}).",
        "",
        "use crate::record::ModelPrice;",
        "",
        "/// $/Mtok, matched by model-name prefix, longest prefix first (the",
        "/// lookup takes the first match). Unknown models cost $0.",
        "pub const PRICES: &[ModelPrice] = &[",
    ]
    for prefix, inp, out, cr, cw in rows:
        lines.append(
            f'    ModelPrice {{ prefix: "{prefix}", input: {inp!r}, '
            f"output: {out!r}, cache_read: {cr!r}, cache_write: {cw!r} }},"
        )
    lines += ["];", ""]

    OUT_PATH.write_text("\n".join(lines), encoding="utf-8", newline="\n")
    print(f"wrote {OUT_PATH} ({len(rows)} entries)")


if __name__ == "__main__":
    main()
