"""Media pipeline for Post-CMS: content-addressed variants → S3-compatible store.

Design (agreed): binaries never live in git. add_media() hashes a file,
generates resized variants at upload time (ffmpeg), uploads everything to an
S3-compatible bucket (MinIO locally, R2 in prod), and returns markdown/HTML
snippets referencing the CDN URL. Git only ever stores the reference text.

Env:
  MEDIA_S3_ENDPOINT (default http://localhost:9000)
  MEDIA_S3_ACCESS   (default postcms)
  MEDIA_S3_SECRET   (default postcms-secret)
  MEDIA_S3_REGION   (default us-east-1; R2 uses "auto")
  MEDIA_BUCKET      (default postcms-media)
  MEDIA_CDN_BASE    (default http://localhost:9000/postcms-media)
"""

from __future__ import annotations

import hashlib
import hmac
import http.client
import json
import os
import subprocess
import tempfile
import urllib.parse
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

# image widths generated at upload time (original is also uploaded)
VARIANT_WIDTHS = (480, 960, 1600)
IMAGE_EXTS = {".png", ".jpg", ".jpeg"}
VIDEO_EXTS = {".mp4", ".webm", ".mov"}
AUDIO_EXTS = {".mp3", ".wav", ".m4a"}

CONTENT_TYPES = {
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".webp": "image/webp",
    ".gif": "image/gif",
    ".svg": "image/svg+xml",
    ".mp4": "video/mp4",
    ".webm": "video/webm",
    ".mov": "video/quicktime",
    ".mp3": "audio/mpeg",
    ".wav": "audio/wav",
    ".m4a": "audio/mp4",
    ".pdf": "application/pdf",
}


def _endpoint_hostport(url: str) -> str:
    """http://localhost:9000 -> localhost:9000"""
    parsed = urllib.parse.urlparse(url)
    host = parsed.hostname or "localhost"
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    return f"{host}:{port}"


class MediaConfig:
    def __init__(self) -> None:
        self.access = os.environ.get("MEDIA_S3_ACCESS", "postcms")
        self.secret = os.environ.get("MEDIA_S3_SECRET", "postcms-secret")
        self.region = os.environ.get("MEDIA_S3_REGION", "us-east-1")
        self.bucket = os.environ.get("MEDIA_BUCKET", "postcms-media")
        self.endpoint = os.environ.get("MEDIA_S3_ENDPOINT", "http://localhost:9000")
        self.cdn_base = os.environ.get(
            "MEDIA_CDN_BASE", "http://localhost:9000/postcms-media"
        ).rstrip("/")

    @property
    def http_host(self) -> str:
        return _endpoint_hostport(self.endpoint)


# ------------------------------------------------------------------ SigV4


def _sign_key(secret: str, datestamp: str, region: str) -> bytes:
    def _h(key: bytes, msg: str) -> bytes:
        return hmac.new(key, msg.encode(), hashlib.sha256).digest()

    k_date = _h(("AWS4" + secret).encode(), datestamp)
    k_region = hmac.new(k_date, region.encode(), hashlib.sha256).digest()
    k_service = hmac.new(k_region, b"s3", hashlib.sha256).digest()
    return hmac.new(k_service, b"aws4_request", hashlib.sha256).digest()


def _s3_request(cfg: MediaConfig, method: str, path: str, payload: bytes = b"",
                query: str = "", content_type: str = "application/octet-stream"
                ) -> tuple[int, bytes]:
    """One SigV4-signed request. path starts with '/'. Returns (status, body)."""
    t = datetime.now(timezone.utc)
    amz_date = t.strftime("%Y%m%dT%H%M%SZ")
    datestamp = t.strftime("%Y%m%d")
    payload_hash = hashlib.sha256(payload).hexdigest()

    host = cfg.http_host
    canonical_headers = (
        f"content-type:{content_type}\n"
        f"host:{host}\n"
        f"x-amz-content-sha256:{payload_hash}\n"
        f"x-amz-date:{amz_date}\n"
    )
    signed_headers = "content-type;host;x-amz-content-sha256;x-amz-date"
    # Canonical request: method, URI, query string, headers, signed headers, hash.
    canonical_request = (
        f"{method}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    )
    scope = f"{datestamp}/{cfg.region}/s3/aws4_request"
    string_to_sign = (
        f"AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n"
        + hashlib.sha256(canonical_request.encode()).hexdigest()
    )
    signing_key = _sign_key(cfg.secret, datestamp, cfg.region)
    signature = hmac.new(signing_key, string_to_sign.encode(), hashlib.sha256).hexdigest()

    headers = {
        "Authorization": (
            f"AWS4-HMAC-SHA256 Credential={cfg.access}/{scope}, "
            f"SignedHeaders={signed_headers}, Signature={signature}"
        ),
        "x-amz-date": amz_date,
        "x-amz-content-sha256": payload_hash,
        "Content-Type": content_type,
    }

    hostname, port = host.split(":")[0], int(host.split(":")[1])
    conn = http.client.HTTPConnection(hostname, port)
    target = path + (f"?{query}" if query else "")
    conn.request(method, target, body=payload, headers=headers)
    r = conn.getresponse()
    body = r.read()
    conn.close()
    return r.status, body


# ------------------------------------------------------------------ bucket


def ensure_public_read(cfg: MediaConfig) -> bool:
    """Set anonymous GetObject policy (mimics R2 public bucket). Idempotent."""
    policy = {
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Principal": {"AWS": ["*"]},
                "Action": ["s3:GetObject"],
                "Resource": [f"arn:aws:s3:::{cfg.bucket}/*"],
            }
        ],
    }
    status, _ = _s3_request(
        cfg, "PUT", f"/{cfg.bucket}", json.dumps(policy).encode(), query="policy="
    )
    return status in (200, 204)


