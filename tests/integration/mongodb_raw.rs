#![allow(unused_imports, dead_code)]

use mongodb::bson::{doc, Document};
use mq_bridge::endpoints::mongodb::MongoDbPublisher;
use mq_bridge::models::{MongoDbConfig, MongoDbFormat};
use mq_bridge::test_utils::{run_test_with_docker, setup_logging, should_run};
use mq_bridge::traits::MessagePublisher;

#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mongodb_publisher_avoids_seq_metadata() {
    if !should_run("mongodb") {
        return;
    }

    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        // No format writes `seq` or a counter document: the collection holds the message alone.
        for format in [MongoDbFormat::Raw, MongoDbFormat::Json] {
            let collection_name = format!("no_seq_{}", fast_uuid_v7::gen_id());
            let config = MongoDbConfig {
                url: "mongodb://localhost:27017".to_string(),
                database: "mq_bridge_test".to_string(),
                collection: Some(collection_name.clone()),
                format: format.clone(),
                ..Default::default()
            };

            let client = mongodb::Client::with_uri_str(&config.url).await.unwrap();
            let collection = client
                .database(&config.database)
                .collection::<Document>(&collection_name);
            collection.drop().await.ok();

            let publisher = MongoDbPublisher::new(&config).await.unwrap();
            publisher
                .send(mq_bridge::CanonicalMessage::new(
                    br#"{"kind":"raw","value":1}"#.to_vec(),
                    None,
                ))
                .await
                .unwrap();

            let stored = collection.find_one(doc! {}).await.unwrap().unwrap();
            assert!(
                !stored.contains_key("seq"),
                "{format:?} should not add seq to inserted documents"
            );
            assert_eq!(
                collection.count_documents(doc! {}).await.unwrap(),
                1,
                "{format:?} should not create a sequencer document"
            );
            collection.drop().await.ok();
        }
    })
    .await;
}
