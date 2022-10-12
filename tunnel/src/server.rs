use lru::LruCache;
use rand::Rng;
use rustls::{HandshakeType, ProtocolVersion};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{ReadHalf, WriteHalf};
use tokio::net::{TcpStream, ToSocketAddrs};
use tracing::{debug, trace};

use std::fmt::Debug;
use std::io;
use std::mem;
use std::net::SocketAddr;
use std::sync::Mutex;

use crate::common::{
    derive_psk, EarlyData, SnowyStream, NOISE_PARAMS, NO_ELLIGATOR_WORKAROUND, PSKLEN,
    TLS_RECORD_HEADER_LENGTH,
};
use crate::totp::Totp;
use crate::utils::{
    get_client_tls_versions, get_server_tls_version, hmac, parse_tls_plain_message,
    read_tls_message, u16_from_be_slice, TlsMessageExt, Xor,
};

const SERVER_HELLO_RANDOM_START_INDEX: usize = TLS_RECORD_HEADER_LENGTH + 6;

/// Server with config to establish snowy tunnels with peer clients
#[derive(Debug)]
pub struct Server<A: ToSocketAddrs + Debug> {
    pub key: [u8; PSKLEN],
    pub camouflage_addr: A,
    pub replay_filter: Mutex<LruCache<[u8; 32], SocketAddr>>, // TODO: TOTP; prevent DoS attack
    pub totp: Totp,
    pub _curve_point_mask: [u8; 32],
}

impl<A: ToSocketAddrs + Debug> Server<A> {
    /// Create a server with a pre-shared key, a camouflage server address, and a capacity of the
    /// internal LRU-based replay filter queue.
    ///
    /// The camouflage server address is to where TLS handshakes from clients are forwarded and
    /// from where responses are forwarded backed to clients. Generally, it should match the server
    /// name specified in a tunnel's client-side.
    pub fn new(key: impl AsRef<[u8]>, camouflage_addr: A, replay_filter_size: usize) -> Self {
        let key = key.as_ref();
        Server {
            key: derive_psk(key),
            camouflage_addr,
            replay_filter: Mutex::new(LruCache::new(replay_filter_size)),
            totp: Totp::new(key, 60, 2),
            _curve_point_mask: hmac(NO_ELLIGATOR_WORKAROUND, key),
        }
    }

    /// Accept a incoming TcpStream as a [`SnowyStream`].
    ///
    /// See [`accept_with_early_data`](#method.accept_with_early_data) for more info.
    pub async fn accept(&self, inbound: TcpStream) -> Result<SnowyStream, AcceptError> {
        self.accept_with_early_data(inbound).await.map(|(s, _d)| s)
    }

