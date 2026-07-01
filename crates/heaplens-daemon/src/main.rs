use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use heaplens_daemon::config::Config;
use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::ingest;
use heaplens_daemon::msg::GraphMsg;
use heaplens_daemon::resolver::Resolver;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Initialise tracing with EnvFilter; default to "info".
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // 2. Load config (pipe name, tick interval).
    let config = Config::load();

    // 3. Create the mpsc channel that connects ingest + timer → graph loop.
    let (tx, mut rx) = mpsc::unbounded_channel::<GraphMsg>();

    // 4 & 5. Clone tx for the timer *before* moving tx into the ingest task.
    let tick_tx = tx.clone();
    tokio::spawn(ingest::run(config.pipe_name.clone(), tx));

    let tick_ms = config.tick_ms;
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_millis(tick_ms));
        loop {
            interval.tick().await;
            if tick_tx.send(GraphMsg::Tick).is_err() {
                break;
            }
        }
    });

    // 6 & 7. Graph loop — owns graph state and runs on the main task.
    let mut graph = OwnershipGraph::new();
    let resolver = Resolver::new();

    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(GraphMsg::Events(events)) => {
                    for ev in &events {
                        match ev.kind {
                            0 => graph.on_alloc(ev),
                            1 => graph.on_dealloc(ev.ptr),
                            2 => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size),
                            _ => {}
                        }
                    }
                }
                Some(GraphMsg::Tick) => {
                    let diff = graph.drain_diff(&resolver);
                    if let Ok(json) = serde_json::to_string(&diff) {
                        println!("{json}");
                    }
                    if let heaplens_protocol::GraphMessage::Diff {
                        ref add,
                        ref update,
                        ref remove,
                        ..
                    } = diff
                    {
                        tracing::debug!(
                            add = add.len(),
                            update = update.len(),
                            remove = remove.len(),
                            "tick diff"
                        );
                    }
                }
                None => break,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    Ok(())
}
