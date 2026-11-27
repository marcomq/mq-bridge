// sqlx keeps its drivers process-wide, so this needs a test binary of its own.
#![cfg(feature = "sqlite")]

use mq_bridge::endpoints::sqlx::SqlxPublisher;
use mq_bridge::models::SqlxConfig;

#[tokio::test]
async fn endpoint_works_with_drivers_the_host_installed() {
    sqlx::any::install_drivers(&[sqlx::sqlite::any::DRIVER]).unwrap();
    mq_bridge::endpoints::sqlx::use_host_sqlx_drivers();

    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("host.db").display());
    let mut config = SqlxConfig::new(url, "events");
    config.auto_create_table = true;

    SqlxPublisher::new(&config).await.unwrap();
}
