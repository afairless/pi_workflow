//! Integration tests for the RPC client against the scripted fake-pi peer
//! (`test-fixtures/rpc-peer/fake_pi.py`): framing edge cases, dialog
//! round-trips (select ok/cancel, input), out-of-order response correlation,
//! abort, deadline expiry, and fatal bad-frame handling.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use pi_plan::rpc::{RpcClient, RpcCommand, RpcError, RpcEvent, SpawnOptions, UiMethod, UiReply};
use tokio::sync::broadcast::error::RecvError;

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

const FAKE_PI: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/test-fixtures/rpc-peer/fake_pi.py"
);

fn case(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test-fixtures/rpc-peer/cases")
        .join(name)
}

/// Spawn the fake peer for a case; each test gets a unique stderr log.
async fn spawn_client(case_name: &str) -> RpcClient {
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let stderr_path =
        std::env::temp_dir().join(format!("pi-plan-rpc-test-{}-{n}.log", std::process::id()));
    let opts = SpawnOptions {
        binary: "python3".to_string(),
        args: vec![
            FAKE_PI.to_string(),
            case(case_name).to_string_lossy().into_owned(),
        ],
        cwd: Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf(),
        stderr_path: Some(stderr_path),
    };
    RpcClient::spawn(&opts).await.expect("spawn fake peer")
}

/// Wait for the next event on the subscription, with a generous deadline.
async fn recv_event(
    rx: &mut tokio::sync::broadcast::Receiver<RpcEvent>,
    timeout: Duration,
) -> Result<RpcEvent, String> {
    tokio::time::timeout(timeout, rx.recv())
        .await
        .map_err(|_| "timed out waiting for event".to_string())?
        .map_err(|e| match e {
            RecvError::Lagged(n) => format!("event channel lagged by {n}"),
            RecvError::Closed => "event stream closed".to_string(),
        })
}

