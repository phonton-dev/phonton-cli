use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

struct ServeProcess(Child);

impl Drop for ServeProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn rpc(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Value {
    client
        .post(url)
        .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn stale_desktop_model_start_is_rejected_before_rpc_admission() {
    let fixture = tempfile::tempdir().unwrap();
    let state = fixture.path().join("state.json");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let _server = ServeProcess(
        Command::new(env!("CARGO_BIN_EXE_phonton"))
            .args(["serve", "--port", &port.to_string()])
            .env("PHONTON_LOCAL_STATE", &state)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let mut ready = false;
    for _ in 0..80 {
        if client
            .get(format!("{base}/health"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "isolated local sidecar did not become ready");
    let url = format!("{base}/rpc");
    let changed = rpc(
        &client,
        &url,
        "models.endpoint.set",
        json!({"endpoint":"http://127.0.0.1:11435"}),
    )
    .await;
    assert_eq!(changed["result"]["endpoint"], "http://127.0.0.1:11435");
    let before = std::fs::read(&state).unwrap();

    // Setup cannot use the new endpoint either. If admission accidentally
    // accepts this request, it would still return an operation ID here.
    let rejected = rpc(
        &client,
        &url,
        "models.start",
        json!({
            "kind":"setup", "model":"", "context":null,
            "expected_endpoint":"http://127.0.0.1:11434",
            "expected_storage_root":fixture.path().join("runtime"),
        }),
    )
    .await;
    assert!(rejected["result"].is_null());
    assert!(rejected["error"]["message"]
        .as_str()
        .unwrap()
        .contains("endpoint changed"));
    let operation = rpc(&client, &url, "models.operation", json!({})).await;
    assert_eq!(operation["result"]["id"], "");
    assert_eq!(operation["result"]["running"], false);
    assert_eq!(std::fs::read(&state).unwrap(), before);

    let accepted = rpc(
        &client,
        &url,
        "models.start",
        json!({
            "kind":"deselect", "model":"", "context":null,
            "expected_endpoint":"http://127.0.0.1:11435",
            "expected_storage_root":fixture.path().join("runtime"),
        }),
    )
    .await;
    let accepted_id = accepted["result"]["id"].as_str().unwrap();
    assert!(!accepted_id.is_empty());
    let mut completed = false;
    for _ in 0..20 {
        let operation = rpc(&client, &url, "models.operation", json!({})).await;
        assert_eq!(operation["result"]["id"], accepted_id);
        if operation["result"]["running"] == false {
            assert_eq!(operation["result"]["result"]["changed"], false);
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(completed, "matching model operation did not finish");
}