    /// Accept a incoming TcpStream as a [`SnowyStream`].
    ///
    /// The server tries to authenticate a client by a Noise handshake message piggybacked by a TLS
    /// ClientHello (the first message in TLS handshakes). If the client is successfully
    /// authenticated as a tunnel peer, the server starts to forward traffic between the client and
    /// the camouflage server until TLS handshakes are finished. After that, the server sends back
    /// noise handshake in response to the client's challenge and transmute the connection into a
    /// snowy tunnel.
    ///
    /// If the client is not authenticated, it returns immediately with pending buffer exposed in
    /// [`AcceptError`]. The caller may decide to proceed to forward traffic between the client and
    /// the camouflage server on its own (falling back to dumb relay) or just reject/drop the
    /// connection.
    pub async fn accept_with_early_data(
        &self,
        mut inbound: TcpStream,
    ) -> Result<(SnowyStream, EarlyData), AcceptError> {
        use AcceptError::*;

        let mut responder = snow::Builder::new(NOISE_PARAMS.clone())
            .psk(0, &self.key)
            .build_responder()
            .expect("Valid NOISE params");
        let mut buf = Vec::new();

        // Ref: https://tls12.xargs.org/
        //      https://github.com/Gowee/rustls-mod/blob/a94a0055e1599d82bd8e212ad2dd19410204d5b7/rustls/src/msgs/message.rs#L88
        //   CH: record header + handshake header + server version + server random + session id len +
        //   session id + ..

        // Noise: -> psk, e
        let mut ping = [0u8; 64];
        let chp = match read_tls_message(&mut inbound, &mut buf)
            .await?
            .ok()
            .and_then(|_| parse_tls_plain_message(&buf).ok())
            .filter(|msg| msg.is_handshake_type(HandshakeType::ClientHello))
            .and_then(|msg| msg.into_client_hello_payload())
        {
            Some(chp) => chp,
            None => {
                return Err(ClientHelloInvalid { buf, io: inbound });
            }
        };

        chp.random.write_slice(&mut ping[..32]); // client random
        let s: (usize, [u8; 32]) = chp.session_id.into();
        ping[32..].copy_from_slice(&s.1[..32]); // session id

        let client_tls1_3 = get_client_tls_versions(&chp)
            .map(|vers| vers.iter().any(|&ver| ver == ProtocolVersion::TLSv1_3))
            .unwrap_or(false);
        trace!(
            "client {} supports TLS 1.3: {}",
            inbound.peer_addr().unwrap(),
            client_tls1_3
        );

        trace!("noise ping from {:?}, ping: {:x?}", &inbound, ping,);
        (&mut ping[..32]).xored(&self._curve_point_mask);
        let mut early_data = [0u8; 16];
        let mut verified = false;
        for token in self.totp.generate_current_skewed::<16>() {
            (&mut ping[48..64]).xored(&token);
            if responder.read_message(&ping, &mut early_data).is_ok() {
                verified = true;
                break;
            }
            (&mut ping[48..64]).xored(&token);
        }
        if !verified {
            return Err(Unauthenticated { buf, io: inbound });
        }
        debug!("authenticated {:?}", &inbound);
        debug!("early_data: {:x?}", early_data);
        {
            let e = ping[..32].try_into().unwrap();
            let mut rf = self.replay_filter.lock().unwrap();
            if let Some(&client_id) = rf.get(&e) {
                return Err(ReplayDetected {
                    buf,
                    io: inbound,
                    nonce: e,
                    first_from: client_id,
                });
            }
            rf.put(e, inbound.peer_addr().unwrap());
        }

        let mut outbound = TcpStream::connect(&self.camouflage_addr).await?;

        // forward Client Hello in whole to camouflage server
        outbound.write_all(&buf).await?;

        // read camouflage Server Hello back
        let shp = match read_tls_message(&mut outbound, &mut buf)
            .await?
            .ok()
            .and_then(|_| parse_tls_plain_message(&buf).ok())
            .filter(|msg| msg.is_handshake_type(HandshakeType::ServerHello))
            .and_then(|msg| msg.into_server_hello_payload())
        {
            Some(shp) => shp,
            None => {
                return Err(ServerHelloInvalid {
                    buf,
                    inbound,
                    outbound,
                });
            }
        };

        let mut pong = [0u8; 48];
        let len = responder
            .write_message(&[], &mut pong)
            .expect("Noise state valid");
        debug_assert_eq!(len, 48);
        trace!(
            // pad_len = pong.len() - (5 + 48),
            "e, ee to {:?}: {:x?}",
            inbound,
            &pong
        );
        (&mut pong[0..32]).xored(&self._curve_point_mask);

        match get_server_tls_version(&shp) {
            Some(ProtocolVersion::TLSv1_3) => {
                // TLS 1.3: handshake done
                debug!(
                    "{} <-> {} negotiated TLS version: 1.3",
                    inbound.peer_addr().unwrap(),
                    outbound.peer_addr().unwrap()
                );
                // forward camouflage server hello back to client
                inbound.write_all(&buf).await?;

                let len = rand::thread_rng().gen_range(108..908);
                buf.reserve_exact(TLS_RECORD_HEADER_LENGTH + len);
                unsafe { buf.set_len(TLS_RECORD_HEADER_LENGTH + len) };
                buf[..3].copy_from_slice(&[0x17, 0x03, 0x03]);
                buf[3..5].copy_from_slice(&(len as u16).to_be_bytes());
                buf[5..5 + 48].copy_from_slice(&pong);
                rand::thread_rng().fill(&mut buf[5 + 48..len - 48]);
                inbound.write_all(&buf).await?;
                trace!("written pong");
            }
            _ => {
                // TLS 1.2: continue handshake
                debug!(
                    "{} <-> {} negotiated TLS version: 1.2 or other",
                    inbound.peer_addr().unwrap(),
                    outbound.peer_addr().unwrap()
                );
                if chp.session_id == shp.session_id {
                    debug!("tls session resumed");
                    // session resumed
                    buf[SERVER_HELLO_RANDOM_START_INDEX..SERVER_HELLO_RANDOM_START_INDEX + 32]
                        .copy_from_slice(&pong[0..32]);

                    // forward camouflage server hello back to client
                    inbound.write_all(&buf).await?;

                    read_tls_message(&mut outbound, &mut buf)
                        .await?
                        .expect("TODO"); // ChangeCipherSpec
                    inbound.write_all(&buf).await?;
                    read_tls_message(&mut outbound, &mut buf)
                        .await?
                        .expect("TODO"); // Finished
                    buf[TLS_RECORD_HEADER_LENGTH..TLS_RECORD_HEADER_LENGTH + 16]
                        .copy_from_slice(&pong[32..48]);
                    inbound.write_all(&buf).await?;
                    debug!(
                        "{} <-> {} tls session resumed",
                        inbound.peer_addr().unwrap(),
                        outbound.peer_addr().unwrap()
                    );
                } else {
                    debug!("tls full handshaking");

                    // forward camouflage server hello back to client
                    inbound.write_all(&buf).await?;

                    relay_until_tls12_handshake_finished(&mut inbound, &mut outbound).await?;
                    debug!(
                        "{} <-> {} tls full handshake done",
                        inbound.peer_addr().unwrap(),
                        outbound.peer_addr().unwrap()
                    );

                    read_tls_message(&mut inbound, &mut buf)
                        .await?
                        .expect("TODO"); // dummy
                    debug!(len = buf.len(), "read dummy");

                    let len = rand::thread_rng().gen_range(108..908);
                    buf.reserve_exact(TLS_RECORD_HEADER_LENGTH + len);
                    unsafe { buf.set_len(TLS_RECORD_HEADER_LENGTH + len) };
                    buf[..3].copy_from_slice(&[0x17, 0x03, 0x03]);
                    buf[3..5].copy_from_slice(&(len as u16).to_be_bytes());
                    buf[5..5 + 48].copy_from_slice(&pong);
                    rand::thread_rng().fill(&mut buf[5 + 48..len - 48]);

                    inbound.write_all(&buf).await?;
                    trace!("written pong");
                }
            }
        }

        // handshake done, drop connection to camouflage server
        mem::drop(outbound);

        // // Noise: <- e, ee
        // let mut pong = [0u8; 5 + 48 + 24]; // 0 - 24 random padding
        // let pad_len = thread_rng().gen_range(0..=24);
        // rand::thread_rng().fill(&mut pong[5 + 48..5 + 48 + pad_len]);
        // pong[..5].copy_from_slice(&[0x17, 0x03, 0x03, 0x00, 0x30 + pad_len as u8]);
        // let len = responder
        //     .write_message(&[], &mut pong[5..])
        //     .expect("Noise state valid");
        // debug_assert_eq!(len, 48);
        // trace!(pad_len, "e, ee to {:?}: {:x?}", inbound, &pong[5..5 + 48]);
        // inbound.write_all(&pong[..5 + 48 + pad_len]).await?;
        // // but, is uniform random length of initial messages a characteristic per se?

        let responder = responder
            .into_transport_mode()
            .expect("Noise handshake done");
        trace!("noise handshake done with {:?}", inbound);
        Ok((SnowyStream::new(inbound, responder), early_data))
    }
}

