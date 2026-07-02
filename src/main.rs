mod diff;
mod otlp;
mod prices;
mod protocol;
mod proxy;
mod record;
mod store;
mod tui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::store::Store;

#[derive(Parser)]
#[command(
    name = "llmscope",
    version,
    about = "Wireshark for LLM traffic — a zero-config local proxy with a top-style TUI"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct ProxyArgs {
    /// Port for the local proxy (top attaches to the same port)
    #[arg(long, default_value_t = 4040)]
    port: u16,
    /// Where Anthropic traffic is forwarded
    #[arg(long, default_value = "https://api.anthropic.com")]
    anthropic_upstream: String,
    /// Where OpenAI-protocol traffic is forwarded (point this at Ollama,
    /// vLLM or llama.cpp to watch local models, e.g. http://127.0.0.1:11434)
    #[arg(long, default_value = "https://api.openai.com")]
    openai_upstream: String,
    /// SQLite capture file (default: per-user data dir)
    #[arg(long)]
    db: Option<PathBuf>,
    /// Also export each request as an OTLP/HTTP JSON span, e.g.
    /// http://127.0.0.1:4318 (an OpenTelemetry collector)
    #[arg(long)]
    otlp_endpoint: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command with its LLM traffic routed through the proxy
    Run {
        #[command(flatten)]
        proxy: ProxyArgs,
        /// The command to run, e.g. `llmscope run -- claude`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Start the proxy on its own (export the base URLs yourself)
    Serve {
        #[command(flatten)]
        proxy: ProxyArgs,
    },
    /// Live top-style view of a running proxy
    Top {
        /// Port of the proxy to attach to
        #[arg(long, default_value_t = 4040)]
        port: u16,
    },
    /// Re-send a captured request through the running proxy. The replay is
    /// captured like any other request. Credentials come from the
    /// environment (ANTHROPIC_API_KEY / OPENAI_API_KEY) — captures never
    /// store them.
    Replay {
        /// Request id, as shown in `top` and the web UI
        id: i64,
        /// Port of the running proxy
        #[arg(long, default_value_t = 4040)]
        port: u16,
    },
    /// Render a demo TUI frame to HTML (for screenshots)
    #[command(hide = true)]
    DebugRender {
        #[arg(long, default_value_t = 140)]
        width: u16,
        #[arg(long, default_value_t = 38)]
        height: u16,
        /// Which screen to render: dashboard | diff
        #[arg(long, default_value = "dashboard")]
        view: String,
        #[arg(long)]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Pricing overrides live next to the default capture db; both the proxy
    // (costing) and top (cache economics) need them.
    record::load_user_prices(&store::default_db_path().with_file_name("prices.toml"));
    match Cli::parse().cmd {
        Cmd::Run { proxy, command } => {
            let port = proxy.port;
            start_proxy(proxy).await?;
            eprintln!(
                "llmscope: proxy on http://127.0.0.1:{port} — run `llmscope top` in another \
                 terminal, or open http://127.0.0.1:{port}/_llmscope/ui"
            );
            let code = run_child(port, &command).await?;
            std::process::exit(code);
        }
        Cmd::Serve { proxy } => {
            let port = proxy.port;
            start_proxy(proxy).await?;
            eprintln!("llmscope: proxy on http://127.0.0.1:{port}\n");
            eprintln!("  PowerShell:");
            eprintln!("    $env:ANTHROPIC_BASE_URL = \"http://127.0.0.1:{port}/anthropic\"");
            eprintln!("    $env:OPENAI_BASE_URL = \"http://127.0.0.1:{port}/openai/v1\"");
            eprintln!("  bash/zsh:");
            eprintln!("    export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}/anthropic");
            eprintln!("    export OPENAI_BASE_URL=http://127.0.0.1:{port}/openai/v1");
            eprintln!("\nrun `llmscope top` in another terminal, or open");
            eprintln!("http://127.0.0.1:{port}/_llmscope/ui — Ctrl+C to stop.");
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Cmd::Top { port } => tui::run(port).await,
        Cmd::Replay { id, port } => replay(port, id).await,
        Cmd::DebugRender {
            width,
            height,
            view,
            out,
        } => {
            std::fs::write(&out, tui::render_demo_html(width, height, &view)?)?;
            eprintln!("wrote {}", out.display());
            Ok(())
        }
    }
}

async fn start_proxy(args: ProxyArgs) -> Result<()> {
    let db_path = args.db.unwrap_or_else(store::default_db_path);
    let store = Arc::new(Store::open(&db_path).context("opening capture db")?);
    let state = Arc::new(proxy::AppState::new(
        args.anthropic_upstream,
        args.openai_upstream,
        args.otlp_endpoint,
        store,
    ));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port))
        .await
        .with_context(|| {
            format!(
                "port {} is busy — another llmscope running? try --port",
                args.port
            )
        })?;
    let app = proxy::router(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("llmscope: proxy stopped: {e}");
        }
    });
    Ok(())
}

async fn replay(port: u16, id: i64) -> Result<()> {
    use std::io::Write;

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let resp = client
        .get(format!("{base}/_llmscope/diff/{id}"))
        .send()
        .await
        .with_context(|| format!("no proxy on :{port} — start `llmscope serve` first"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("no captured request with id {id}");
    }
    let payload: proxy::DiffPayload = resp.error_for_status()?.json().await?;

    let url = format!("{base}/{}{}", payload.curr.provider, payload.curr.path);
    let mut req = client.post(&url).header("content-type", "application/json");
    req = if payload.curr.provider == "anthropic" {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .context("set ANTHROPIC_API_KEY to replay an Anthropic request")?;
        req.header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
    } else {
        let key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
            .context("set OPENAI_API_KEY (or OPENROUTER_API_KEY) to replay this request")?;
        req.header("authorization", format!("Bearer {key}"))
    };

    eprintln!(
        "llmscope: replaying #{id} · {} · {}{}",
        payload.curr.model, payload.curr.provider, payload.curr.path
    );
    let mut resp = req.body(payload.curr_body).send().await?;
    let status = resp.status();
    eprintln!("llmscope: upstream says {status}");
    let mut out = std::io::stdout().lock();
    while let Some(chunk) = resp.chunk().await? {
        out.write_all(&chunk)?;
    }
    out.flush()?;
    if !status.is_success() {
        bail!("upstream returned {status}");
    }
    Ok(())
}

async fn run_child(port: u16, command: &[String]) -> Result<i32> {
    let base = format!("http://127.0.0.1:{port}");
    let envs = [
        ("ANTHROPIC_BASE_URL", format!("{base}/anthropic")),
        ("OPENAI_BASE_URL", format!("{base}/openai/v1")),
        // Older SDKs and some frameworks still read this spelling.
        ("OPENAI_API_BASE", format!("{base}/openai/v1")),
    ];

    let spawn = |program: &str, args: &[String]| {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        for (k, v) in &envs {
            cmd.env(k, v);
        }
        cmd.status()
    };

    let status = match spawn(&command[0], &command[1..]).await {
        Ok(s) => s,
        // Windows: npm-installed CLIs are .cmd shims that CreateProcess can't
        // launch directly — retry through the shell.
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::NotFound => {
            let mut with_shell = vec!["/C".to_string()];
            with_shell.extend_from_slice(command);
            spawn("cmd", &with_shell)
                .await
                .with_context(|| format!("could not launch `{}`", command[0]))?
        }
        Err(e) => bail!("could not launch `{}`: {e}", command[0]),
    };
    Ok(status.code().unwrap_or(1))
}
