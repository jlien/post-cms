"""add_media input tests: bytes in the call over HTTP, type by magic bytes,
a size cap, and stdio paths confined to the uploads dir with no symlinks.

Standard library only, no network: the bucket upload (media.add_media) is
stubbed and records what it would have uploaded.

    python3 -m unittest discover -s mcp -p 'test_*.py'
"""
from __future__ import annotations

import base64
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))
import media  # noqa: E402  (the module tool_add_media imports)

_spec = importlib.util.spec_from_file_location("pcms_server_media_test", HERE / "server.py")
assert _spec is not None and _spec.loader is not None
srv = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(srv)

PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 32
SAMPLES = {
    ".png": PNG,
    ".jpg": b"\xff\xd8\xff\xe0" + b"\x00" * 16,
    ".gif": b"GIF89a" + b"\x00" * 16,
    ".webp": b"RIFF\x00\x00\x00\x00WEBPVP8 ",
    ".wav": b"RIFF\x00\x00\x00\x00WAVEfmt ",
    ".pdf": b"%PDF-1.7\n",
    ".webm": b"\x1a\x45\xdf\xa3" + b"\x00" * 8,
    ".mp4": b"\x00\x00\x00\x18ftypisom" + b"\x00" * 8,
    ".mov": b"\x00\x00\x00\x14ftypqt  " + b"\x00" * 8,
    ".m4a": b"\x00\x00\x00\x1cftypM4A " + b"\x00" * 8,
    ".mp3": b"ID3\x04\x00" + b"\x00" * 8,
}
MP3_FRAMES = (b"\xff\xfb\x90\x64", b"\xff\xf3\x48\xc4", b"\xff\xe3\x18\xc4")
REFUSED = {
    "svg": b'<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>',
    "html": b"<!doctype html><script>alert(1)</script>",
    "text": b"root:x:0:0:root:/root:/bin/sh\n",
    "ssh key": b"-----BEGIN OPENSSH PRIVATE KEY-----\n",
    "utf-16 text": "\ufeffAB".encode("utf-16-le"),
    "utf-16 bom only": b"\xff\xfeA\x00",
    "mpeg layer I": b"\xff\xfe\x90\x00",
    "mpeg bad bitrate": b"\xff\xfb\xf0\x00",
    "mpeg bad rate": b"\xff\xfb\x9c\x00",
    "mpeg reserved version": b"\xff\xeb\x90\x00",
}


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


class MediaUploadTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="pcms-media-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        content = self.tmp / "content"
        (content / "hello").mkdir(parents=True)
        self.uploads = self.tmp / "uploads"
        self.uploads.mkdir()
        self.outside = self.tmp / "outside"
        self.outside.mkdir()
        self.uploaded: list[tuple[str, str, bytes]] = []

        def fake_add_media(slug: str, file_path: str, alt: str = "") -> dict:
            p = Path(file_path)
            self.uploaded.append((slug, p.name, p.read_bytes()))
            return {"slug": slug, "name": p.name}

        env = {k: v for k, v in os.environ.items()
               if k not in ("PCMS_TRANSPORT", "MCP_MEDIA_MAX_BYTES")}
        env["MCP_UPLOADS_DIR"] = str(self.uploads)
        for p in (mock.patch.object(srv, "CONTENT", content),
                  mock.patch.object(media, "add_media", fake_add_media),
                  mock.patch.dict(os.environ, env, clear=True)):
            p.start()
            self.addCleanup(p.stop)

    def http(self) -> None:
        os.environ["PCMS_TRANSPORT"] = "http"
        tok = srv._HTTP_IDENTITY.set({"login": "x", "role": "admin"})
        self.addCleanup(srv._HTTP_IDENTITY.reset, tok)

    def call(self, **args) -> dict:
        return srv.tool_add_media({"slug": "hello", **args})

    # ---- type by content

    def test_every_known_type_is_sniffed(self):
        for ext, data in SAMPLES.items():
            with self.subTest(ext=ext):
                self.assertEqual(media.sniff_ext(data), ext)

    def test_mp3_frame_headers(self):
        for frame in MP3_FRAMES:
            with self.subTest(frame=frame):
                self.assertEqual(media.sniff_ext(frame + b"\x00" * 8), ".mp3")
        for label, data in REFUSED.items():
            with self.subTest(kind=label):
                self.assertIsNone(media.sniff_ext(data))

    def test_type_comes_from_content_not_name(self):
        self.http()
        out = self.call(content_base64=b64(SAMPLES[".pdf"]), filename="photo.png")
        self.assertEqual(out["name"], "photo.pdf")

    def test_unrecognized_content_is_refused(self):
        self.http()
        for label, data in REFUSED.items():
            with self.subTest(kind=label), self.assertRaises(ValueError):
                self.call(content_base64=b64(data), filename="image.png")
        self.assertEqual(self.uploaded, [])

    def test_filename_cannot_steer_the_path(self):
        self.http()
        out = self.call(content_base64=b64(PNG), filename="../../etc/pass wdK.png")
        self.assertEqual(out["name"], "pass-wd.png")
        out = self.call(content_base64=b64(PNG), filename="...")
        self.assertEqual(out["name"], "upload.png")

    # ---- bytes over HTTP

    def test_http_upload_reaches_the_bucket(self):
        self.http()
        self.call(content_base64=b64(PNG), filename="logo.png", alt="Logo")
        self.assertEqual(self.uploaded, [("hello", "logo.png", PNG)])

    def test_http_refuses_file_path(self):
        self.http()
        (self.uploads / "a.png").write_bytes(PNG)
        with self.assertRaisesRegex(ValueError, "not accepted over HTTP"):
            self.call(file_path="a.png")
        self.assertEqual(self.uploaded, [])

    def test_exactly_one_source(self):
        with self.assertRaises(ValueError):
            self.call()
        with self.assertRaises(ValueError):
            self.call(content_base64=b64(PNG), file_path="a.png")

    def test_size_cap(self):
        self.http()
        os.environ["MCP_MEDIA_MAX_BYTES"] = "64"
        self.call(content_base64=b64(PNG))  # 40 bytes: fine
        with self.assertRaisesRegex(ValueError, "limit"):
            self.call(content_base64=b64(PNG + b"\x00" * 40))
        with self.assertRaisesRegex(ValueError, "limit"):
            self.call(content_base64="A" * 1000)  # refused before decoding

    def test_invalid_base64(self):
        self.http()
        for bad in ("not base64!", "", 5, None, "iVBORw0KGgo=\n<script>"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self.call(content_base64=bad)

    # ---- stdio path mode

    def test_stdio_path_inside_uploads_dir(self):
        (self.uploads / "sub").mkdir()
        (self.uploads / "sub" / "pic.jpg").write_bytes(PNG)
        self.call(file_path="sub/pic.jpg")
        self.assertEqual(self.uploaded, [("hello", "pic.png", PNG)])

    def test_stdio_path_outside_uploads_dir_is_refused(self):
        (self.outside / "secret.png").write_bytes(PNG)
        for path in (str(self.outside / "secret.png"), "../outside/secret.png",
                     str(self.uploads), "."):
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.call(file_path=path)
        self.assertEqual(self.uploaded, [])

    def test_stdio_symlinks_are_refused(self):
        (self.outside / "secret.png").write_bytes(PNG)
        os.symlink(self.outside / "secret.png", self.uploads / "link.png")
        os.symlink(self.outside, self.uploads / "dirlink")
        (self.uploads / "real.png").write_bytes(PNG)
        os.symlink(self.uploads / "real.png", self.uploads / "inner.png")
        for path in ("link.png", "dirlink/secret.png", "inner.png"):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "symlink"):
                self.call(file_path=path)
        self.assertEqual(self.uploaded, [])

    def test_stdio_symlink_swapped_in_after_the_check_is_refused(self):
        # The race: the realpath check passes, then a directory becomes a
        # symlink before open. Simulate it by making the check see nothing.
        (self.outside / "secret.png").write_bytes(PNG)
        os.symlink(self.outside, self.uploads / "swap")
        with mock.patch.object(media.os.path, "realpath", os.path.abspath):
            with self.assertRaisesRegex(ValueError, "symlink"):
                self.call(file_path="swap/secret.png")
            with self.assertRaisesRegex(ValueError, "symlink"):
                (self.uploads / "f.png").symlink_to(self.outside / "secret.png")
                self.call(file_path="f.png")
        self.assertEqual(self.uploaded, [])

    def test_stdio_non_regular_file_is_refused(self):
        (self.uploads / "d.png").mkdir()
        with self.assertRaises(ValueError):
            self.call(file_path="d.png")

    def test_stdio_missing_uploads_dir_is_refused(self):
        os.environ["MCP_UPLOADS_DIR"] = str(self.tmp / "nope")
        with self.assertRaisesRegex(ValueError, "uploads dir"):
            self.call(file_path="a.png")

    def test_stdio_size_cap(self):
        os.environ["MCP_MEDIA_MAX_BYTES"] = "16"
        (self.uploads / "big.png").write_bytes(PNG)
        with self.assertRaisesRegex(ValueError, "limit"):
            self.call(file_path="big.png")

    def test_invalid_slug(self):
        for slug in ("../x", "", 5, "Hello"):
            with self.subTest(slug=slug), self.assertRaises(ValueError):
                srv.tool_add_media({"slug": slug, "content_base64": b64(PNG)})


