//! Narrow client-only adapter around the pinned, experimental upstream provider.
//! No algorithm implementations are copied here. Remove guards once fixed upstream.
use rustls::{self, crypto};
use std::sync;
use zeroize::Zeroizing;

pub(crate) fn provider() -> crypto::CryptoProvider {
    let mut provider = rustls_rustcrypto::provider();
    provider.cipher_suites = SUITES
        .iter()
        .map(rustls::SupportedCipherSuite::Tls13)
        .collect();
    provider.kx_groups = vec![&X25519, &P256];
    provider.key_provider = &NoPrivateKeys;
    provider
}

// Upstream uses u64::MAX for AES-GCM. Match Rustls's documented 2^24-record
// bound; Rustls remains responsible for key updates and record accounting.
static SUITES: sync::LazyLock<[rustls::Tls13CipherSuite; 3]> = sync::LazyLock::new(|| {
    [
        bounded_suite(rustls_rustcrypto::TLS13_AES_128_GCM_SHA256, 1 << 24),
        bounded_suite(rustls_rustcrypto::TLS13_CHACHA20_POLY1305_SHA256, u64::MAX),
        bounded_suite(rustls_rustcrypto::TLS13_AES_256_GCM_SHA384, 1 << 24),
    ]
});

fn bounded_suite(suite: rustls::SupportedCipherSuite, limit: u64) -> rustls::Tls13CipherSuite {
    let suite = suite.tls13().expect("pinned TLS 1.3 suite");
    rustls::Tls13CipherSuite {
        common: crypto::CipherSuiteCommon {
            suite: suite.common.suite,
            hash_provider: suite.common.hash_provider,
            confidentiality_limit: limit,
        },
        hkdf_provider: suite.hkdf_provider,
        aead_alg: suite.aead_alg,
        quic: None,
    }
}

#[derive(Debug)]
struct NoPrivateKeys;
impl crypto::KeyProvider for NoPrivateKeys {
    fn load_private_key(
        &self,
        _: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> Result<sync::Arc<dyn rustls::sign::SigningKey>, rustls::Error> {
        Err(rustls::Error::General(
            "client certificates are not supported".into(),
        ))
    }
}

#[derive(Debug)]
struct X25519;
impl crypto::SupportedKxGroup for X25519 {
    fn name(&self) -> rustls::NamedGroup {
        rustls::NamedGroup::X25519
    }
    fn start(&self) -> Result<Box<dyn crypto::ActiveKeyExchange>, rustls::Error> {
        let mut bytes = Zeroizing::new([0; 32]);
        getrandom::fill(&mut *bytes).map_err(|_| rustls::Error::FailedToGetRandomBytes)?;
        // StaticSecret gives fallible RNG setup; this adapter still consumes each
        // secret exactly once and zeroizes it on drop. Never reused as a static key.
        let secret = x25519_dalek::StaticSecret::from(*bytes);
        let public = x25519_dalek::PublicKey::from(&secret);
        Ok(Box::new(X25519Exchange { secret, public }))
    }
}
struct X25519Exchange {
    secret: x25519_dalek::StaticSecret,
    public: x25519_dalek::PublicKey,
}
impl crypto::ActiveKeyExchange for X25519Exchange {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<crypto::SharedSecret, rustls::Error> {
        let peer: [u8; 32] = peer.try_into().map_err(|_| invalid_share())?;
        let secret = self.secret.diffie_hellman(&peer.into());
        if !secret.was_contributory() {
            return Err(invalid_share());
        }
        Ok(secret.as_bytes().as_slice().into())
    }
    fn pub_key(&self) -> &[u8] {
        self.public.as_bytes()
    }
    fn group(&self) -> rustls::NamedGroup {
        rustls::NamedGroup::X25519
    }
}

#[derive(Debug)]
struct P256;
impl crypto::SupportedKxGroup for P256 {
    fn name(&self) -> rustls::NamedGroup {
        rustls::NamedGroup::secp256r1
    }
    fn start(&self) -> Result<Box<dyn crypto::ActiveKeyExchange>, rustls::Error> {
        use p256::elliptic_curve::Generate as _;
        let secret = p256::ecdh::EphemeralSecret::try_generate_from_rng(&mut getrandom::SysRng)
            .map_err(|_| rustls::Error::FailedToGetRandomBytes)?;
        let public = p256::PublicKey::from(&secret).to_sec1_bytes();
        Ok(Box::new(P256Exchange { secret, public }))
    }
}
struct P256Exchange {
    secret: p256::ecdh::EphemeralSecret,
    public: Box<[u8]>,
}
impl crypto::ActiveKeyExchange for P256Exchange {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<crypto::SharedSecret, rustls::Error> {
        let public = p256::PublicKey::from_sec1_bytes(peer).map_err(|_| invalid_share())?;
        Ok(self
            .secret
            .diffie_hellman(&public)
            .raw_secret_bytes()
            .as_slice()
            .into())
    }
    fn pub_key(&self) -> &[u8] {
        &self.public
    }
    fn group(&self) -> rustls::NamedGroup {
        rustls::NamedGroup::secp256r1
    }
}
fn invalid_share() -> rustls::Error {
    rustls::PeerMisbehaved::InvalidKeyShare.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::SupportedKxGroup as _;

    #[test]
    fn rejects_low_order_and_malformed_x25519_shares() {
        let mut one = [0; 32];
        one[0] = 1;
        let mut p = [0xff; 32];
        p[0] = 0xed;
        p[31] = 0x7f;
        let mut p_minus_one = p;
        p_minus_one[0] -= 1;
        let mut p_plus_one = p;
        p_plus_one[0] += 1;
        for peer in [[0; 32], one, p_minus_one, p, p_plus_one] {
            assert!(X25519.start().unwrap().complete(&peer).is_err());
        }
        for peer in [&[][..], &[9; 31], &[9; 33]] {
            assert!(X25519.start().unwrap().complete(peer).is_err());
        }
    }
    #[test]
    fn rejects_invalid_p256_points() {
        for peer in [&[][..], &[0; 65], &[4; 65], &[2; 32]] {
            assert!(P256.start().unwrap().complete(peer).is_err());
        }
    }
    #[test]
    fn exchanges_agree_and_aes_has_finite_limits() {
        let provider = provider();
        for group in provider.kx_groups {
            let a = group.start().unwrap();
            let b = group.start().unwrap();
            let a_pub = a.pub_key().to_vec();
            let b_pub = b.pub_key().to_vec();
            assert_eq!(
                a.complete(&b_pub).unwrap().secret_bytes(),
                b.complete(&a_pub).unwrap().secret_bytes()
            );
        }
        for suite in provider.cipher_suites {
            let suite = suite.tls13().unwrap();
            let expected =
                if suite.common.suite == rustls::CipherSuite::TLS13_CHACHA20_POLY1305_SHA256 {
                    u64::MAX
                } else {
                    1 << 24
                };
            assert_eq!(suite.common.confidentiality_limit, expected);
            assert!(suite.quic.is_none());
        }
        assert!(
            provider
                .key_provider
                .load_private_key(rustls::pki_types::PrivatePkcs8KeyDer::from(vec![]).into())
                .is_err()
        );
    }
}
