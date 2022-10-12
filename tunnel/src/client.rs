use derivative::Derivative;
use rand::Rng;
use rustls::internal::msgs::enums::ExtensionType;
use rustls::{
    ClientConnection as RustlsClientConnection, ContentType as TlsContentType, HandshakeType,
    ProtocolVersion, ServerName,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::warn;
use tracing::{debug, trace};

use std::io::{self, Write};
use std::mem::{self, MaybeUninit};
use std::sync::Arc;

use crate::common::NO_ELLIGATOR_WORKAROUND;
use crate::totp::Totp;
use crate::utils::{hmac, parse_tls_plain_message, u16_from_be_slice, Xor};
use crate::FingerprintSpec;

use crate::utils::{
    get_server_tls_version, read_tls_message, NoCertificateVerification, TlsMessageExt,
};

use super::common::{
    derive_psk, SnowyStream, DEFAULT_ALPN_PROTOCOLS, MAXIMUM_CIPHERTEXT_LENGTH, NOISE_PARAMS,
    PSKLEN, TLS_RECORD_HEADER_LENGTH,
};

/// Client with config to establish snowy tunnels with peer servers
#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct Client {
    pub key: [u8; PSKLEN],
    pub server_name: ServerName,
    #[derivative(Debug = "ignore")]
    pub tlsconf: rustls::ClientConfig,
    pub fingerprint_spec: Arc<FingerprintSpec>,
    pub totp: Totp,
    pub _curve_point_mask: [u8; 32],
    // pub verify_tls: bool,
}

impl Client {
    /// Create a client with a pre-shared key and a server name for camouflage
    ///
    /// The server name would be sent out as [Server Name Indication](https://en.wikipedia.org/wiki/Server_Name_Indication).
    /// Generally, it should match the camouflage server address specified on a tunnel's server-side. .
    pub fn new(key: impl AsRef<[u8]>, server_name: ServerName) -> Self {
        Self::new_with_fingerprint(key, server_name, Default::default())
    }

