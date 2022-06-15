use testground::client::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (client, _run_parameters) = Client::new().await?;
    client.wait_network_initialized().await?;

    client.record_success().await?;
    Ok(())
}
