//! Stratum v2 Template Distribution Protocol server (sv2-spec 07).
//!
//! Noise_NX over TCP is the only transport. Plan and constraints:
//! `docs/sv2-template-provider.md`.

mod session;
mod template;
pub mod testutil;
mod transport;

use bitcoin::secp256k1::{Keypair, Secp256k1};
use rbitcoin_net::ChainHub;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

pub use transport::Frame;

/// Concurrent TDP sessions. RAM trade (CONTRIBUTING 9): each session retains
/// the witness-serialized txs of its live templates (≤ ~4 MB × ~3), so the
/// cap bounds retention at ≤ ~96 MB.
pub const MAX_SESSIONS: usize = 8;

pub struct Sv2TpConfig {
    pub listen: SocketAddr,
    /// Tip, params, and the attached mempool the templates are built from.
    pub chain: Arc<ChainHub>,
    /// Authority secret key; clients pin its x-only public key.
    pub authority_secret: [u8; 32],
    /// Validity of the per-connection Noise certificate signed by the authority.
    pub cert_validity: Duration,
    /// How long a template on a replaced prev hash still answers requests.
    pub stale_grace: Duration,
}

pub struct Sv2TpHandle {
    pub local_addr: SocketAddr,
    /// X-only authority public key the clients verify the certificate against.
    pub authority_pubkey: [u8; 32],
    task: JoinHandle<()>,
    sessions: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Sv2TpHandle {
    /// `authority_pubkey` as SRI `key-utils` prints it (the form JDC and pool
    /// configs take): base58check of version `1u16` LE, then the x-only key.
    pub fn authority_key(&self) -> String {
        let mut v = [0u8; 34];
        v[..2].copy_from_slice(&1u16.to_le_bytes());
        v[2..].copy_from_slice(&self.authority_pubkey);
        bitcoin::base58::encode_check(&v)
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for t in sessions.drain(..) {
            t.abort();
        }
    }
}

/// Bind the listener and serve TDP sessions until [`Sv2TpHandle::shutdown`].
///
/// At [`MAX_SESSIONS`] the next connection is closed before the handshake;
/// existing sessions are not touched.
pub async fn run_sv2_tp(config: Sv2TpConfig) -> io::Result<Sv2TpHandle> {
    let keypair =
        Keypair::from_seckey_slice(&Secp256k1::new(), &config.authority_secret).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("sv2 authority key: {e}"),
            )
        })?;
    let authority_pubkey = keypair.x_only_public_key().0.serialize();
    let authority_secret = config.authority_secret;
    let cert_validity = config.cert_validity;
    let stale_grace = config.stale_grace;
    let chain = config.chain;
    let listener = TcpListener::bind(config.listen).await?;
    let local_addr = listener.local_addr()?;
    let slots = Arc::new(Semaphore::new(MAX_SESSIONS));
    let sessions: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let sessions_c = sessions.clone();

    let task = tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(a) => a,
                Err(e) => {
                    rbitcoin_log::warn!("sv2: accept failed ({e})");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(slot) = slots.clone().try_acquire_owned() else {
                rbitcoin_log::warn!("sv2: reject {peer} (at max_sessions={MAX_SESSIONS})");
                drop(stream);
                continue;
            };
            let responder = match noise_sv2::Responder::from_authority_kp(
                &authority_pubkey,
                &authority_secret,
                cert_validity,
            ) {
                Ok(r) => r,
                Err(e) => {
                    rbitcoin_log::warn!("sv2: responder for {peer} ({e:?})");
                    continue;
                }
            };
            rbitcoin_log::info!("sv2: connect {peer}");
            let chain = Arc::clone(&chain);
            let h = tokio::spawn(async move {
                let _slot = slot;
                match session::serve(stream, responder, chain, stale_grace).await {
                    Ok(()) => rbitcoin_log::info!("sv2: disconnect {peer}"),
                    Err(e) => rbitcoin_log::info!("sv2: disconnect {peer} ({e})"),
                }
            });
            let mut g = sessions_c.lock().unwrap_or_else(|e| e.into_inner());
            g.retain(|t| !t.is_finished());
            g.push(h);
        }
    });

    Ok(Sv2TpHandle {
        local_addr,
        authority_pubkey,
        task,
        sessions,
    })
}

#[cfg(test)]
mod listener_tests;
#[cfg(test)]
mod template_tests;
#[cfg(test)]
mod test_chain;