    /// Create a client with a pre-shared key, a server name for camouflage and additionally a
    /// fingerprint specification used to apply to TLS ClientHello
    pub fn new_with_fingerprint(
        key: impl AsRef<[u8]>,
        server_name: ServerName,
        fingerprint_spec: FingerprintSpec,
    ) -> Self {
        let key = key.as_ref();

        // TODO: option for verifying camouflage cert
        let mut tlsconf = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification {}))
            .with_no_client_auth();
        if let Some(ref ja3) = fingerprint_spec.ja3 {
            // fingerprint_spec.alpn is effective iff alpn is set in ja3
            if ja3
                .extensions_as_typed()
                .any(|ext| ext == ExtensionType::ALProtocolNegotiation)
            {
                // It is necessary to add it to conf. Only adding it to allowed_unsolicited_extensions
                // resulted in TLS client rejection when ALPN is negeotiated.
                tlsconf.alpn_protocols = fingerprint_spec
                    .alpn
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| Vec::from(DEFAULT_ALPN_PROTOCOLS.map(Vec::from)));
            }
        }

        Client {
            key: derive_psk(key),
            server_name,
            tlsconf,
            fingerprint_spec: Arc::new(fingerprint_spec),
            totp: Totp::new(key, 60, 2),
            _curve_point_mask: hmac(NO_ELLIGATOR_WORKAROUND, key),
        }
    }

    /// Handshake with a peer server on the other end of the `TcpStream`
    #[inline(always)]
    pub async fn connect(&self, stream: TcpStream) -> io::Result<SnowyStream> {
        self.connect_with_early_data(stream, [0u8; 16]).await
    }

    /// Handshake with a peer server on the other end of the `TcpStream`, sending a early data
    /// piggybacked by ClientHello
    ///
    /// The early data embeded covertly in ClientHello session id along with Noise handshake. And
    /// it has nothing to do with TLS 1.3 early data.
    pub async fn connect_with_early_data(
        &self,
        mut stream: TcpStream,
        early_data: [u8; 16],
    ) -> io::Result<SnowyStream> {
        let mut initiator = snow::Builder::new(NOISE_PARAMS.clone())
            .psk(0, &self.key)
            .build_initiator()
            .expect("Noise params valid");
        // Noise: -> psk, e
        let mut ping = [0u8; 64];
        let time_token = self.totp.generate_current::<16>();
        initiator
            .write_message(&early_data, &mut ping)
            .expect("Noise state valid");
        // Mask the curve point to avoid being distinguished. It is a temporary workaround.
        // We should have used Elligator. But there seems no working implementation in Rust for now.
        (&mut ping[0..32]).xored(&self._curve_point_mask);
        // sign AEAD tag with time (many-time pad should be generally secure since tag is random)
        (&mut ping[48..64]).xored(&time_token);
        let random = <[u8; 32]>::try_from(&ping[0..32]).unwrap().into();
        let session_id = <[u8; 32]>::try_from(&ping[32..64])
            .unwrap()
            .as_slice()
            .into();
        trace!("noise ping to {:?}, ping: {:x?}", &stream, ping,);

        let chwriter = self
            .fingerprint_spec
            .get_client_hello_overwriter(true, true);
        let mut tlsconn = rustls::ClientConnection::new_with(
            Arc::new(self.tlsconf.clone()),
            self.server_name.clone(),
            random,
            Some(session_id),
            None,
            None,
            chwriter,
        )
        .expect("TLS config valid");

        let mut buf: Vec<MaybeUninit<u8>> =
            Vec::with_capacity(TLS_RECORD_HEADER_LENGTH + MAXIMUM_CIPHERTEXT_LENGTH);
        let mut buf: Vec<u8> = unsafe {
            buf.set_len(buf.capacity());
            mem::transmute(buf)
        };
        let len = tlsconn.write_tls(&mut io::Cursor::new(&mut buf))?; // Write for Vec is dummy?
        unsafe { buf.set_len(len) };
        debug_assert!(!tlsconn.wants_write() & tlsconn.wants_read());
        stream.write_all(&buf).await?; // forward Client Hello

        // read Server Hello
        let shp = read_tls_message(&mut stream, &mut buf)
            .await?
            .ok()
            .and_then(|_| parse_tls_plain_message(&buf).ok())
            .filter(|msg| msg.is_handshake_type(HandshakeType::ServerHello))
            .and_then(|msg| msg.into_server_hello_payload())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Not or invalid Server Hello")
            })?;

        let mut pong = [0u8; 48];

        // server negotiated TLS version
        match get_server_tls_version(&shp) {
            Some(ProtocolVersion::TLSv1_3) => {
                // TLS 1.3: handshake treated as done
                // In TLS 1.3, all messages after client/server hello are encrypted by the session
                // key generated by ECDHE. An eavesdropper won't be able to see certificate and
                // certificate verify (signature of ECDHE public key). So there is no need to copy
                // handshake procedures any more. Actually, even Server Hello can also be
                // fabricated locally without be distinguished. Here the fingerprint in ServerHello
                // is useful, though.
                // TODO: Cache SH for latter use instead of request camouflage server every time.
                // TODO: Send mibble box compatibility CCS and more ApplicationData frames, as
                //   in typical TLS 1.3 handshake.

                // Noise: <- e, ee
                read_tls_message(&mut stream, &mut buf)
                    .await?
                    .map_err(|_e| {
                        io::Error::new(io::ErrorKind::InvalidData, "First data frame not noise")
                    })?; // TODO: timeout
                if buf.len() < 5 + 48 {
                    warn!(
                        "Noise handshake {} <-> {} failed. Wrong key or time out of sync?",
                        stream.local_addr().unwrap(),
                        stream.peer_addr().unwrap()
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Noise handshake failed due to message length shorter than expected",
                    ));
                }
                pong[0..48]
                    .copy_from_slice(&buf[TLS_RECORD_HEADER_LENGTH..TLS_RECORD_HEADER_LENGTH + 48]);
                (&mut pong[0..32]).xored(&self._curve_point_mask);
            }
            _ => {
                // TLS 1.2: conitnue full handshake via rustls
                // In TLS 1.2, the handshake procedures are basically transparent. That is, an
                // eavesdropper could verify the unencrypted signature against the camouflage
                // servers' public key. So the camouflage server is requested every time.
                if shp.session_id == session_id {
                    // tls session resumed
                    pong[0..32].copy_from_slice(&shp.random.0);
                    (&mut pong[0..32]).xored(&self._curve_point_mask);

                    read_tls_message(&mut stream, &mut buf)
                        .await?
                        .expect("TODO"); // CCS
                    read_tls_message(&mut stream, &mut buf)
                        .await?
                        .expect("TODO"); // Finished
                    if pong.len() < 16 {
                        warn!(
                            "Noise handshake {} <-> {} failed: Expected handshake Finished, got something too short",
                            stream.local_addr().unwrap(),
                            stream.peer_addr().unwrap()
                        );
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Noise handshake failed due to message length shorter than expected",
                        ));
                    }
                    pong[32..48].copy_from_slice(
                        &buf[TLS_RECORD_HEADER_LENGTH..TLS_RECORD_HEADER_LENGTH + 16],
                    );
                } else {
                    // feed previously read Server Hello
                    tlsconn.read_tls(&mut io::Cursor::new(&mut buf))?;
                    tls12_handshake(&mut tlsconn, &mut stream, false).await?;
                    // TLS1.2 handshake done
                    // send a dummy packet as camouflage
                    let len = rand::thread_rng().gen_range(108..908);
                    buf.reserve_exact(TLS_RECORD_HEADER_LENGTH + len);
                    unsafe { buf.set_len(TLS_RECORD_HEADER_LENGTH + len) };
                    debug_assert!(TLS_RECORD_HEADER_LENGTH + len <= buf.capacity());
                    buf[..3].copy_from_slice(&[0x17, 0x03, 0x03]);
                    buf[3..5].copy_from_slice(&(len as u16).to_be_bytes());
                    rand::thread_rng()
                        .fill(&mut buf[TLS_RECORD_HEADER_LENGTH..TLS_RECORD_HEADER_LENGTH + len]);
                    stream.write_all(&buf).await?;
                    trace!(len, "write dummy");
                    read_tls_message(&mut stream, &mut buf)
                        .await?
                        .expect("TODO");
                    if buf.len() < 5 + 48 {
                        warn!(
                            "Noise handshake {} <-> {} failed. Wrong key or time out of sync?",
                            stream.local_addr().unwrap(),
                            stream.peer_addr().unwrap()
                        );
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Noise handshake failed due to message length shorter than expected",
                        ));
                    }
                    pong[0..48].copy_from_slice(
                        &buf[TLS_RECORD_HEADER_LENGTH..TLS_RECORD_HEADER_LENGTH + 48],
                    );
                    (&mut pong[0..32]).xored(&self._curve_point_mask);
                }
            }
        }

        // let e_ee: [u8; 48] = pong[5..5 + 48].try_into().unwrap(); // 32B pubkey + 16B AEAD tag
        trace!(
            // pad_len = pong.len() - (5 + 48),
            "e, ee from {:?}: {:x?}",
            stream,
            &pong
        );
        initiator
            .read_message(&pong, &mut [])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?; // TODO: allow recovery?
        let noise = initiator
            .into_transport_mode()
            .expect("Noise handshake done");
        trace!("noise handshake done with {:?}", stream);
        Ok(SnowyStream::new(stream, noise))
    }
}

