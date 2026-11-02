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
import stat
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


# ffmpeg and ffprobe parse untrusted uploads: every run has a timeout, no
# stdin, and a minimal environment; input is probed (pixels, duration)
# before ffmpeg ever decodes it.
DEFAULT_FF_TIMEOUT = 60
DEFAULT_MAX_PIXELS = 50_000_000
DEFAULT_MAX_SECONDS = 600


def _env_int(name: str, default: int) -> int:
    raw = os.environ.get(name, "").strip()
    return int(raw) if raw.isdigit() and int(raw) > 0 else default


def _ff_env() -> dict[str, str]:
    return {"PATH": os.environ.get("PATH") or "/usr/bin:/bin", "LC_ALL": "C"}


def _run_ff(argv: list[str]) -> subprocess.CompletedProcess | None:
    """Run ffmpeg/ffprobe bounded; None when missing or over the timeout."""
    try:
        return subprocess.run(argv, capture_output=True, text=True,
                              stdin=subprocess.DEVNULL, env=_ff_env(),
                              timeout=_env_int("MCP_FFMPEG_TIMEOUT", DEFAULT_FF_TIMEOUT))
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None


def _probe(src: Path) -> dict[str, float] | None:
    """Width, height and duration of the first video stream, or None when
    ffprobe is missing, times out or cannot read the file."""
    res = _run_ff(["ffprobe", "-v", "error", "-select_streams", "v:0",
                   "-show_entries", "stream=width,height:format=duration",
                   "-of", "json", str(src)])
    if res is None or res.returncode != 0:
        return None
    try:
        info = json.loads(res.stdout or "{}")
        stream = (info.get("streams") or [{}])[0]
        width, height = int(stream["width"]), int(stream["height"])
        raw = (info.get("format") or {}).get("duration")
        duration = float(raw) if raw not in (None, "N/A") else 0.0
    except (ValueError, KeyError, TypeError, IndexError):
        return None
    if width <= 0 or height <= 0:
        return None
    return {"width": width, "height": height, "duration": duration}


def check_probe(probe: dict[str, float]) -> None:
    """Refuse decode bombs before ffmpeg touches them."""
    pixels = probe["width"] * probe["height"]
    cap = _env_int("MCP_MEDIA_MAX_PIXELS", DEFAULT_MAX_PIXELS)
    if pixels > cap:
        raise ValueError(f"image is {int(probe['width'])}x{int(probe['height'])}, over "
                         f"the {cap}-pixel limit (MCP_MEDIA_MAX_PIXELS)")
    secs = _env_int("MCP_MEDIA_MAX_SECONDS", DEFAULT_MAX_SECONDS)
    if probe["duration"] > secs:
        raise ValueError(f"media runs {probe['duration']:.0f}s, over the {secs}s "
                         "limit (MCP_MEDIA_MAX_SECONDS)")


def _has_webp_encoder() -> bool:
    res = _run_ff(["ffmpeg", "-hide_banner", "-encoders"])
    return res is not None and "libwebp" in res.stdout


def _make_resized(src: Path, workdir: Path) -> list[tuple[Path, int]]:
    """Resize variants via ffmpeg. Returns [(path, width)]."""
    variants: list[tuple[Path, int]] = []
    for width in VARIANT_WIDTHS:
        out = workdir / f"{src.stem}-{width}w{src.suffix}"
        res = _run_ff(["ffmpeg", "-nostdin", "-y", "-v", "error", "-i", str(src),
                       "-frames:v", "1", "-vf", f"scale={width}:-2", str(out)])
        if res is not None and res.returncode == 0 and out.exists():
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

    # Probe before any upload or decode; unprobed images get no variants.
    probe = _probe(src) if ext in IMAGE_EXTS else None
    if probe is not None:
        check_probe(probe)

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

    if ext in IMAGE_EXTS and probe is not None:
        with tempfile.TemporaryDirectory(prefix="postcms-media-") as tmp:
            workdir = Path(tmp)
            for path, _width in _make_resized(src, workdir):
                vkey = f"{slug}/{stem}.{digest}-{path.name.split('-')[-1]}"
                _upload(vkey, path, CONTENT_TYPES[ext])
            if _has_webp_encoder():
                webp = workdir / f"{stem}.{digest}.webp"
                res = _run_ff(["ffmpeg", "-nostdin", "-y", "-v", "error", "-i", str(src),
                               "-frames:v", "1", "-c:v", "libwebp", "-q:v", "80", str(webp)])
                if res is not None and res.returncode == 0 and webp.exists():
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


# ------------------------------------------------------- upload input checks
#
# add_media never trusts a caller-supplied extension or a server path:
#   * the file type comes from magic bytes (sniff_ext); SVG and anything
#     without a known signature is refused (SVG can carry script);
#   * over HTTP the bytes arrive in the call (decode_upload), capped;
#   * on stdio a file_path is read only from inside the uploads dir
#     (MCP_UPLOADS_DIR, default <repo>/uploads), never through a symlink.

DEFAULT_MAX_UPLOAD_BYTES = 25 * 1024 * 1024


def max_upload_bytes() -> int:
    raw = os.environ.get("MCP_MEDIA_MAX_BYTES", "").strip()
    if raw.isdigit() and int(raw) > 0:
        return int(raw)
    return DEFAULT_MAX_UPLOAD_BYTES


