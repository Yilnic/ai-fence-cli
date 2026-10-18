"""Local HTTPS fixture with a separate CA and server leaf certificate."""
import ssl
import subprocess
import threading
from http.server import ThreadingHTTPServer


def start_tls(root, handler):
    cert, key = root / "ca.pem", root / "key.pem"
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-subj", "/CN=127.0.0.1", "-addext", "subjectAltName=IP:127.0.0.1",
                    "-keyout", str(key), "-out", str(cert)], capture_output=True, check=True, timeout=15)
    leaf, leaf_key, csr = root / "server.pem", root / "server.key", root / "server.csr"
    extensions = root / "server.ext"
    extensions.write_text("basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=IP:127.0.0.1\n")
    subprocess.run(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=127.0.0.1",
                    "-keyout", str(leaf_key), "-out", str(csr)], capture_output=True, check=True, timeout=15)
    subprocess.run(["openssl", "x509", "-req", "-in", str(csr), "-CA", str(cert), "-CAkey", str(key),
                    "-CAcreateserial", "-days", "1", "-extfile", str(extensions), "-out", str(leaf)],
                   capture_output=True, check=True, timeout=15)
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    server.origin = f"https://127.0.0.1:{server.server_port}"
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(leaf, leaf_key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, cert
