pub mod analysis;
mod server;
pub mod workspace;

pub async fn run() {
    server::run().await;
}