def ensure_bucket(cfg: MediaConfig) -> None:
    status, _ = _s3_request(cfg, "PUT", f"/{cfg.bucket}")
    if status not in (200, 409):
        raise RuntimeError(f"bucket create failed: HTTP {status}")


# ------------------------------------------------------------------ variants


def _has_webp_encoder() -> bool:
    try:
        res = subprocess.run(
            ["ffmpeg", "-hide_banner", "-encoders"], capture_output=True, text=True
        )
        return "libwebp" in res.stdout
    except FileNotFoundError:
        return False


def _make_resized(src: Path, workdir: Path) -> list[tuple[Path, int]]:
    """Resize variants via ffmpeg. Returns [(path, width)]."""
    variants: list[tuple[Path, int]] = []
    for width in VARIANT_WIDTHS:
        out = workdir / f"{src.stem}-{width}w{src.suffix}"
        res = subprocess.run(
            ["ffmpeg", "-y", "-v", "error", "-i", str(src),
             "-vf", f"scale={width}:-2", str(out)],
            capture_output=True, text=True)
        if res.returncode == 0 and out.exists():
            variants.append((out, width))
    return variants


# ------------------------------------------------------------------ public API


def add_media(slug: str, file_path: str, alt: str = "") -> dict[str, Any]:
    """Upload one file (+resized variants for images) to the media bucket.

    Returns urls, uploads, and ready-to-paste markdown/HTML snippets.
    Nothing is written to git here — the caller embeds the snippet in a post.
    """
    src = Path(file_path).expanduser().resolve()
    if not src.is_file():
        raise ValueError(f"file not found: {src}")
    ext = src.suffix.lower()
    if ext not in CONTENT_TYPES:
        raise ValueError(f"unsupported media type: {ext}")
    if not slug or "/" in slug or ".." in slug:
        raise ValueError(f"invalid slug: {slug!r}")

    cfg = MediaConfig()
    ensure_bucket(cfg)

    digest = hashlib.sha256(src.read_bytes()).hexdigest()[:8]
    stem = src.stem
    base_key = f"{slug}/{stem}.{digest}{ext}"
    cdn = cfg.cdn_base

    uploads: list[dict[str, Any]] = []

    def _upload(key: str, path: Path, ctype: str) -> None:
        status, _ = _s3_request(
            cfg, "PUT", f"/{cfg.bucket}/{key}", path.read_bytes(), content_type=ctype
        )
        if status not in (200, 204):
            raise RuntimeError(f"upload failed for {key}: HTTP {status}")
        uploads.append({"key": key, "url": f"{cdn}/{key}", "content_type": ctype})

    _upload(base_key, src, CONTENT_TYPES[ext])

    if ext in IMAGE_EXTS:
        with tempfile.TemporaryDirectory(prefix="postcms-media-") as tmp:
            workdir = Path(tmp)
            for path, _width in _make_resized(src, workdir):
                vkey = f"{slug}/{stem}.{digest}-{path.name.split('-')[-1]}"
                _upload(vkey, path, CONTENT_TYPES[ext])
            if _has_webp_encoder():
                webp = workdir / f"{stem}.{digest}.webp"
                res = subprocess.run(
                    ["ffmpeg", "-y", "-v", "error", "-i", str(src),
                     "-c:v", "libwebp", "-q:v", "80", str(webp)],
                    capture_output=True, text=True)
                if res.returncode == 0 and webp.exists():
                    _upload(f"{slug}/{stem}.{digest}.webp", webp, "image/webp")

    ensure_public_read(cfg)

    primary = f"{cdn}/{base_key}"
    result: dict[str, Any] = {
        "slug": slug,
        "key": base_key,
        "url": primary,
        "uploads": uploads,
        "alt": alt,
    }

    if ext in IMAGE_EXTS:
        widths = [
            (u, k.rsplit("-", 1)[-1].split(".")[0] + "w")
            for u, k in ((x["url"], x["key"]) for x in uploads)
            if k != base_key
        ]
        widths.sort(key=lambda pair: int(pair[1].rstrip("w")))
        srcset = ", ".join(f"{u} {w}" for u, w in widths)
        result["markdown"] = f"![{alt}]({primary})"
        result["html"] = (
            f'<img src="{primary}" alt="{alt}" '
            f'srcset="{srcset}" sizes="(max-width: 700px) 100vw, 700px">'
        )
    elif ext in VIDEO_EXTS:
        result["html"] = f'<video controls src="{primary}"></video>'
        result["markdown"] = result["html"]
    elif ext in AUDIO_EXTS:
        result["html"] = f'<audio controls src="{primary}"></audio>'
        result["markdown"] = result["html"]
    else:
        result["markdown"] = f"[{alt or stem}]({primary})"
        result["html"] = result["markdown"]

    return result
