# llmscope

**Wireshark for LLM traffic.** A zero-config local proxy that shows you what your agents *actually* send — every request, token, cache hit and dollar — live, in a `top`-style TUI.

No SDK. No account. No config. One binary.

```
llmscope run -- claude        # terminal 1: your agent, unchanged
llmscope top                  # terminal 2: watch everything it does
```

```
┌ llmscope ── ● proxy :4040 ──────────────────────────────────────────────────┐
│ req 47 │ in 1.24M │ cached 71% │ out 38.4k │ spend $4.83                    │
└──────────────────────────────────────────────────────────────────────────────┘
┌ tokens/s  peak 214 ───────────────────┐┌ time-to-first-token  avg 480ms ────┐
│      ▄▆█▇▅▂    ▁▃▅▇█▆▄▂▁      ▂▄▆█▆▃  ││ ▂▁▂▃▂▁█▂▁▂▂▃▂▁▂▇▂▁                 │
└───────────────────────────────────────┘└────────────────────────────────────┘
┌ requests ────────────────────────────────────────────────────────────────────┐
│ TIME      MODEL              IN      CACHE   OUT     TTFT    TOTAL    COST   │
│ 14:02:11  claude-sonnet-4-5  82.1k   94%     1.2k    412ms   8.2s     $0.11  │
│ 14:01:58  claude-haiku-4-5   3.4k    0%      210     190ms   1.1s     $0.004 │
└──────────────────────────────────────────────────────────────────────────────┘
  q quit
```

## How it works

`llmscope run -- <cmd>` starts a local proxy and launches your command with
`ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` pointed at it. Claude Code, Codex,
Gemini CLI and every major SDK respect those variables, so there is nothing
to instrument. Responses stream through untouched — llmscope tees the bytes,
parses the SSE stream on the side, and records:

- **tokens** — input / output / cache reads / cache writes, per request
- **cost** — priced per model, including cache read/write multipliers
- **latency** — time-to-first-token vs. total generation time
- **full bodies** — every request and response, in a local SQLite file

`llmscope top` attaches to the proxy from another terminal and renders the
live view.

## Local models too

Point the OpenAI-protocol upstream at anything that speaks it:

```
llmscope run --openai-upstream http://127.0.0.1:11434 -- python my_agent.py   # Ollama
llmscope run --openai-upstream http://127.0.0.1:8000  -- python my_agent.py   # vLLM
```

## Privacy

Everything stays on your machine. Captures go to a local SQLite file
(`llmscope run --db <path>` to relocate). Authorization headers and API keys
are never stored — only request/response bodies and timing metadata.

## Status

Early. Core proxy + TUI work. Planned next:

- [ ] **turn diff** — see exactly what your agent re-sends every turn
- [ ] cache-miss cost analysis ("this session wasted $X on cache misses")
- [ ] request detail / body viewer in the TUI
- [ ] web UI for deep inspection
- [ ] pricing table via config file (built-ins cover common models)

## Build

```
cargo build --release
```
