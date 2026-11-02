"""Forge tests: the GitLab backend against a stub GitLab HTTP server on
127.0.0.1, the GitHub backend's gh command lines, and the MCP tools in
FORGE=gitlab mode without any gh call.

Standard library only, no network beyond the loopback stub.

    python3 -m unittest discover -s mcp -p 'test_*.py'
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import logging
import os
import subprocess
import sys
import threading
import time
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import forge  # noqa: E402

_spec = importlib.util.spec_from_file_location(
    "pcms_server_forge_test", Path(__file__).with_name("server.py"))
assert _spec is not None and _spec.loader is not None
srv = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(srv)

TOKEN = "stub-credential-for-tests-7d1e"
SHA = lambda n: f"{n:040x}"  # noqa: E731


class StubGitLab:
    """A tiny in-memory GitLab: projects/1 (also group/site), merge
    requests, notes, branches, commits and the revert endpoint."""

    def __init__(self) -> None:
        self.requests: list[dict] = []
        self.mrs: dict[int, dict] = {}
        self.notes: dict[int, list[dict]] = {}
        self.mr_commits: dict[int, list[dict]] = {}
        self.branches: dict[str, str] = {"main": SHA(1)}
        self.merge_method = "merge"
        self.force: dict[str, tuple[int, dict]] = {}  # "METHOD path" -> (status, body)
        self.big = False
        self.redirect_to: str | None = None
        self.sleep = 0.0
        self.next_sha = 1000
        self.reverted: list[tuple[str, str]] = []
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever, kwargs={"poll_interval": 0.05},
                                       daemon=True)
        self.thread.start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def stop(self) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()

    def new_sha(self) -> str:
        self.next_sha += 1
        return SHA(self.next_sha)

    def add_mr(self, iid: int, branch: str, *, state: str = "opened", commits: int = 1,
               people: list[tuple[str, str]] | None = None, **extra) -> dict:
        commits_list = []
        for i in range(commits):
            name, email = (people or [("Alice", "alice@example.com")])[i % len(people or [1])]
            commits_list.append({"id": self.new_sha(), "author_name": name,
                                 "author_email": email, "committer_name": name,
                                 "committer_email": email})
        commits_list.reverse()  # newest first, like GitLab
        self.mr_commits[iid] = commits_list
        self.branches[branch] = commits_list[0]["id"]
        mr = {"iid": iid, "source_branch": branch, "target_branch": "main",
              "title": f"t{iid}", "description": f"risk: copy\n{iid}",
              "state": state, "sha": commits_list[0]["id"],
              "web_url": f"{self.url}/group/site/-/merge_requests/{iid}",
              "author": {"name": "Alice"}, "merge_commit_sha": None,
              "squash_commit_sha": None, "created_at": "2026-01-01T00:00:00Z",
              "diverged_commits_count": 0, "detailed_merge_status": "mergeable"}
        mr.update(extra)
        self.mrs[iid] = mr
        self.notes.setdefault(iid, [])
        return mr

    def _handler(self):
        stub = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):  # keep test output quiet
                pass

            def _send(self, status: int, body, headers: dict | None = None) -> None:
                raw = json.dumps(body).encode() if body is not None else b""
                if stub.big:
                    raw = b"[" + b" " * (5 * 1024 * 1024) + b"]"
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                for k, v in (headers or {}).items():
                    self.send_header(k, v)
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                try:
                    self.wfile.write(raw)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # the client stopped reading at its size cap

            def _route(self, method: str) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                raw_body = self.rfile.read(length) if length else b""
                stub.requests.append({"method": method, "path": self.path,
                                      "headers": {k.lower(): v for k, v in self.headers.items()},
                                      "body": raw_body.decode()})
                if stub.sleep:
                    time.sleep(stub.sleep)
                if stub.redirect_to:
                    self.send_response(302)
                    self.send_header("Location", stub.redirect_to + self.path)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if self.headers.get("PRIVATE-TOKEN") != TOKEN:
                    return self._send(401, {"message": "401 Unauthorized"})
                u = urllib.parse.urlsplit(self.path)
                q = dict(urllib.parse.parse_qsl(u.query))
                path = u.path
                key = f"{method} {path}"
                if key in stub.force:
                    status, body = stub.force[key]
                    return self._send(status, body)
                body = json.loads(raw_body) if raw_body else {}
                p = "/api/v4/projects/1"
                p2 = "/api/v4/projects/group%2Fsite"
                if path.startswith(p2):
                    path = p + path[len(p2):]
                if path == "/api/v4/user":
                    return self._send(200, {"username": "alice", "name": "Alice",
                                            "email": "alice@example.com"})
                if path == p:
                    return self._send(200, {"id": 1, "merge_method": stub.merge_method})
                if path == p + "/merge_requests" and method == "GET":
                    rows = list(stub.mrs.values())
                    st = q.get("state")
                    if st and st != "all":
                        rows = [m for m in rows if m["state"] == st]
                    if q.get("source_branch"):
                        rows = [m for m in rows if m["source_branch"] == q["source_branch"]]
                    rows.sort(key=lambda m: -m["iid"])
                    return self._paginate(rows, q)
                if path == p + "/merge_requests" and method == "POST":
                    src = body["source_branch"]
                    if src not in stub.branches:
                        return self._send(422, {"message": ["Source branch does not exist"]})
                    if any(m["source_branch"] == src and m["state"] == "opened"
                           for m in stub.mrs.values()):
                        return self._send(409, {"message": ["Another open merge request already exists for this source branch"]})
                    iid = max(stub.mrs, default=0) + 1
                    mr = stub.add_mr(iid, src, commits=0 or 1)
                    mr["sha"] = stub.branches[src]
                    mr["title"], mr["description"] = body["title"], body["description"]
                    return self._send(201, mr)
                parts = path[len(p):].split("/")
                if len(parts) >= 3 and parts[1] == "merge_requests":
                    iid = int(parts[2])
                    mr = stub.mrs.get(iid)
                    if mr is None:
                        return self._send(404, {"message": "404 Not found"})
                    rest = parts[3:]
                    if not rest and method == "GET":
                        return self._send(200, mr)
                    if not rest and method == "PUT":
                        if body.get("state_event") == "close":
                            mr["state"] = "closed"
                        return self._send(200, mr)
                    if rest == ["commits"]:
                        return self._paginate(stub.mr_commits.get(iid, []), q)
                    if rest == ["notes"] and method == "POST":
                        note = {"id": len(stub.notes[iid]) + 1, "body": body["body"],
                                "system": False}
                        stub.notes[iid].append(note)
                        return self._send(201, note)
                    if rest == ["notes"] and method == "GET":
                        return self._paginate(list(reversed(stub.notes[iid])), q)
                    if rest == ["merge"] and method == "PUT":
                        if mr["state"] != "opened":
                            return self._send(405, {"message": "405 Method Not Allowed"})
                        if body.get("sha") and body["sha"] != mr["sha"]:
                            return self._send(409, {"message": "SHA does not match HEAD of source branch"})
                        if stub.merge_method == "ff" and mr.get("diverged_commits_count"):
                            return self._send(406, {"message": "Branch cannot be merged"})
                        mr["state"] = "merged"
                        mr["merge_commit_sha"] = (None if stub.merge_method == "ff"
                                                  else stub.new_sha())
                        stub.branches["main"] = mr["merge_commit_sha"] or mr["sha"]
                        if body.get("should_remove_source_branch"):
                            stub.branches.pop(mr["source_branch"], None)
                        return self._send(200, mr)
                if parts[1:3] == ["repository", "branches"]:
                    if method == "POST":
                        stub.branches[body["branch"]] = stub.branches[body["ref"]]
                        return self._send(201, {"name": body["branch"]})
                    if method == "DELETE":
                        name = urllib.parse.unquote(parts[3])
                        if stub.branches.pop(name, None) is None:
                            return self._send(404, {"message": "404 Branch Not Found"})
                        return self._send(204, None)
                if parts[1:3] == ["repository", "commits"] and parts[4:] == ["revert"]:
                    sha = parts[3]
                    new = stub.new_sha()
                    stub.reverted.append((sha, body["branch"]))
                    stub.branches[body["branch"]] = new
                    return self._send(201, {"id": new, "title": f"Revert {sha[:8]}"})
                return self._send(404, {"message": "404 Not found"})

            def _paginate(self, rows: list, q: dict) -> None:
                per = int(q.get("per_page") or 20)
                page = int(q.get("page") or 1)
                chunk = rows[(page - 1) * per: page * per]
                hdrs = {"X-Next-Page": str(page + 1) if page * per < len(rows) else ""}
                self._send(200, chunk, hdrs)

            def do_GET(self):
                self._route("GET")

            def do_POST(self):
                self._route("POST")

            def do_PUT(self):
                self._route("PUT")

            def do_DELETE(self):
                self._route("DELETE")

        return H


class _StubCase(unittest.TestCase):
    def setUp(self) -> None:
        self.stub = StubGitLab()
        self.addCleanup(self.stub.stop)
        env = {k: v for k, v in os.environ.items()
               if k.lower() not in ("http_proxy", "https_proxy", "all_proxy", "no_proxy")}
        env.update({"FORGE": "gitlab", "GITLAB_URL": self.stub.url,
                    "GITLAB_PROJECT": "group/site", "GITLAB_TOKEN": TOKEN,
                    "GITLAB_ALLOW_HTTP_LOOPBACK": "1"})
        patch = mock.patch.dict(os.environ, env, clear=True)
        patch.start()
        self.addCleanup(patch.stop)
        self.f = forge.from_env(Path("."))
        self.f.merge_poll = 0.01

    def assert_token_only_in_header(self) -> None:
        self.assertTrue(self.stub.requests)
        for r in self.stub.requests:
            self.assertNotIn(TOKEN, r["path"])
            self.assertNotIn(urllib.parse.quote(TOKEN), r["path"])
            self.assertNotIn(TOKEN, r["body"])
            self.assertEqual(r["headers"].get("private-token"), TOKEN)
            self.assertNotIn("authorization", r["headers"])


class GitLabForgeTest(_StubCase):
    def test_create_change(self):
        self.stub.branches["agent/hello"] = SHA(7)
        ch = self.f.create_change("agent/hello", "Hello", "risk: copy")
        self.assertEqual(ch.id, 1)
        self.assertEqual(ch.state, "open")
        self.assertEqual(ch.branch, "agent/hello")
        self.assertTrue(ch.url.endswith("/merge_requests/1"))
        post = [r for r in self.stub.requests if r["method"] == "POST"][0]
        self.assertTrue(post["path"].startswith("/api/v4/projects/group%2Fsite/merge_requests"))
        sent = json.loads(post["body"])
        self.assertEqual(sent["target_branch"], "main")
        self.assertTrue(sent["remove_source_branch"])
        self.assert_token_only_in_header()

    def test_create_change_conflict_is_clear(self):
        self.stub.add_mr(1, "agent/hello")
        with self.assertRaises(forge.ForgeError) as cm:
            self.f.create_change("agent/hello", "Hello", "b")
        self.assertEqual(cm.exception.status, 409)
        self.assertIn("conflict (409)", str(cm.exception))

    def test_get_change_by_branch_and_commit_pagination(self):
        people = [("Alice", "alice@example.com"), ("Bob", "bob@example.com")]
        self.stub.add_mr(4, "agent/long", commits=250, people=people)
        self.stub.mr_commits[4][0]["committer_name"] = "Carol"
        self.stub.mr_commits[4][0]["committer_email"] = "carol@example.com"
        ch = self.f.get_change("agent/long", with_commits=True)
        assert ch is not None
        self.assertEqual(ch.id, 4)
        self.assertEqual(ch.head_sha, self.stub.mrs[4]["sha"])
        self.assertEqual(len(ch.commits), 250)
        self.assertEqual([c["sha"] for c in ch.commits],
                         [c["id"] for c in self.stub.mr_commits[4]])
        self.assertEqual(set(ch.people), set(people) | {("Carol", "carol@example.com")})
        pages = [r for r in self.stub.requests if "/merge_requests/4/commits" in r["path"]]
        self.assertEqual(len(pages), 3)
        self.assertIsNone(self.f.get_change("agent/missing"))

    def test_merge_with_matching_sha(self):
        mr = self.stub.add_mr(2, "agent/a")
        ch = self.f.merge_change(2, sha_pin=mr["sha"])
        self.assertEqual(ch.state, "merged")
        self.assertTrue(ch.merge_sha)
        put = [r for r in self.stub.requests if r["method"] == "PUT"][0]
        self.assertEqual(json.loads(put["body"])["sha"], mr["sha"])
        self.assertNotIn("agent/a", self.stub.branches)
        self.assert_token_only_in_header()

    def test_merge_with_moved_head_refuses(self):
        self.stub.add_mr(2, "agent/a")
        with self.assertRaises(forge.ForgeError) as cm:
            self.f.merge_change("agent/a", sha_pin=SHA(99))
        self.assertEqual(cm.exception.status, 409)
        self.assertIn("head moved", str(cm.exception))
        self.assertEqual(self.stub.mrs[2]["state"], "opened")

    def test_merge_rejects_bad_sha(self):
        self.stub.add_mr(2, "agent/a")
        with self.assertRaises(forge.ForgeError):
            self.f.merge_change(2, sha_pin="../../x")
        self.assertFalse([r for r in self.stub.requests if r["method"] == "PUT"])

    def test_fast_forward_project_behind_fails_cleanly(self):
        self.stub.merge_method = "ff"
        mr = self.stub.add_mr(3, "agent/ff", diverged_commits_count=2,
                              detailed_merge_status="need_rebase")
        with self.assertRaises(forge.ForgeError) as cm:
            self.f.merge_change(3, sha_pin=mr["sha"])
        self.assertIn("fast-forward only", str(cm.exception))
        self.assertIn("does not rebase", str(cm.exception))
        self.assertFalse([r for r in self.stub.requests if r["path"].endswith("/merge")])

    def test_fast_forward_project_up_to_date_merges(self):
        self.stub.merge_method = "ff"
        mr = self.stub.add_mr(3, "agent/ff")
        ch = self.f.merge_change(3, sha_pin=mr["sha"])
        self.assertEqual(ch.state, "merged")
        self.assertEqual(ch.merge_sha, mr["sha"])

    def test_close_deletes_branch(self):
        self.stub.add_mr(5, "agent/c")
        self.f.close_change("agent/c")
        self.assertEqual(self.stub.mrs[5]["state"], "closed")
        self.assertNotIn("agent/c", self.stub.branches)
        delete = [r for r in self.stub.requests if r["method"] == "DELETE"][0]
        self.assertIn("/repository/branches/agent%2Fc", delete["path"])

    def test_comment_and_last_comment_skip_system_notes(self):
        self.stub.add_mr(6, "agent/n")
        self.f.comment(6, "REJECTED: too long")
        self.stub.notes[6].append({"id": 9, "body": "closed", "system": True})
        self.assertEqual(self.f.last_comment(6), "REJECTED: too long")
        self.assertEqual(self.stub.notes[6][0]["body"], "REJECTED: too long")

    def test_revert_merge_commit_through_mr(self):
        mr = self.stub.add_mr(7, "agent/r")
        self.f.merge_change(7, sha_pin=mr["sha"])
        merged = self.f.get_change(7, with_commits=True)
        assert merged is not None
        rev = self.f.revert_change(merged)
        self.assertEqual(self.stub.reverted, [(merged.merge_sha, rev.branch)])
        self.assertTrue(rev.branch.startswith("revert/r-"))
        self.assertEqual(rev.head_sha, self.stub.branches[rev.branch])
        done = self.f.merge_change(rev.id, sha_pin=rev.head_sha)
        self.assertEqual(done.state, "merged")
        self.assertFalse([r for r in self.stub.requests
                          if r["method"] == "PUT" and "/repository/" in r["path"]])

    def test_revert_fast_forward_reverts_each_commit_newest_first(self):
        self.stub.merge_method = "ff"
        mr = self.stub.add_mr(8, "agent/ffr", commits=3)
        self.f.merge_change(8, sha_pin=mr["sha"])
        merged = self.f.get_change(8, with_commits=True)
        assert merged is not None
        merged.merge_sha = None  # ff: GitLab reports no merge commit
        self.f.revert_change(merged)
        self.assertEqual([s for s, _ in self.stub.reverted],
                         [c["id"] for c in self.stub.mr_commits[8]])

    def test_revert_failure_removes_revert_branch(self):
        mr = self.stub.add_mr(9, "agent/x")
        self.f.merge_change(9, sha_pin=mr["sha"])
        merged = self.f.get_change(9)
        assert merged is not None
        before = set(self.stub.branches)
        self.stub.force[f"POST /api/v4/projects/group%2Fsite/repository/commits/{merged.merge_sha}/revert"] = (
            422, {"message": "Sorry, we cannot revert this commit automatically."})
        with self.assertRaises(forge.ForgeError) as cm:
            self.f.revert_change(merged)
        self.assertIn("422", str(cm.exception))
        self.assertEqual(set(self.stub.branches), before)

    def test_error_mapping(self):
        self.stub.add_mr(1, "agent/e")
        base = "/api/v4/projects/group%2Fsite/merge_requests/1"
        cases = {401: "token rejected", 403: "forbidden", 404: "not found",
                 405: "not allowed in its current state", 409: "conflict (409)"}
        for status, phrase in cases.items():
            with self.subTest(status=status):
                self.stub.force = {f"GET {base}": (status, {"message": "nope"})}
                with self.assertRaises(forge.ForgeError) as cm:
                    self.f.get_change(1)
                self.assertEqual(cm.exception.status, status)
                self.assertIn(phrase, str(cm.exception))
        self.stub.force = {}
        bad = forge.GitLabForge(self.f.base, self.f.project, "wrong-token")
        with self.assertRaises(forge.ForgeError) as cm:
            bad.get_change(1)
        self.assertIn("token rejected (401)", str(cm.exception))
        self.assertNotIn("wrong-token", str(cm.exception))

    def test_merge_405_is_clear(self):
        self.stub.add_mr(1, "agent/closed", state="closed")
        with self.assertRaises(forge.ForgeError) as cm:
            self.f.merge_change(1)
        self.assertEqual(cm.exception.status, 405)
        self.assertIn("draft, closed", str(cm.exception))

    def test_response_size_cap(self):
        self.stub.big = True
        with self.assertRaises(forge.ForgeError) as cm:
            self.f.list_changes("open")
        self.assertIn("larger than", str(cm.exception))

    def test_timeout(self):
        self.f.timeout = 0.2
        self.stub.sleep = 0.6
        with self.assertRaises(forge.ForgeError) as cm:
            self.f.current_user()
        self.assertIn("timed out", str(cm.exception))

    def test_redirect_is_not_followed_with_token(self):
        other = StubGitLab()
        self.addCleanup(other.stop)
        self.stub.redirect_to = other.url
        with self.assertRaises(forge.ForgeError) as cm:
            self.f.current_user()
        self.assertIn("redirect", str(cm.exception))
        self.assertEqual(other.requests, [])

    def test_list_changes_paginates_with_limit(self):
        for i in range(1, 131):
            self.stub.add_mr(i, f"agent/p{i}", state="merged")
        rows = self.f.list_changes("merged", limit=100)
        self.assertEqual(len(rows), 100)
        self.assertEqual(rows[0].id, 130)
        allrows = self.f.list_changes("merged")
        self.assertEqual(len(allrows), 130)

    def test_token_never_in_logs_urls_errors_or_repr(self):
        err, out = io.StringIO(), io.StringIO()
        logs = io.StringIO()
        handler = logging.StreamHandler(logs)
        logging.getLogger().addHandler(handler)
        self.addCleanup(logging.getLogger().removeHandler, handler)
        messages = [repr(self.f), str(vars(self.f).get("base"))]
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(out):
            self.stub.add_mr(1, "agent/t")
            self.f.comment(1, "hi")
            self.f.get_change("agent/t", with_commits=True)
            for status in (401, 403, 404, 405, 409, 500):
                self.stub.force = {"GET /api/v4/user": (status, {"message": TOKEN})}
                try:
                    self.f.current_user()
                except forge.ForgeError as e:
                    messages.append(str(e))
            self.stub.force = {}
        for text in [err.getvalue(), out.getvalue(), logs.getvalue(), *messages]:
            self.assertNotIn(TOKEN, text)
        self.assert_token_only_in_header()


class GitLabConfigTest(unittest.TestCase):
    def test_https_required(self):
        self.assertEqual(forge.gitlab_base_url("https://git.example.com/"),
                         "https://git.example.com")
        self.assertEqual(forge.gitlab_base_url("https://example.com/gitlab"),
                         "https://example.com/gitlab")
        for bad in ("http://git.example.com", "ftp://git.example.com",
                    "https://user:pw@git.example.com", "https://git.example.com/?private_token=x",
                    "http://127.0.0.1:9", "", "https://"):
            with self.subTest(bad=bad), self.assertRaises(forge.ForgeError):
                forge.gitlab_base_url(bad)
        self.assertEqual(forge.gitlab_base_url("http://127.0.0.1:9", allow_http_loopback=True),
                         "http://127.0.0.1:9")
        with self.assertRaises(forge.ForgeError):
            forge.gitlab_base_url("http://git.example.com", allow_http_loopback=True)

    def test_project_id(self):
        self.assertEqual(forge.gitlab_project_id("42"), "42")
        self.assertEqual(forge.gitlab_project_id("group/sub/site"), "group%2Fsub%2Fsite")
        self.assertEqual(forge.gitlab_project_id("group%2Fsite"), "group%2Fsite")
        for bad in ("", "site", "group/../x", "group/site?x=1", "group/si te"):
            with self.subTest(bad=bad), self.assertRaises(forge.ForgeError):
                forge.gitlab_project_id(bad)

    def test_factory(self):
        self.assertIsInstance(forge.from_env(Path("."), env={}), forge.GitHubForge)
        with self.assertRaises(forge.ForgeError):
            forge.from_env(Path("."), env={"FORGE": "bitbucket"})
        with self.assertRaises(forge.ForgeError) as cm:
            forge.from_env(Path("."), env={"FORGE": "gitlab", "GITLAB_URL": "https://g.example.com",
                                           "GITLAB_PROJECT": "1"})
        self.assertIn("GITLAB_TOKEN", str(cm.exception))


# ------------------------------------------------------------ GitHub mode


class FakeGh:
    """Records gh argv and answers from a table keyed by argv prefix."""

    def __init__(self, answers: dict[tuple, tuple[int, str]] | None = None) -> None:
        self.calls: list[list[str]] = []
        self.answers = answers or {}

    def __call__(self, cmd, *a, **kw):
        if cmd and cmd[0] == "gh":
            self.calls.append(list(cmd))
            for prefix, (rc, out) in self.answers.items():
                if tuple(cmd[:len(prefix)]) == prefix:
                    return subprocess.CompletedProcess(cmd, rc, out, "" if rc == 0 else "boom")
            return subprocess.CompletedProcess(cmd, 0, "", "")
        raise AssertionError(f"unexpected command {cmd!r}")


class GitHubForgeTest(unittest.TestCase):
    def setUp(self) -> None:
        self.f = forge.GitHubForge(Path("."))

    def run_with(self, fake: FakeGh):
        return mock.patch.object(subprocess, "run", fake)

    def test_create_uses_gh_pr_create(self):
        fake = FakeGh({("gh", "pr", "create"): (0, "x\nhttps://github.com/o/r/pull/5\n")})
        with self.run_with(fake):
            ch = self.f.create_change("agent/a", "T", "B", head="agent/a")
        self.assertEqual(ch.url, "https://github.com/o/r/pull/5")
        self.assertEqual(fake.calls, [["gh", "pr", "create", "--title", "T", "--body", "B",
                                       "--head", "agent/a"]])

    def test_merge_comment_close_commands(self):
        fake = FakeGh()
        with self.run_with(fake):
            self.f.comment(5, "ok")
            self.f.merge_change(5)
            self.f.merge_change(5, sha_pin=SHA(3))
            self.f.close_change(5)
        self.assertEqual(fake.calls, [
            ["gh", "pr", "comment", "5", "--body", "ok"],
            ["gh", "pr", "merge", "5", "--merge", "--delete-branch"],
            ["gh", "pr", "merge", "5", "--merge", "--delete-branch",
             "--match-head-commit", SHA(3)],
            ["gh", "pr", "close", "5", "--delete-branch"],
        ])

    def test_failures_raise_with_stderr(self):
        fake = FakeGh({("gh", "pr", "merge"): (1, "")})
        with self.run_with(fake), self.assertRaises(forge.ForgeError) as cm:
            self.f.merge_change(5)
        self.assertEqual(str(cm.exception), "boom")

    def test_get_change_and_list(self):
        fake = FakeGh({
            ("gh", "pr", "list", "--head"): (0, json.dumps([{
                "number": 5, "url": "u", "headRefName": "agent/a",
                "headRefOid": SHA(4), "state": "OPEN"}])),
            ("gh", "pr", "list", "--state", "open"): (0, json.dumps([{
                "number": 5, "title": "T", "headRefName": "agent/a", "url": "u",
                "author": {"name": "Alice"}, "body": "risk: copy"}])),
            ("gh", "api", "--paginate"): (0, json.dumps({
                "sha": SHA(4), "author_name": "Alice", "author_email": "a@example.com",
                "committer_name": "Bob", "committer_email": "b@example.com"}) + "\n"),
        })
        with self.run_with(fake):
            ch = self.f.get_change("agent/a", with_commits=True)
            rows = self.f.list_changes("open")
        assert ch is not None
        self.assertEqual((ch.id, ch.head_sha, ch.state), (5, SHA(4), "open"))
        self.assertEqual(ch.people, [("Alice", "a@example.com"), ("Bob", "b@example.com")])
        self.assertEqual((rows[0].author, rows[0].body), ("Alice", "risk: copy"))
        self.assertEqual(fake.calls[-1], ["gh", "pr", "list", "--state", "open", "--json",
                                          "number,title,headRefName,url,author,createdAt,body"])

    def test_server_defaults_to_github_and_reject_uses_gh(self):
        fake = FakeGh({("gh", "pr", "list", "--head"): (0, json.dumps([{"number": 8}]))})
        ident = {"login": "bob", "name": "Bob", "email": "bob@example.com", "source": "test"}
        env = {k: v for k, v in os.environ.items() if k != "FORGE"}
        with mock.patch.dict(os.environ, env, clear=True), self.run_with(fake), \
                mock.patch.object(srv, "_gh_identity", return_value=ident), \
                mock.patch.object(srv, "_proposal_branch", return_value="agent/a"):
            out = srv.tool_reject_proposal({"slug": "a", "reason": "no"})
        self.assertEqual((out["pr"], out["recorded"], out["closed"]), (8, True, True))
        self.assertEqual(fake.calls[1][:4], ["gh", "pr", "comment", "8"])
        self.assertEqual(fake.calls[2], ["gh", "pr", "close", "8", "--delete-branch"])


# ------------------------------------------------------ server, FORGE=gitlab


def _no_gh(cmd, *a, **kw):
    if cmd and cmd[0] == "gh":
        raise AssertionError("gh must not run in FORGE=gitlab mode")
    raise AssertionError(f"unexpected command {cmd!r}")


class ServerGitLabModeTest(_StubCase):
    def setUp(self) -> None:
        super().setUp()
        for p in (mock.patch.object(subprocess, "run", _no_gh),
                  mock.patch.object(srv, "_GH_IDENTITY_CACHE", None),
                  mock.patch.object(srv, "_proposal_field_diff", return_value=[])):
            p.start()
            self.addCleanup(p.stop)

    def test_identity_from_token_user(self):
        ident = srv._gh_identity()
        self.assertEqual(ident["login"], "alice")
        self.assertEqual(ident["source"], "gitlab:alice via GITLAB_TOKEN")

    def test_approve_one_merges_mr(self):
        self.stub.add_mr(3, "agent/a")
        with mock.patch.object(srv, "_proposal_branch", return_value="agent/a"):
            out = srv._approve_one("a", "Bob", "bob@example.com", "test")
        self.assertEqual((out["pr"], out["merged"], out["approval_recorded"]), (3, True, True))
        self.assertEqual(self.stub.mrs[3]["state"], "merged")
        self.assertIn("Approved by Bob", self.stub.notes[3][0]["body"])

    def test_approve_without_open_mr(self):
        with mock.patch.object(srv, "_proposal_branch", return_value="agent/none"), \
                self.assertRaises(ValueError) as cm:
            srv._approve_one("none", "Bob", "bob@example.com", "test")
        self.assertIn("nothing to approve", str(cm.exception))

    def test_reject_closes_mr(self):
        self.stub.add_mr(4, "agent/b")
        with mock.patch.object(srv, "_proposal_branch", return_value="agent/b"):
            out = srv.tool_reject_proposal({"slug": "b", "reason": "off-brand"})
        self.assertTrue(out["closed"] and out["recorded"])
        self.assertEqual(self.stub.mrs[4]["state"], "closed")
        self.assertTrue(self.stub.notes[4][0]["body"].startswith("REJECTED: off-brand"))

    def test_list_proposals_and_rejections(self):
        self.stub.add_mr(1, "agent/a")
        self.stub.add_mr(2, "feature/x")
        self.stub.add_mr(3, "agent/old", state="closed")
        self.stub.notes[3].append({"id": 1, "body": "REJECTED: stale", "system": False})
        out = srv.tool_list_proposals({"include_rejected": True})
        self.assertEqual([p["slug"] for p in out["proposals"]], ["a"])
        self.assertEqual(out["proposals"][0]["risk"], "copy")
        self.assertEqual(out["recently_rejected"][0]["reason"], "stale")

    def test_revert_through_pinned_mr(self):
        self.stub.add_mr(1, "agent/foo-bar")
        mr = self.stub.add_mr(2, "agent/foo-r2", people=[("Carol", "carol@example.com")])
        self.f.merge_change(1)
        self.f.merge_change(2, sha_pin=mr["sha"])
        out = srv.tool_revert_proposal({"slug": "foo", "deploy": False})
        self.assertTrue(out["reverted"])
        self.assertEqual(out["pr"], 2)  # exact branch match: not agent/foo-bar
        self.assertEqual(out["authors"], [{"name": "Carol", "email": "carol@example.com"}])
        self.assertEqual(self.stub.reverted[0][0], self.stub.mrs[2]["merge_commit_sha"])
        rev_iid = max(self.stub.mrs)
        self.assertEqual(self.stub.mrs[rev_iid]["state"], "merged")
        put = [r for r in self.stub.requests
               if r["path"].endswith(f"/merge_requests/{rev_iid}/merge")][0]
        self.assertEqual(json.loads(put["body"])["sha"], self.stub.mrs[rev_iid]["sha"])
        self.assertIn("Reverted by Alice", self.stub.notes[2][0]["body"])
        self.assert_token_only_in_header()

    def test_revert_unknown_slug(self):
        with self.assertRaises(ValueError) as cm:
            srv.tool_revert_proposal({"slug": "nope", "deploy": False})
        self.assertIn("no merged merge request", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