async fn tls12_handshake(
    tlsconn: &mut RustlsClientConnection,
    stream: &mut TcpStream,
    stop_after_server_ccs: bool,
) -> io::Result<()> {
    let mut buf: Vec<MaybeUninit<u8>> =
        Vec::with_capacity(TLS_RECORD_HEADER_LENGTH + MAXIMUM_CIPHERTEXT_LENGTH);
    let mut buf: Vec<u8> = unsafe {
        buf.set_len(buf.capacity());
        mem::transmute(buf)
    };
    let mut seen_ccs = false;
    loop {
        match (tlsconn.wants_read(), tlsconn.wants_write()) {
            (_, true) => {
                // flow: client -> server
                // always prefer to write out over reading in, to avoid deadlock-like waiting
                let len = tlsconn.write_tls(&mut io::Cursor::new(&mut buf)).unwrap();
                // typically, multiple messages are written by a single call
                trace!(
                    first_protocol = u16_from_be_slice(&buf[1..3]),
                    first_msglen = u16_from_be_slice(&buf[3..5]),
                    totallen = len,
                    "tls handshake {} => {}, first type: {:?}",
                    stream.local_addr().unwrap(),
                    stream.peer_addr().unwrap(),
                    TlsContentType::from(buf[0]),
                );
                stream.write_all(&buf[..len]).await?;
            }
            (true, false) => {
                // flow: client <- server
                stream.read_exact(&mut buf[..5]).await?;
                let len = u16_from_be_slice(&buf[3..5]) as usize;
                stream.read_exact(&mut buf[5..5 + len]).await?;
                trace!(
                    protocol = u16_from_be_slice(&buf[1..3]),
                    msglen = u16_from_be_slice(&buf[3..5]),
                    "tls handshake {} <= {}, type: {:?}",
                    stream.local_addr().unwrap(),
                    stream.peer_addr().unwrap(),
                    TlsContentType::from(buf[0]),
                );
                let mut n = tlsconn
                    .read_tls(&mut io::Cursor::new(&mut buf[..5 + len]))
                    .unwrap();
                if n < 5 + len {
                    n += tlsconn
                        .read_tls(&mut io::Cursor::new(&mut buf[n..5 + len]))
                        .unwrap();
                }
                debug_assert_eq!(n, 5 + len);
                tlsconn.process_new_packets().map_err(|e| {
                    debug!(
                        "tls state error when handshaking {} <-> {}: {:?}",
                        stream.local_addr().unwrap(),
                        stream.peer_addr().unwrap(),
                        e
                    );
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("TLS handshake state: {}", e),
                    )
                })?;
                match TlsContentType::from(buf[0]) {
                    TlsContentType::ChangeCipherSpec => {
                        seen_ccs = true;
                        // after server ChangeCipherSpec, the final Handshake Finished message is encrypted
                        // so it can be used to carry other data
                        if stop_after_server_ccs {
                            break;
                        }
                    }
                    _ => {
                        debug_assert_eq!(buf[0], TlsContentType::Handshake.get_u8());
                        // by default, handshake is done after the Handshake Finished message
                        if seen_ccs {
                            break;
                        }
                    }
                }
            }
            (false, false) => break,
        }
    }
    trace!(
        "tls handshake {} <-> {} done",
        stream.local_addr().unwrap(),
        stream.peer_addr().unwrap(),
    );
    Ok(())
}