class FfmpegBoundsTest(unittest.TestCase):
    """The real media.add_media with the bucket and subprocess stubbed."""

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="pcms-ff-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.src = self.tmp / "pic.png"
        self.src.write_bytes(PNG)
        self.calls: list[tuple[list[str], dict]] = []
        self.puts: list[str] = []
        self.probe_out = json.dumps({"streams": [{"width": 800, "height": 600}],
                                     "format": {}})
        self.ff_raises: Exception | None = None

        def fake_run(argv, **kw):
            self.calls.append((argv, kw))
            if argv[0] == "ffprobe":
                return subprocess.CompletedProcess(argv, 0, self.probe_out, "")
            if self.ff_raises is not None:
                raise self.ff_raises
            return subprocess.CompletedProcess(argv, 1, "", "")

        def fake_s3(cfg, method, path, payload=b"", content_type=""):
            self.puts.append(path)
            return 200, b""
        for p in (mock.patch.object(media.subprocess, "run", fake_run),
                  mock.patch.object(media, "_s3_request", fake_s3),
                  mock.patch.object(media, "ensure_bucket", lambda cfg: None),
                  mock.patch.object(media, "ensure_public_read", lambda cfg: True),
                  mock.patch.dict(os.environ, {"PATH": "/usr/bin:/bin",
                                               "MEDIA_S3_SECRET": "do-not-pass"})):
            p.start()
            self.addCleanup(p.stop)

    def test_every_ff_run_is_bounded(self):
        media.add_media("hello", str(self.src))
        self.assertGreater(len(self.calls), 1)
        self.assertEqual(self.calls[0][0][0], "ffprobe")
        for argv, kw in self.calls:
            with self.subTest(argv=argv[:2]):
                self.assertEqual(kw.get("timeout"), media.DEFAULT_FF_TIMEOUT)
                self.assertIs(kw.get("stdin"), subprocess.DEVNULL)
                self.assertEqual(set(kw["env"]), {"PATH", "LC_ALL"})
                self.assertNotIn("do-not-pass", json.dumps(kw["env"]))

    def test_pixel_cap_refuses_before_upload_or_decode(self):
        self.probe_out = json.dumps({"streams": [{"width": 100000, "height": 100000}],
                                     "format": {}})
        with self.assertRaisesRegex(ValueError, "pixel limit"):
            media.add_media("hello", str(self.src))
        self.assertEqual([a[0] for a, _ in self.calls], ["ffprobe"])
        self.assertEqual(self.puts, [])

    def test_duration_cap(self):
        self.probe_out = json.dumps({"streams": [{"width": 10, "height": 10}],
                                     "format": {"duration": "99999.0"}})
        with self.assertRaisesRegex(ValueError, "limit"):
            media.add_media("hello", str(self.src))
        self.assertEqual(self.puts, [])

    def test_unprobeable_image_gets_no_ffmpeg(self):
        self.probe_out = "not json"
        media.add_media("hello", str(self.src))
        self.assertEqual([a[0] for a, _ in self.calls], ["ffprobe"])
        self.assertEqual(len(self.puts), 1)  # the original only

    def test_ffmpeg_timeout_is_survived(self):
        self.ff_raises = subprocess.TimeoutExpired("ffmpeg", 1)
        out = media.add_media("hello", str(self.src))
        self.assertEqual(len(out["uploads"]), 1)

    def test_timeout_env_override(self):
        with mock.patch.dict(os.environ, {"MCP_FFMPEG_TIMEOUT": "5"}):
            media.add_media("hello", str(self.src))
        self.assertTrue(all(kw["timeout"] == 5 for _, kw in self.calls))


if __name__ == "__main__":
    unittest.main()
