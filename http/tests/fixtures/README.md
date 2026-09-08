# TLS test identities

Public, test-only DER certificates and PKCS#8 keys. Never use these keys outside
loopback fixtures. ECDSA P-256 root/intermediate/leaf and RSA-2048 root/leaf,
SHA-256 signatures, localhost/127.0.0.1 subject alternative names. Normal validity
is 2020–2100; expired and future fixtures use 2020–2021 and 2050–2100.
`not-a-ca` uses the intermediate public key without CA permission. `client-only`
has clientAuth instead of serverAuth. Wrong-name certificates name wrong.invalid.
Tests mutate signatures in memory; no runtime certificate generator or C crypto
is a dependency of the library or Rust tests.
