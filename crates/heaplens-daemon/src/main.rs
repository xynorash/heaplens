pub mod config;
pub mod graph;
pub mod ingest;
pub mod msg;
pub mod resolver;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Ok(())
}
