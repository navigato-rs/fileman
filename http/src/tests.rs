use super::*;
use std::{
    io::{Read as _, Write as _},
    net, thread,
};

const ROOT: &[u8] = include_bytes!("../tests/fixtures/root.der");
const INTERMEDIATE: &[u8] = include_bytes!("../tests/fixtures/intermediate.der");
const LEAF: &[u8] = include_bytes!("../tests/fixtures/leaf.der");
const KEY: &[u8] = include_bytes!("../tests/fixtures/leaf-key.der");
const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

fn client(bytes: u64) -> Client {
    Client::with_roots(
        vec![ROOT.to_vec().into()],
        Limits {
            timeout: time::Duration::from_secs(3),
            response_bytes: bytes,
        },
    )
    .unwrap()
}

fn serve(
    chain: Vec<Vec<u8>>,
    key: &[u8],
    response: Vec<u8>,
    suite: usize,
    group: usize,
) -> (String, thread::JoinHandle<Vec<u8>>) {
    let mut crypto = provider::provider();
    crypto.cipher_suites = vec![crypto.cipher_suites[suite]];
    crypto.kx_groups = vec![crypto.kx_groups[group]];
    // Server-only fixture signing. Public client API cannot load private keys.
    crypto.key_provider = rustls_rustcrypto::provider().key_provider;
    let config = rustls::ServerConfig::builder_with_provider(sync::Arc::new(crypto))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            chain.into_iter().map(Into::into).collect(),
            pki_types::PrivatePkcs8KeyDer::from(key.to_vec()).into(),
        )
        .unwrap();
    let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "https://localhost:{}/",
        listener.local_addr().unwrap().port()
    );
    let handle = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(time::Duration::from_secs(3)))
            .unwrap();
        socket
            .set_write_timeout(Some(time::Duration::from_secs(3)))
            .unwrap();
        let mut stream = rustls::StreamOwned::new(
            rustls::ServerConnection::new(sync::Arc::new(config)).unwrap(),
            socket,
        );
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return request,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
            assert!(request.len() < 128 * 1024);
            if let Some(end) = request.windows(4).position(|p| p == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length: usize = header
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap_or("0")
                    .trim()
                    .parse()
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        stream.conn.send_close_notify();
        let _ = stream.flush();
        request
    });
    (url, handle)
}
fn normal(response: &[u8]) -> (String, thread::JoinHandle<Vec<u8>>) {
    serve(
        vec![LEAF.to_vec(), INTERMEDIATE.to_vec()],
        KEY,
        response.to_vec(),
        0,
        0,
    )
}