def sniff_ext(data: bytes) -> str | None:
    """File extension from the leading bytes, or None when unrecognized."""
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return ".png"
    if data.startswith(b"\xff\xd8\xff"):
        return ".jpg"
    if data[:6] in (b"GIF87a", b"GIF89a"):
        return ".gif"
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return ".webp"
    if data[:4] == b"RIFF" and data[8:12] == b"WAVE":
        return ".wav"
    if data.startswith(b"%PDF-"):
        return ".pdf"
    if data[:4] == b"\x1a\x45\xdf\xa3":
        return ".webm"
    if data[4:8] == b"ftyp" and len(data) >= 12:
        brand = data[8:12]
        if brand == b"qt  ":
            return ".mov"
        if brand in (b"M4A ", b"M4B "):
            return ".m4a"
        return ".mp4"
    if data.startswith(b"ID3") or _mpeg_layer3_header(data):
        return ".mp3"
    return None


def _mpeg_layer3_header(data: bytes) -> bool:
    """An MPEG audio Layer III frame header: 11-bit sync, a defined version,
    layer III, a bitrate index that is neither free (0) nor bad (15), and a
    defined sample rate. A UTF-16 BOM (FF FE) is layer I, so it fails."""
    if len(data) < 4 or data[0] != 0xFF or data[1] & 0xE0 != 0xE0:
        return False
    version = (data[1] >> 3) & 0x3
    layer = (data[1] >> 1) & 0x3
    bitrate = data[2] >> 4
    rate = (data[2] >> 2) & 0x3
    return version != 1 and layer == 1 and bitrate not in (0, 15) and rate != 3


def safe_stem(name: Any) -> str:
    stem = Path(name).stem if isinstance(name, str) else ""
    stem = "".join(c if (c.isascii() and (c.isalnum() or c in "-_")) else "-"
                   for c in stem)[:64].strip("-_")
    return stem or "upload"


def decode_upload(content_base64: Any) -> bytes:
    """Strict base64 to bytes, refusing anything over the size cap before
    decoding it."""
    import base64
    import binascii

    if not isinstance(content_base64, str) or not content_base64:
        raise ValueError("content_base64 must be a non-empty base64 string")
    cap = max_upload_bytes()
    if len(content_base64) > 4 * ((cap + 2) // 3):
        raise ValueError(f"upload is over the {cap}-byte limit (MCP_MEDIA_MAX_BYTES)")
    try:
        data = base64.b64decode(content_base64, validate=True)
    except (binascii.Error, ValueError):
        raise ValueError("content_base64 is not valid base64") from None
    if not data:
        raise ValueError("upload is empty")
    if len(data) > cap:
        raise ValueError(f"upload is over the {cap}-byte limit (MCP_MEDIA_MAX_BYTES)")
    return data


def uploads_dir(repo_root: Path) -> Path:
    raw = os.environ.get("MCP_UPLOADS_DIR", "").strip()
    return Path(raw) if raw else repo_root / "uploads"


def read_confined(file_path: Any, root_dir: Path) -> bytes:
    """Bytes of a regular file inside root_dir. Refuses paths outside it,
    any symlink on the way (realpath must equal the plain absolute path),
    and files over the size cap."""
    if not isinstance(file_path, str) or not file_path:
        raise ValueError("file_path must be a non-empty string")
    root = os.path.realpath(root_dir)
    if not os.path.isdir(root):
        raise ValueError(f"uploads dir not found: {root_dir} (set MCP_UPLOADS_DIR)")
    path = os.path.abspath(file_path if os.path.isabs(file_path)
                           else os.path.join(root, file_path))
    if path == root or os.path.commonpath([root, path]) != root:
        raise ValueError(f"file_path must be inside the uploads dir {root}")
    if os.path.realpath(path) != path:
        raise ValueError("file_path may not be or pass through a symlink")
    fd = _open_beneath(root, os.path.relpath(path, root))
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise ValueError("file_path is not a regular file")
    with os.fdopen(fd, "rb") as fh:
        cap = max_upload_bytes()
        data = fh.read(cap + 1)
    if len(data) > cap:
        raise ValueError(f"file is over the {cap}-byte limit (MCP_MEDIA_MAX_BYTES)")
    if not data:
        raise ValueError("file is empty")
    return data


def _open_beneath(root: str, rel: str) -> int:
    """Open root/rel read-only without following a symlink anywhere below
    root: each directory is opened relative to its parent's fd with
    O_NOFOLLOW | O_DIRECTORY, then the file relative to the last one. A
    directory swapped for a symlink mid-walk fails instead of escaping."""
    parts = rel.split(os.sep)
    if not parts or any(p in ("", ".", "..") for p in parts):
        raise ValueError("file_path must be inside the uploads dir")
    nofollow = os.O_NOFOLLOW
    cur = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in parts[:-1]:
            try:
                nxt = os.open(part, os.O_RDONLY | os.O_DIRECTORY | nofollow, dir_fd=cur)
            except OSError as exc:
                raise _open_error(exc) from None
            os.close(cur)
            cur = nxt
        try:
            return os.open(parts[-1], os.O_RDONLY | nofollow | os.O_NONBLOCK, dir_fd=cur)
        except OSError as exc:
            raise _open_error(exc) from None
    finally:
        os.close(cur)


def _open_error(exc: OSError) -> ValueError:
    import errno

    if exc.errno in (errno.ELOOP, errno.ENOTDIR, errno.EMLINK):
        return ValueError("file_path may not be or pass through a symlink")
    return ValueError(f"cannot open file_path: {exc.strerror}")


def add_media_bytes(slug: str, data: bytes, name: Any, alt: str = "") -> dict[str, Any]:
    """Type-check bytes by magic and upload them through add_media from a
    private temp copy named <safe stem><sniffed extension>."""
    ext = sniff_ext(data)
    if ext is None:
        raise ValueError("unsupported media type: the content is not a recognized "
                         "image, video, audio or PDF file (checked by content, not name)")
    with tempfile.TemporaryDirectory(prefix="postcms-upload-") as tmp:
        path = Path(tmp) / f"{safe_stem(name)}{ext}"
        path.write_bytes(data)
        return add_media(slug, str(path), alt)
