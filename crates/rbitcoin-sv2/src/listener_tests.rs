use crate::test_chain::padded_chain;
use crate::testutil::TpClient;
use crate::{run_sv2_tp, Sv2TpConfig, MAX_SESSIONS, SETUP_TIMEOUT};
use common_messages_sv2::{
    SetupConnectionError, SetupConnectionSuccess, MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
    MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use template_distribution_sv2::MESSAGE_TYPE_REQUEST_TRANSACTION_DATA_ERROR;

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
        stale_grace: Duration::from_secs(10),
        setup_timeout: SETUP_TIMEOUT,
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

/// key-utils 1.2.0 vector: SRI clients configure the TP authority key in
/// this form, so the handle must print it, and a client must connect with it.
#[tokio::test]
async fn authority_key_prints_in_key_utils_base58check() {
    let secret = bitcoin::base58::decode_check("zmBEmPhqo3A92FkiLVvyCz6htc3e53ph3ZbD4ASqGaLjwnFLi")
        .expect("vector secret");
    let tc = padded_chain("sv2-authority-key", 0);
    let tp = run_sv2_tp(Sv2TpConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        chain: std::sync::Arc::clone(&tc.chain),
        authority_secret: secret.try_into().expect("32-byte secret"),
        cert_validity: Duration::from_secs(3600),
        stale_grace: Duration::from_secs(10),
        setup_timeout: SETUP_TIMEOUT,
    })
    .await
    .expect("listen");
    let key = tp.authority_key();
    assert_eq!(key, "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan");

    let decoded = bitcoin::base58::decode_check(&key).unwrap();
    let pk: [u8; 32] = decoded[2..].try_into().unwrap();
    let mut c = TpClient::connect(tp.local_addr, pk)
        .await
        .expect("handshake against the printed key");
    c.setup_connection(TDP, 2, 2, 0).await.unwrap();
    let mut f = c.recv().await.expect("setup reply");
    assert_eq!(f.msg_type, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS);
    let _: SetupConnectionSuccess = binary_sv2::from_bytes(&mut f.payload).expect("decode");
    tp.shutdown().await;
}

/// Silent sockets take every slot at accept; the setup deadline must close
/// them so a real client gets in.
#[tokio::test]
async fn silent_sockets_are_dropped_at_the_setup_deadline() {
    use tokio::io::AsyncReadExt;

    let tc = padded_chain("sv2-setup-deadline", 0);
    let setup_timeout = Duration::from_millis(300);
    let tp = run_sv2_tp(Sv2TpConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        chain: std::sync::Arc::clone(&tc.chain),
        authority_secret: [7; 32],
        cert_validity: Duration::from_secs(3600),
        stale_grace: Duration::from_secs(10),
        setup_timeout,
    })
    .await
    .expect("listen");
    let (addr, pk) = (tp.local_addr, tp.authority_pubkey);

    let mut silent = Vec::new();
    for _ in 0..MAX_SESSIONS {
        silent.push(tokio::net::TcpStream::connect(addr).await.expect("tcp"));
    }
    for s in &mut silent {
        let mut b = [0u8; 1];
        let closed = tokio::time::timeout(setup_timeout * 10, s.read(&mut b)).await;
        assert!(
            matches!(closed, Ok(Ok(0) | Err(_))),
            "silent socket must be closed after the setup deadline"
        );
    }

    let mut c = connect_when_free(addr, pk).await;
    c.setup_connection(TDP, 2, 2, 0).await.unwrap();
    let f = c.recv().await.expect("setup reply");
    assert_eq!(f.msg_type, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS);
    tp.shutdown().await;
}

/// A TDP session without `CoinbaseOutputConstraints` never gets a template
/// and never writes, so the setup deadline also covers the first constraints.
/// Other frames before them do not reset it.
#[tokio::test]
async fn session_without_constraints_is_dropped_at_the_setup_deadline() {
    let tc = padded_chain("sv2-constraints-deadline", 0);
    let setup_timeout = Duration::from_millis(300);
    let tp = run_sv2_tp(Sv2TpConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        chain: std::sync::Arc::clone(&tc.chain),
        authority_secret: [7; 32],
        cert_validity: Duration::from_secs(3600),
        stale_grace: Duration::from_secs(10),
        setup_timeout,
    })
    .await
    .expect("listen");
    let (addr, pk) = (tp.local_addr, tp.authority_pubkey);

    let mut idle = TpClient::connect(addr, pk).await.expect("handshake");
    idle.setup_connection(TDP, 2, 2, 0).await.unwrap();
    idle.recv().await.expect("setup reply");
    idle.request_transaction_data(1).await.unwrap();
    let f = idle
        .recv()
        .await
        .expect("request answered before constraints");
    assert_eq!(f.msg_type, MESSAGE_TYPE_REQUEST_TRANSACTION_DATA_ERROR);

    let mut ok = TpClient::connect(addr, pk).await.expect("handshake");
    ok.setup_connection(TDP, 2, 2, 0).await.unwrap();
    ok.recv().await.expect("setup reply");
    ok.coinbase_output_constraints(1, 1).await.unwrap();

    let closed = tokio::time::timeout(setup_timeout * 10, async {
        while idle.recv().await.is_ok() {}
    })
    .await;
    assert!(closed.is_ok(), "session without constraints must be closed");

    tokio::time::sleep(setup_timeout * 2).await;
    ok.request_transaction_data(u64::MAX).await.unwrap();
    loop {
        let f = ok
            .recv()
            .await
            .expect("session with constraints stays open");
        if f.msg_type == MESSAGE_TYPE_REQUEST_TRANSACTION_DATA_ERROR {
            break;
        }
    }
    tp.shutdown().await;
}
