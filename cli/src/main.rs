#[tokio::main]
async fn main() -> anyhow::Result<()> {
    astrcode_cli::app::run_from_env().await
}