#[test]
fn all_suites_and_both_key_exchanges_complete_https() {
    for suite in 0..3 {
        for group in 0..2 {
            let (url, server) = serve(
                vec![LEAF.to_vec(), INTERMEDIATE.to_vec()],
                KEY,
                OK.to_vec(),
                suite,
                group,
            );
            let response = client(2).get(&url).unwrap();
            assert_eq!(response.status, 200);
            assert_eq!(response.body, b"ok");
            assert!(server.join().unwrap().starts_with(b"GET / HTTP/1.1\r\n"));
        }
    }
}
#[test]
fn verifies_rsa_chain_and_handshake_signature() {
    let (url, server) = serve(
        vec![include_bytes!("../tests/fixtures/rsa-leaf.der").to_vec()],
        include_bytes!("../tests/fixtures/rsa-key.der"),
        OK.to_vec(),
        0,
        0,
    );
    let client = Client::with_roots(
        vec![
            include_bytes!("../tests/fixtures/rsa-root.der")
                .to_vec()
                .into(),
        ],
        Limits::default(),
    )
    .unwrap();
    assert_eq!(client.get(&url).unwrap().body, b"ok");
    assert!(!server.join().unwrap().is_empty());
}
#[test]
fn rejects_bad_certificates_before_sending_http() {
    let mut signature = LEAF.to_vec();
    *signature.last_mut().unwrap() ^= 1;
    let cases = [
        (
            include_bytes!("../tests/fixtures/expired.der").to_vec(),
            INTERMEDIATE.to_vec(),
            "Expired",
        ),
        (
            include_bytes!("../tests/fixtures/future.der").to_vec(),
            INTERMEDIATE.to_vec(),
            "NotValidYet",
        ),
        (
            include_bytes!("../tests/fixtures/wrong-name.der").to_vec(),
            INTERMEDIATE.to_vec(),
            "NotValidForName",
        ),
        (
            include_bytes!("../tests/fixtures/client-only.der").to_vec(),
            INTERMEDIATE.to_vec(),
            "InvalidPurpose",
        ),
        (
            LEAF.to_vec(),
            include_bytes!("../tests/fixtures/not-a-ca.der").to_vec(),
            "Other",
        ),
        (signature, INTERMEDIATE.to_vec(), "BadSignature"),
    ];
    for (leaf, intermediate, expected) in cases {
        let (url, server) = serve(vec![leaf, intermediate], KEY, OK.to_vec(), 0, 0);
        let error = client(1024)
            .get(&url)
            .err()
            .expect("invalid certificate accepted");
        let detail = format!("{error:?}");
        assert!(detail.contains("InvalidCertificate"), "{detail}");
        assert!(detail.contains(expected), "wanted {expected}: {detail}");
        assert!(
            server.join().unwrap().is_empty(),
            "HTTP escaped failed verification"
        );
    }
}
#[test]
fn rejects_missing_intermediate_and_untrusted_root() {
    for chain in [
        vec![LEAF.to_vec()],
        vec![LEAF.to_vec(), INTERMEDIATE.to_vec()],
    ] {
        let (url, server) = serve(chain.clone(), KEY, OK.to_vec(), 0, 0);
        let client = if chain.len() == 1 {
            client(1024)
        } else {
            Client::with_roots(
                vec![
                    include_bytes!("../tests/fixtures/rsa-root.der")
                        .to_vec()
                        .into(),
                ],
                Limits::default(),
            )
            .unwrap()
        };
        let error = client.get(&url).err().expect("untrusted chain accepted");
        assert!(format!("{error:?}").contains("UnknownIssuer"), "{error:?}");
        assert!(server.join().unwrap().is_empty());
    }
}
#[test]
fn rejects_invalid_inputs_without_network() {
    let client = client(1024);
    for url in [
        "http://localhost/",
        "https://user:password@localhost/",
        "https://localhost/#fragment",
        "https://localhost/\r\ninjected",
        "relative",
        "https:///missing",
    ] {
        assert!(matches!(client.get(url), Err(Error::InvalidUrl)), "{url}");
    }
    assert!(matches!(
        Client::with_roots(vec![], Limits::default()),
        Err(Error::TrustStore)
    ));
    assert!(matches!(
        Client::with_roots(vec![vec![1, 2, 3].into()], Limits::default()),
        Err(Error::TrustStore)
    ));
    for (timeout, response_bytes) in [
        (time::Duration::ZERO, 1),
        (time::Duration::from_secs(61), 1),
        (time::Duration::from_secs(1), 0),
        (time::Duration::from_secs(1), 9 * 1024 * 1024),
    ] {
        assert!(matches!(
            Client::with_roots(
                vec![ROOT.to_vec().into()],
                Limits {
                    timeout,
                    response_bytes
                }
            ),
            Err(Error::InvalidLimits)
        ));
    }
    assert!(matches!(
        client.post(
            "https://localhost/",
            "application/octet-stream",
            &vec![0; 65537]
        ),
        Err(Error::RequestTooLarge)
    ));
}
#[test]
fn posts_binary_once_and_preserves_http_failure() {
    let reply = b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (url, server) = normal(reply);
    let data: Vec<_> = (0..65536).map(|i| i as u8).collect();
    assert_eq!(
        client(1024)
            .post(&url, "application/octet-stream", &data)
            .unwrap()
            .status,
        429
    );
    let request = server.join().unwrap();
    assert!(request.starts_with(b"POST / HTTP/1.1\r\n"));
    let end = request.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
    assert_eq!(&request[end..], &data);
}
#[test]
fn refuses_redirects_and_oversized_responses() {
    let (url, server) =
        normal(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n");
    assert!(matches!(client(1024).get(&url), Err(Error::Redirect)));
    server.join().unwrap();
    for response in [
        &b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"[..],
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"[..],
    ] {
        let (url, server) = normal(response);
        assert!(matches!(client(4).get(&url), Err(Error::ResponseTooLarge)));
        server.join().unwrap();
    }
}
#[test]
fn stalled_handshake_has_a_deadline() {
    let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "https://127.0.0.1:{}/",
        listener.local_addr().unwrap().port()
    );
    let server = thread::spawn(move || {
        let (_socket, _) = listener.accept().unwrap();
        thread::sleep(time::Duration::from_millis(500));
    });
    let client = Client::with_roots(
        vec![ROOT.to_vec().into()],
        Limits {
            timeout: time::Duration::from_millis(100),
            response_bytes: 1024,
        },
    )
    .unwrap();
    let start = time::Instant::now();
    let error = client.get(&url).err().expect("stalled handshake succeeded");
    assert!(
        matches!(
            error,
            Error::Transport(ureq::Error::Timeout(_)) | Error::Transport(ureq::Error::Io(_))
        ),
        "{error:?}"
    );
    assert!(start.elapsed() < time::Duration::from_secs(3));
    server.join().unwrap();
}

