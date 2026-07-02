"""Measure llmscope's proxy overhead against a local mock upstream.

Runs a trivial OpenAI-shaped upstream in-process, launches `llmscope serve`
pointed at it, then times identical requests sent directly vs through the
proxy over a reused connection. Only the added hop differs.

Usage: python scripts/bench_overhead.py  (needs `llmscope` on PATH)
"""

import http.client
import json
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MOCK_PORT = 4959
PROXY_PORT = 4949
WARMUP = 30
SAMPLES = 300

RESPONSE = json.dumps(
    {
        "id": "chatcmpl-bench",
        "object": "chat.completion",
        "model": "gpt-4o-mini",
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": "Hello from the mock."},
                "finish_reason": "stop",
            }
        ],
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": 6,
            "prompt_tokens_details": {"cached_tokens": 0},
        },
    }
).encode()


class MockUpstream(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(RESPONSE)))
        self.end_headers()
        self.wfile.write(RESPONSE)

    def log_message(self, *_args):
        pass


def bench(port: int, path: str) -> list[float]:
    conn = http.client.HTTPConnection("127.0.0.1", port)
    body = json.dumps(
        {
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": "benchmark ping"}],
        }
    )
    headers = {"Content-Type": "application/json"}
    times: list[float] = []
    for i in range(WARMUP + SAMPLES):
        t0 = time.perf_counter()
        conn.request("POST", path, body, headers)
        resp = conn.getresponse()
        resp.read()
        elapsed_ms = (time.perf_counter() - t0) * 1000.0
        assert resp.status == 200, resp.status
        if i >= WARMUP:
            times.append(elapsed_ms)
    conn.close()
    return times


def pct(values: list[float], p: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * p))]


class QuietServer(ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        pass  # pooled connections dropping at teardown are expected


def main() -> None:
    server = QuietServer(("127.0.0.1", MOCK_PORT), MockUpstream)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as tmp:
        proxy = subprocess.Popen(
            [
                "llmscope",
                "serve",
                "--port",
                str(PROXY_PORT),
                "--openai-upstream",
                f"http://127.0.0.1:{MOCK_PORT}",
                "--db",
                f"{tmp}/bench.db",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            time.sleep(1.5)
            direct = bench(MOCK_PORT, "/v1/chat/completions")
            proxied = bench(PROXY_PORT, "/openai/v1/chat/completions")
        finally:
            proxy.terminate()
            try:
                proxy.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proxy.kill()
            time.sleep(0.3)  # let Windows release the db file handles
    server.shutdown()

    d50, d95 = statistics.median(direct), pct(direct, 0.95)
    p50, p95 = statistics.median(proxied), pct(proxied, 0.95)
    print(f"direct   p50 {d50:.2f} ms   p95 {d95:.2f} ms   (n={len(direct)})")
    print(f"proxied  p50 {p50:.2f} ms   p95 {p95:.2f} ms   (n={len(proxied)})")
    print(f"overhead p50 {p50 - d50:.2f} ms   p95 {p95 - d95:.2f} ms")


if __name__ == "__main__":
    sys.exit(main())
