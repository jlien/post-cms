"""Probe: verify SigV4 PUT/GET against local MinIO (S3-compatible = R2 path).

Same signing code shape the MCP add_media tool will use against real R2.
"""
import hashlib
import hmac
import http.client
import json
import sys
from datetime import datetime, timezone

HOST = "localhost:9000"
ACCESS = "postcms"
SECRET = "postcms-secret"
REGION = "us-east-1"  # MinIO default; R2 uses 'auto'


def sign(method, path, payload=b""):
    t = datetime.now(timezone.utc)
    amz_date = t.strftime("%Y%m%dT%H%M%SZ")
    datestamp = t.strftime("%Y%m%d")
    payload_hash = hashlib.sha256(payload).hexdigest()

    canonical_headers = (
        f"host:{HOST}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n"
    )
    signed_headers = "host;x-amz-content-sha256;x-amz-date"
    canonical_request = (
        f"{method}\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    )
    scope = f"{datestamp}/{REGION}/s3/aws4_request"
    string_to_sign = (
        f"AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n"
        + hashlib.sha256(canonical_request.encode()).hexdigest()
    )

    def _key(key, msg):
        return hmac.new(key, msg.encode(), hashlib.sha256).digest()

    k_date = _key(("AWS4" + SECRET).encode(), datestamp)
    k_region = hmac.new(k_date, REGION.encode(), hashlib.sha256).digest()
    k_service = hmac.new(k_region, b"s3", hashlib.sha256).digest()
    k_signing = hmac.new(k_service, b"aws4_request", hashlib.sha256).digest()
    signature = hmac.new(k_signing, string_to_sign.encode(), hashlib.sha256).hexdigest()

    return (
        f"AWS4-HMAC-SHA256 Credential={ACCESS}/{scope}, "
        f"SignedHeaders={signed_headers}, Signature={signature}",
        amz_date,
        payload_hash,
    )


def s3_request(method, path, payload=b"", headers_extra=None):
    auth, amz_date, payload_hash = sign(method, path, payload)
    headers = {
        "Authorization": auth,
        "x-amz-date": amz_date,
        "x-amz-content-sha256": payload_hash,
        "host": HOST,
    }
    if headers_extra:
        headers.update(headers_extra)
    conn = http.client.HTTPConnection(HOST.split(":")[0], int(HOST.split(":")[1]))
    conn.request(method, path, body=payload, headers=headers)
    r = conn.getresponse()
    body = r.read()
    conn.close()
    return r.status, body


results = {}

# 1. Create bucket
status, _ = s3_request("PUT", "/postcms-media")
results["create_bucket"] = status  # 200 or 409 (already exists) both fine

# 2. PUT object
content = b"fake-png-bytes-0123456789"
status, _ = s3_request("PUT", "/postcms-media/diagram.a3f9c2.png", content)
results["put_object"] = status

# 3. GET object back
status, body = s3_request("GET", "/postcms-media/diagram.a3f9c2.png")
results["get_object"] = status
results["roundtrip_ok"] = body == content

# 4. Head bucket
status, _ = s3_request("HEAD", "/postcms-media")
results["head_bucket"] = status

print(json.dumps(results, indent=2))
sys.exit(0 if all(results[k] in (200, 409) for k in
                  ("create_bucket", "put_object", "get_object", "head_bucket"))
         and results["roundtrip_ok"] else 1)
