use crate::test_chain::padded_chain;
use crate::testutil::TpClient;
use crate::{run_sv2_tp, Sv2TpConfig, MAX_SESSIONS};
use common_messages_sv2::{
    SetupConnectionError, SetupConnectionSuccess, MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
    MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

const TDP: u8 = 2;

async fn connect_when_free(addr: SocketAddr, pk: [u8; 32]) -> TpClient {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match TpClient::connect(addr, pk).await {
            Ok(c) => return c,
            Err(e) if Instant::now() > deadline => panic!("no free session slot: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

async fn expect_error(c: &mut TpClient, flags: u32, code: &str) {
    let mut f = c.recv().await.expect("setup reply");
    assert_eq!(f.msg_type, MESSAGE_TYPE_SETUP_CONNECTION_ERROR);
    let e: SetupConnectionError = binary_sv2::from_bytes(&mut f.payload).expect("decode");
    assert_eq!(e.flags, flags);
    assert_eq!(e.error_code.as_utf8_or_hex(), code);
    let closed = tokio::time::timeout(Duration::from_secs(5), c.recv()).await;
    assert!(
        matches!(closed, Ok(Err(_))),
        "connection must close after SetupConnection.Error"
    );
}

#[tokio::test]
async fn setup_connection_success_errors_and_session_cap() {
    let tc = padded_chain("sv2-listener", 0);
    let tp = run_sv2_tp(Sv2TpConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        chain: tc.chain.clone(),
        authority_secret: [7; 32],
        cert_validity: Duration::from_secs(3600),
    })
    .await
    .expect("listen");
    let (addr, pk) = (tp.local_addr, tp.authority_pubkey);

    let mut live = Vec::new();
    for _ in 0..MAX_SESSIONS {
        let mut c = TpClient::connect(addr, pk).await.expect("handshake");
        c.setup_connection(TDP, 2, 2, 0).await.unwrap();
        let mut f = c.recv().await.expect("setup reply");
        assert_eq!(f.msg_type, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS);
        let ok: SetupConnectionSuccess = binary_sv2::from_bytes(&mut f.payload).expect("decode");
        assert_eq!((ok.used_version, ok.flags), (2, 0));
        live.push(c);
    }

    let extra = TpClient::connect(addr, pk).await;
    assert!(
        extra.is_err(),
        "session over the cap must close before the handshake"
    );
    for c in live.iter_mut().step_by(MAX_SESSIONS - 1) {
        let still_open = tokio::time::timeout(Duration::from_millis(200), c.recv()).await;
        assert!(
            still_open.is_err(),
            "existing session must stay up at the cap"
        );
    }
    drop(live);

    let mut c = connect_when_free(addr, pk).await;
    c.setup_connection(TDP, 2, 2, 0b101).await.unwrap();
    expect_error(&mut c, 0b101, "unsupported-feature-flags").await;

    let mut c = connect_when_free(addr, pk).await;
    c.setup_connection(0, 2, 2, 0).await.unwrap();
    expect_error(&mut c, 0, "unsupported-protocol").await;

    let mut c = connect_when_free(addr, pk).await;
    c.setup_connection(TDP, 3, 4, 0).await.unwrap();
    expect_error(&mut c, 0, "protocol-version-mismatch").await;

    tp.shutdown().await;
}