/// Error returned by [`Server::accept`] with self-explanatory fields
pub enum AcceptError {
    IoError(io::Error),
    Unauthenticated {
        buf: Vec<u8>,
        io: TcpStream,
    },
    ReplayDetected {
        buf: Vec<u8>,
        io: TcpStream,
        nonce: [u8; 32],
        first_from: SocketAddr,
    },
    ClientHelloInvalid {
        buf: Vec<u8>,
        io: TcpStream,
    },
    ServerHelloInvalid {
        buf: Vec<u8>,
        inbound: TcpStream,
        outbound: TcpStream,
    },
}

impl From<io::Error> for AcceptError {
    fn from(err: io::Error) -> Self {
        Self::IoError(err)
    }
}

// Adapted from: https://github.com/ihciah/shadow-tls/blob/2bbdc26cff1120ba9c8eded39ad743c4c4f687c4/src/protocol.rs#L138
async fn copy_until_tls12_handshake_finished<'a>(
    mut read_half: ReadHalf<'a>,
    mut write_half: WriteHalf<'a>,
) -> io::Result<()> {
    const HANDSHAKE: u8 = 0x16;
    const CHANGE_CIPHER_SPEC: u8 = 0x14;
    // header_buf is used to read handshake frame header, will be a fixed size buffer.
    let mut header_buf = [0u8; 5];
    // data_buf is used to read and write data, and can be expanded.
    let mut data_buf = vec![0u8; 2048];
    let mut has_seen_change_cipher_spec = false;

    loop {
        // read exact 5 bytes
        read_half.read_exact(&mut header_buf).await?;

        // parse length
        let data_size = u16_from_be_slice(&header_buf[3..5]) as usize;

        // copy header and that much data
        write_half.write_all(&header_buf).await?;
        if data_size > data_buf.len() {
            data_buf.resize(data_size, 0);
        }
        read_half.read_exact(&mut data_buf[0..data_size]).await?;
        write_half.write_all(&data_buf[0..data_size]).await?;

        // check header type
        if header_buf[0] != HANDSHAKE {
            if header_buf[0] != CHANGE_CIPHER_SPEC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid TLS state",
                ));
            }
            if !has_seen_change_cipher_spec {
                has_seen_change_cipher_spec = true;
                continue;
            }
        }
        if has_seen_change_cipher_spec {
            break;
        }
    }
    Ok(())
}

async fn relay_until_tls12_handshake_finished(
    inbound: &mut TcpStream,
    outbound: &mut TcpStream,
) -> io::Result<()> {
    let (rin, win) = inbound.split();
    let (rout, wout) = outbound.split();
    let (a, b) = tokio::join!(
        copy_until_tls12_handshake_finished(rin, wout),
        copy_until_tls12_handshake_finished(rout, win)
    );
    a?;
    b?;
    Ok(())
}
