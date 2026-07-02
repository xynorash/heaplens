use std::time::Duration;

use heaplens_daemon::msg::StoreMsg;
use heaplens_daemon::store;
use heaplens_protocol::{NodeDto, NodeState};

fn make_dto(id: u64) -> NodeDto {
    NodeDto {
        id,
        ptr: id * 1000,
        size: 64,
        ts: 100,
        symbol: "test".to_owned(),
        live: true,
        state: NodeState::Healthy,
        edges: vec![],
    }
}

fn temp_db_path(name: &str) -> String {
    format!(
        "{}\\heaplens_test_{}.db",
        std::env::temp_dir().display(),
        name
    )
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn store_inserts_nodes() {
    let path = temp_db_path("insert");
    cleanup(&path);

    let tx = store::open(&path).unwrap();
    tx.send(StoreMsg::Nodes(vec![make_dto(1), make_dto(2), make_dto(3)]))
        .unwrap();
    tx.send(StoreMsg::Flush).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let conn = rusqlite::Connection::open(&path).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 3);

    cleanup(&path);
}

#[tokio::test]
async fn store_batches_on_timer() {
    let path = temp_db_path("batch");
    cleanup(&path);

    let tx = store::open(&path).unwrap();
    for i in 0..50u64 {
        tx.send(StoreMsg::Nodes(vec![make_dto(i)])).unwrap();
    }
    // No Flush — wait for the 100ms batch timer plus margin
    tokio::time::sleep(Duration::from_millis(400)).await;

    let conn = rusqlite::Connection::open(&path).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 50);

    cleanup(&path);
}

#[tokio::test]
async fn store_shutdown_flushes() {
    let path = temp_db_path("shutdown");
    cleanup(&path);

    {
        let tx = store::open(&path).unwrap();
        tx.send(StoreMsg::Nodes(vec![make_dto(99)])).unwrap();
        tx.send(StoreMsg::Shutdown).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        // tx dropped here
    }

    let conn = rusqlite::Connection::open(&path).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert!(count >= 1, "shutdown should flush pending rows, got {count}");

    cleanup(&path);
}