/// Wait until the client reports the peer dead (read loop terminated).
async fn wait_dead(client: &RpcClient, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while !client.is_dead() {
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

#[tokio::test]
async fn dialog_select_round_trip_answers_inline() {
    let client = spawn_client("dialog_select_ok.jsonl").await;
    let mut events = client.subscribe().await.expect("subscribe before death");

    let resp = client
        .request(RpcCommand::Prompt {
            message: "hello".to_string(),
            streaming_behavior: None,
        })
        .await
        .expect("prompt accepted");
    assert!(resp.success, "prompt response success");

    let dialog = recv_event(&mut events, Duration::from_secs(5))
        .await
        .expect("dialog event");
    match dialog {
        RpcEvent::ExtensionUiRequest(req) => {
            assert_eq!(req.id, "dialog-1");
            assert_eq!(req.method, UiMethod::Select);
            assert_eq!(req.title.as_deref(), Some("Allow dangerous command?"));
            assert_eq!(req.options, ["Allow", "Block"]);
            assert!(req.is_dialog());
            client
                .reply_extension_ui(&req.id, &UiReply::Value("Allow".to_string()))
                .await
                .expect("reply");
        }
        other => panic!("expected a dialog, got {other:?}"),
    }

    match recv_event(&mut events, Duration::from_secs(5))
        .await
        .expect("settled")
    {
        RpcEvent::AgentSettled => {}
        other => panic!("expected agent_settled, got {other:?}"),
    }
    let _ = client.kill().await;
}

#[tokio::test]
async fn dialog_select_cancel_dismisses() {
    let client = spawn_client("dialog_select_cancel.jsonl").await;
    let mut events = client.subscribe().await.expect("subscribe");

    let resp = client
        .request(RpcCommand::Prompt {
            message: "hello".to_string(),
            streaming_behavior: None,
        })
        .await
        .expect("prompt accepted");
    assert!(resp.success);

    match recv_event(&mut events, Duration::from_secs(5))
        .await
        .expect("dialog")
    {
        RpcEvent::ExtensionUiRequest(req) => {
            assert_eq!(req.id, "dialog-2");
            client
                .reply_extension_ui(&req.id, &UiReply::Cancelled)
                .await
                .expect("reply cancelled");
        }
        other => panic!("expected a dialog, got {other:?}"),
    }
    match recv_event(&mut events, Duration::from_secs(5))
        .await
        .expect("settled")
    {
        RpcEvent::AgentSettled => {}
        other => panic!("expected agent_settled, got {other:?}"),
    }
    let _ = client.kill().await;
}

#[tokio::test]
async fn dialog_input_exchange_returns_text() {
    let client = spawn_client("dialog_input.jsonl").await;
    let mut events = client.subscribe().await.expect("subscribe");

    let resp = client
        .request(RpcCommand::Prompt {
            message: "hello".to_string(),
            streaming_behavior: None,
        })
        .await
        .expect("prompt accepted");
    assert!(resp.success);

    match recv_event(&mut events, Duration::from_secs(5))
        .await
        .expect("dialog")
    {
        RpcEvent::ExtensionUiRequest(req) => {
            assert_eq!(req.method, UiMethod::Input);
            assert_eq!(req.placeholder.as_deref(), Some("type something..."));
            client
                .reply_extension_ui(&req.id, &UiReply::Value("some text".to_string()))
                .await
                .expect("reply");
        }
        other => panic!("expected a dialog, got {other:?}"),
    }
    match recv_event(&mut events, Duration::from_secs(5))
        .await
        .expect("settled")
    {
        RpcEvent::AgentSettled => {}
        other => panic!("expected agent_settled, got {other:?}"),
    }
    let _ = client.kill().await;
}

#[tokio::test]
async fn out_of_order_responses_correlate_by_id() {
    let client = Arc::new(spawn_client("out_of_order.jsonl").await);

    // Two concurrent GET_MESSAGES requests. The peer answers the SECOND
    // request's frame first (the first is delayed 400 ms); id correlation
    // must hand each response to its own request.
    let client_a = Arc::clone(&client);
    let (tx_done, mut rx_done) = tokio::sync::mpsc::unbounded_channel();
    let txa = tx_done.clone();
    let handle_a = tokio::spawn(async move {
        let resp = client_a
            .request(RpcCommand::GetMessages)
            .await
            .expect("first request resolves");
        let tag = resp.data.and_then(|d| d.get("tag").cloned());
        let _ = txa.send(tag);
    });

    // Send the second request a little later so the first reaches the peer
    // first (its response is the delayed 400 ms one).
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client_b = Arc::clone(&client);
    let txb = tx_done.clone();
    let handle_b = tokio::spawn(async move {
        let resp = client_b
            .request(RpcCommand::GetMessages)
            .await
            .expect("second request resolves");
        let tag = resp.data.and_then(|d| d.get("tag").cloned());
        let _ = txb.send(tag);
    });

    let first = tokio::time::timeout(Duration::from_secs(5), rx_done.recv())
        .await
        .expect("first completion within timeout")
        .expect("completion channel open")
        .expect("tag present")
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        first, "second",
        "the delayed response must not win the race"
    );

    let second = tokio::time::timeout(Duration::from_secs(5), rx_done.recv())
        .await
        .expect("second completion within timeout")
        .expect("completion channel open")
        .expect("tag present")
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(second, "first");

    handle_a.await.expect("task a");
    handle_b.await.expect("task b");
    let _ = client.kill().await;
}

#[tokio::test]
async fn abort_command_succeeds() {
    let client = spawn_client("abort_ok.jsonl").await;
    let resp = client
        .request(RpcCommand::Abort)
        .await
        .expect("abort answered");
    assert!(resp.success);
    assert_eq!(resp.command, "abort");
    let _ = client.kill().await;
}

#[tokio::test]
async fn command_deadline_expires_with_error() {
    let mut client = spawn_client("timeout_case.jsonl").await;
    client.set_command_timeout(Duration::from_millis(300));

    let err = client
        .request(RpcCommand::GetSessionStats)
        .await
        .expect_err("300 ms deadline beats the peer's 3000 ms answer");
    assert!(
        matches!(err, RpcError::Timeout { .. }),
        "expected a timeout error, got {err:?}"
    );
    let _ = client.kill().await;
}

#[tokio::test]
async fn bad_frame_is_a_fatal_protocol_error() {
    let client = spawn_client("bad_frame.jsonl").await;
    // The peer emits garbage immediately after startup; the read loop must
    // die and every subsequent request must fail rather than hang.
    let dead = wait_dead(&client, Duration::from_secs(5)).await;
    assert!(dead, "peer must be marked dead after a bad frame");

    let err = client
        .request(RpcCommand::GetSessionStats)
        .await
        .expect_err("request after protocol death");
    assert!(
        matches!(err, RpcError::Protocol(_) | RpcError::PeerClosed),
        "got {err:?}"
    );
    assert!(client.subscribe().await.is_none(), "no events after death");
    let _ = client.kill().await;
}

#[tokio::test]
async fn oversized_frame_is_a_fatal_protocol_error() {
    let client = spawn_client("oversize_frame.jsonl").await;
    let dead = wait_dead(&client, Duration::from_secs(15)).await;
    assert!(dead, "peer must be marked dead after an oversized frame");

    let err = client
        .request(RpcCommand::Abort)
        .await
        .expect_err("request after protocol death");
    assert!(
        matches!(err, RpcError::Protocol(_) | RpcError::PeerClosed),
        "got {err:?}"
    );
    let _ = client.kill().await;
}

#[tokio::test]
async fn crlf_terminated_frames_are_accepted() {
    let client = spawn_client("crlf_frame.jsonl").await;
    let mut events = client.subscribe().await.expect("subscribe");
    match recv_event(&mut events, Duration::from_secs(5))
        .await
        .expect("settled")
    {
        RpcEvent::AgentSettled => {}
        other => panic!("expected agent_settled, got {other:?}"),
    }
    let _ = client.kill().await;
}
