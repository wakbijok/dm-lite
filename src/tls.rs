//! The TLS crypto provider behind every rustls connection dmem accepts or makes: `dmem serve`,
//! the remote client, `dmem upgrade` and `dmem migrate --url`. It is aws-lc-rs, the one rustls
//! provider with ML-KEM: key exchange offers the X25519MLKEM768 hybrid (ML-KEM-768 combined
//! with X25519) first, then X25519, P-256 and P-384 for a peer without it. Certificates and
//! their signatures stay classical.

/// Install aws-lc-rs as the process-wide rustls provider. rustls 0.23 needs one before any TLS
/// work and cannot pick one itself when two are compiled in (other crates pull ring), and
/// reqwest's no-provider rustls panics without one. Idempotent: an installed provider is kept.
pub fn install_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// A blocking HTTP client builder on rustls with that provider. A `dist` build also links the
/// platform TLS library (hf-hub turns on reqwest's default-tls), which reqwest would otherwise
/// pick: OpenSSL 3.0 on Ubuntu 22.04/24.04, for one, has no ML-KEM. HTTP/1.1 only: hf-hub also
/// turns on reqwest's HTTP/2, which rustls would offer first (`dmem serve` takes it), while 0.3.7
/// and every build without the embedding model speak HTTP/1.1.
#[cfg(any(feature = "client", feature = "self-update"))]
pub fn http_builder() -> reqwest::blocking::ClientBuilder {
    install_provider();
    reqwest::blocking::Client::builder().use_rustls_tls().http1_only()
}

#[cfg(test)]
pub(crate) mod tests {
    use rustls::NamedGroup;

    /// The key exchange groups a TLS client offers, in its order of preference: `connect` gets
    /// an https URL on a local listener that reads the ClientHello and hangs up.
    pub(crate) fn offered_groups(connect: impl FnOnce(String) + Send + 'static) -> Vec<NamedGroup> {
        client_hello(connect, |h| h.named_groups().unwrap_or_default().to_vec())
    }

    /// What `read` takes from the ClientHello a TLS client sends to a local listener (see above).
    fn client_hello<T>(connect: impl FnOnce(String) + Send + 'static, read: impl FnOnce(&rustls::server::ClientHello) -> T) -> T {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let client = std::thread::spawn(move || connect(url));
        let (mut sock, _) = listener.accept().unwrap();
        let mut acceptor = rustls::server::Acceptor::default();
        let accepted = loop {
            assert!(acceptor.read_tls(&mut sock).unwrap() > 0, "closed before its ClientHello");
            if let Some(a) = acceptor.accept().map_err(|(e, _)| e).unwrap() {
                break a;
            }
        };
        let out = read(&accepted.client_hello());
        drop(sock);
        client.join().unwrap();
        out
    }

    // Every TLS path installs this provider: the post-quantum hybrid first, then the classical
    // groups for a peer without ML-KEM.
    #[test]
    fn provider_offers_the_pq_hybrid_first() {
        super::install_provider();
        let provider = rustls::crypto::CryptoProvider::get_default().expect("a provider is installed");
        let groups: Vec<NamedGroup> = provider.kx_groups.iter().map(|g| g.name()).collect();
        assert_eq!(groups, [NamedGroup::X25519MLKEM768, NamedGroup::X25519, NamedGroup::secp256r1, NamedGroup::secp384r1]);
    }

    // The HTTP clients ask for HTTP/1.1 alone, as a build without HTTP/2 does, so a `dist` client
    // does not move to h2 against `dmem serve`, GitHub or a proxy.
    #[cfg(any(feature = "client", feature = "self-update"))]
    #[test]
    fn http_clients_ask_for_http1_only() {
        let alpn = client_hello(
            |url| {
                let _ = super::http_builder().build().unwrap().get(url).send();
            },
            |h| h.alpn().map(|protos| protos.map(<[u8]>::to_vec).collect::<Vec<_>>()),
        );
        assert_eq!(alpn, Some(vec![b"http/1.1".to_vec()]));
    }
}
