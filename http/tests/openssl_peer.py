"""Test the Rust HTTPS probe against an independent, loopback-only TLS peer.

Python's SSL server is a test oracle, not a dependency of either application.
Uses only public test identities; no credentials, profiles or external requests.
"""
import argparse
import base64
import os
from pathlib import Path
import socket
import ssl
import subprocess
import tempfile
import threading

FIXTURES = Path(__file__).resolve().parent / 'fixtures'


def pem(label, data):
    encoded = base64.b64encode(data)
    return (f'-----BEGIN {label}-----\n'.encode()
            + b'\n'.join(encoded[i:i+64] for i in range(0, len(encoded), 64))
            + f'\n-----END {label}-----\n'.encode())


def check(binary, name, chain, key, root, group, valid):
    with tempfile.TemporaryDirectory(prefix='navigato-tls-') as directory:
        directory = Path(directory)
        certfile, keyfile, cafile = [directory / p for p in ('chain.pem', 'key.pem', 'root.pem')]
        certfile.write_bytes(b''.join(pem('CERTIFICATE', cert) for cert in chain))
        keyfile.write_bytes(pem('PRIVATE KEY', key))
        cafile.write_bytes(pem('CERTIFICATE', root))
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.minimum_version = ssl.TLSVersion.TLSv1_3
        context.maximum_version = ssl.TLSVersion.TLSv1_3
        context.load_cert_chain(certfile, keyfile)
        context.set_ecdh_curve(group)
        errors, received, versions = [], [], []
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen(1)
            listener.settimeout(10)
            port = listener.getsockname()[1]

            def serve():
                try:
                    raw, _ = listener.accept()
                    raw.settimeout(5)
                    with raw, context.wrap_socket(raw, server_side=True) as conn:
                        versions.append(conn.version())
                        request = conn.recv(8192)
                        received.append(request)
                        body = b'independent TLS peer\n'
                        conn.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: '
                                     + str(len(body)).encode() + b'\r\nConnection: close\r\n\r\n' + body)
                except (OSError, ssl.SSLError) as error:
                    errors.append(str(error))

            worker = threading.Thread(target=serve, daemon=True)
            worker.start()
            env = os.environ.copy()
            env['SSL_CERT_FILE'] = str(cafile)
            env.pop('SSL_CERT_DIR', None)
            result = subprocess.run([str(binary), f'https://127.0.0.1:{port}/'], env=env,
                                    text=True, capture_output=True, timeout=12)
            worker.join(timeout=11)
            assert not worker.is_alive(), f'{name}: TLS peer did not stop'
            if valid:
                assert result.returncode == 0, (name, result.stderr, errors)
                assert 'HTTP 200; 21 bytes' in result.stdout, (name, result.stdout)
                assert received and received[0].startswith(b'GET / HTTP/1.1\r\n'), name
                assert versions == ['TLSv1.3'], name
            else:
                assert result.returncode != 0, f'{name}: invalid peer accepted'
                assert not received, f'{name}: HTTP sent before trust was established'
        print(f'{name}: passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    fixture = lambda name: (FIXTURES / f'{name}.der').read_bytes()
    intermediate, key, root = map(fixture, ('intermediate', 'leaf-key', 'root'))
    for group in ('X25519', 'prime256v1'):
        check(binary, 'ECDSA/' + group, [fixture('leaf'), intermediate], key, root, group, True)
        check(binary, 'RSA/' + group, [fixture('rsa-leaf')], fixture('rsa-key'), fixture('rsa-root'), group, True)
    for name in ('expired', 'future', 'wrong-name', 'client-only'):
        check(binary, name, [fixture(name), intermediate], key, root, 'X25519', False)
    corrupted = bytearray(fixture('leaf'))
    corrupted[-1] ^= 1
    check(binary, 'signature', [bytes(corrupted), intermediate], key, root, 'X25519', False)
    check(binary, 'missing intermediate', [fixture('leaf')], key, root, 'X25519', False)
    check(binary, 'untrusted root', [fixture('leaf'), intermediate], key, fixture('rsa-root'), 'X25519', False)
    print(ssl.OPENSSL_VERSION + ': all 11 loopback cases passed')


if __name__ == '__main__':
    main()