#[test]
fn altered_records_fail_authentication_in_every_suite() {
    fn transfer(from: &mut rustls::Connection, to: &mut rustls::Connection) {
        let mut bytes = Vec::new();
        while from.wants_write() {
            from.write_tls(&mut bytes).unwrap();
        }
        if !bytes.is_empty() {
            to.read_tls(&mut &bytes[..]).unwrap();
            to.process_new_packets().unwrap();
        }
    }
    for suite in 0..3 {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ROOT.to_vec().into()).unwrap();
        let client =
            rustls::ClientConfig::builder_with_provider(sync::Arc::new(provider::provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        let mut crypto = provider::provider();
        crypto.cipher_suites = vec![crypto.cipher_suites[suite]];
        crypto.key_provider = rustls_rustcrypto::provider().key_provider;
        let server = rustls::ServerConfig::builder_with_provider(sync::Arc::new(crypto))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![LEAF.to_vec().into(), INTERMEDIATE.to_vec().into()],
                pki_types::PrivatePkcs8KeyDer::from(KEY.to_vec()).into(),
            )
            .unwrap();
        let mut client: rustls::Connection =
            rustls::ClientConnection::new(sync::Arc::new(client), "localhost".try_into().unwrap())
                .unwrap()
                .into();
        let mut server: rustls::Connection = rustls::ServerConnection::new(sync::Arc::new(server))
            .unwrap()
            .into();
        for _ in 0..8 {
            transfer(&mut client, &mut server);
            transfer(&mut server, &mut client);
        }
        assert!(!client.is_handshaking() && !server.is_handshaking());
        server.writer().write_all(b"authenticated body").unwrap();
        let mut bytes = Vec::new();
        server.write_tls(&mut bytes).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        client.read_tls(&mut &bytes[..]).unwrap();
        assert!(matches!(
            client.process_new_packets(),
            Err(rustls::Error::DecryptError)
        ));
    }
}

#[test]
fn oversized_headers_and_truncated_bodies_are_errors() {
    let large = format!(
        "HTTP/1.1 200 OK\r\nX-Large: {}\r\nContent-Length: 0\r\n\r\n",
        "x".repeat(17 * 1024)
    );
    for response in [
        large.as_bytes(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nx",
    ] {
        let (url, server) = normal(response);
        assert!(client(1024).get(&url).is_err());
        server.join().unwrap();
    }
}
