"""Publish gate tests for the MCP server: approve_proposal, merge=true on the
propose_* tools, and revert_proposal share one gate (MCP_ALLOW_APPROVE,
approver rights, distinct approver over the full branch author set).

Standard library only, no network: git runs against a local bare origin in
a temp dir, and every `gh` (forge) call is stubbed and recorded.

    python3 -m unittest discover -s mcp -p 'test_*.py'
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

_spec = importlib.util.spec_from_file_location(
    "pcms_server_under_test", Path(__file__).with_name("server.py"))
assert _spec is not None and _spec.loader is not None
srv = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(srv)

_REAL_RUN = subprocess.run

PEOPLE = {
    login: {"login": login, "name": login.capitalize(),
            "email": f"{login}@example.com", "source": f"test:{login}"}
    for login in ("alice", "bob", "carol", "dave")
}

GATE_ENV = ("MCP_ALLOW_APPROVE", "PCMS_TRANSPORT", "REQUIRE_DISTINCT_APPROVER",
            "MCP_APPROVERS", "MCP_ALLOW_SELF_REVERT")


def _who(login: str) -> dict[str, str]:
    p = PEOPLE[login]
    return {"GIT_AUTHOR_NAME": p["name"], "GIT_AUTHOR_EMAIL": p["email"],
            "GIT_COMMITTER_NAME": p["name"], "GIT_COMMITTER_EMAIL": p["email"]}


class PublishGateTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="pcms-gate-test-"))
        self.addCleanup(lambda: _REAL_RUN(["rm", "-rf", str(self.tmp)]))
        # Isolated git: no user or system config, hooks or signing.
        base_env = {k: v for k, v in os.environ.items() if k not in GATE_ENV}
        base_env.update({"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"})
        env_patch = mock.patch.dict(os.environ, base_env, clear=True)
        env_patch.start()
        self.addCleanup(env_patch.stop)

        self.origin = self.tmp / "origin.git"
        self.work = self.tmp / "work"
        self.git(None, "init", "-q", "--bare", "-b", "main", str(self.origin))
        self.git(None, "clone", "-q", str(self.origin), str(self.work))
        self.git(self.work, "switch", "-q", "-c", "main")
        (self.work / "README").write_text("site\n")
        self.git(self.work, "add", "README")
        self.git(self.work, "commit", "-q", "-m", "init", who="dave")
        self.git(self.work, "push", "-q", "-u", "origin", "main")

        root_patch = mock.patch.object(srv, "ROOT", self.work)
        root_patch.start()
        self.addCleanup(root_patch.stop)

        self.gh_calls: list[list[str]] = []

        def fake_run(cmd, *a, **kw):
            if isinstance(cmd, list) and cmd and cmd[0] == "gh":
                self.gh_calls.append(list(cmd))
                out = json.dumps([{"number": 7}]) if cmd[1:3] == ["pr", "list"] else ""
                return subprocess.CompletedProcess(cmd, 0, out, "")
            if isinstance(cmd, list) and cmd[:1] == ["cargo"]:
                raise AssertionError("a gated call must not reach the build")
            return _REAL_RUN(cmd, *a, **kw)

        run_patch = mock.patch.object(srv.subprocess, "run", fake_run)
        run_patch.start()
        self.addCleanup(run_patch.stop)

    # ---- helpers -------------------------------------------------------

    def git(self, cwd: Path | None, *args: str, who: str | None = None) -> str:
        env = {**os.environ, **(_who(who) if who else {})}
        cmd = ["git", *(["-C", str(cwd)] if cwd else []), *args]
        res = _REAL_RUN(cmd, capture_output=True, text=True, env=env)
        if res.returncode != 0:
            raise AssertionError(f"{cmd}: {res.stderr}")
        return res.stdout.strip()

    def commit_on(self, branch: str, who: str, name: str) -> None:
        self.git(self.work, "switch", "-q", branch)
        (self.work / name).write_text(f"{name} by {who}\n")
        self.git(self.work, "add", name)
        self.git(self.work, "commit", "-q", "-m", f"{name}", who=who)
        self.git(self.work, "push", "-q", "-u", "origin", branch)
        self.git(self.work, "switch", "-q", "main")

    def proposal(self, slug: str, *authors: str) -> str:
        branch = f"agent/{slug}"
        self.git(self.work, "switch", "-q", "-c", branch, "main")
        self.git(self.work, "switch", "-q", "main")
        for i, who in enumerate(authors):
            self.commit_on(branch, who, f"{slug}-{i}.md")
        return branch

    def shipped(self, slug: str, *authors: str, merger: str = "carol") -> str:
        """A proposal merged into main the way the forge merges a PR."""
        branch = self.proposal(slug, *authors)
        self.git(self.work, "merge", "-q", "--no-ff", branch,
                 "-m", f"Merge pull request #7 from example/{branch}", who=merger)
        self.git(self.work, "push", "-q", "origin", "main")
        return self.origin_main()

    def origin_main(self) -> str:
        return self.git(None, "--git-dir", str(self.origin), "rev-parse", "main")

    @contextlib.contextmanager
    def as_user(self, login: str, transport: str = "http", **env: str):
        overrides = {k: v for k, v in env.items()}
        if transport == "http":
            overrides["PCMS_TRANSPORT"] = "http"
        with mock.patch.dict(os.environ, overrides):
            if transport == "http":
                token = srv._HTTP_IDENTITY.set(dict(PEOPLE[login]))
                try:
                    yield
                finally:
                    srv._HTTP_IDENTITY.reset(token)
            else:
                # stdio: identity comes from the cached gh session.
                with mock.patch.object(srv, "_GH_IDENTITY_CACHE", dict(PEOPLE[login])):
                    yield

    def merges(self) -> list[list[str]]:
        return [c for c in self.gh_calls if c[1:3] == ["pr", "merge"]]

    # ---- merge=true on the propose_* tools ----------------------------

    def test_author_cannot_self_merge_via_merge_true(self) -> None:
        calls = {
            "propose_change": {"slug": "hello", "title": "Hello", "body": "hi"},
            "propose_template": {"name": "card", "body": "{% block content %}{% endblock %}"},
            "propose_plugin": {"name": "pixel", "snippet": "<script></script>"},
        }
        for tool, args in calls.items():
            with self.subTest(tool=tool), self.as_user("alice", MCP_ALLOW_APPROVE="1"):
                with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                    srv.HANDLERS[tool]({**args, "merge": True})
        self.assertEqual(self.gh_calls, [], "a refused merge must not reach the forge")
        heads = self.git(None, "--git-dir", str(self.origin), "branch", "--list", "agent/*")
        self.assertEqual(heads, "", "a refused merge must not push a branch")

    def test_approval_off_blocks_merge_true(self) -> None:
        for env in ({}, {"MCP_ALLOW_APPROVE": "0"}):
            with self.subTest(env=env), self.as_user("alice", **env):
                with self.assertRaisesRegex(ValueError, "merging is disabled"):
                    srv.tool_propose_change({"slug": "hello", "body": "hi", "merge": True})
        with self.as_user("alice", transport="stdio", MCP_ALLOW_APPROVE="0"):
            with self.assertRaisesRegex(ValueError, "merging is disabled"):
                srv.tool_propose_change({"slug": "hello", "body": "hi", "merge": True})
        self.assertEqual(self.gh_calls, [])

    # ---- approve_proposal: full branch author set ---------------------

    def test_author_cannot_approve_after_another_person_commits(self) -> None:
        self.proposal("post-a", "alice", "bob")  # bob's commit is the latest
        for who in ("alice", "bob"):
            with self.subTest(approver=who), self.as_user(who, MCP_ALLOW_APPROVE="1"):
                with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                    srv.tool_approve_proposal({"slug": "post-a", "deploy": False})
        self.assertEqual(self.merges(), [])

    def test_committer_counts_as_author(self) -> None:
        branch = self.proposal("post-b", "alice")
        self.git(self.work, "switch", "-q", branch)
        (self.work / "x.md").write_text("x\n")
        self.git(self.work, "add", "x.md")
        env = {**_who("alice"), "GIT_COMMITTER_NAME": "Bob",
               "GIT_COMMITTER_EMAIL": "bob@example.com"}
        _REAL_RUN(["git", "-C", str(self.work), "commit", "-q", "-m", "x"],
                  env={**os.environ, **env}, check=True)
        self.git(self.work, "push", "-q", "origin", branch)
        self.git(self.work, "switch", "-q", "main")
        with self.as_user("bob", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                srv.tool_approve_proposal({"slug": "post-b", "deploy": False})

    def test_valid_approver_approves_pinned_to_checked_head(self) -> None:
        branch = self.proposal("post-a", "alice", "bob")
        head = self.git(self.work, "rev-parse", branch)
        with self.as_user("carol", MCP_ALLOW_APPROVE="1", MCP_APPROVERS="carol"):
            out = srv.tool_approve_proposal({"slug": "post-a", "deploy": False})
        self.assertEqual(out["count"], 1)
        self.assertEqual(len(self.merges()), 1)
        self.assertIn("--match-head-commit", self.merges()[0])
        self.assertIn(head, self.merges()[0])

    def test_non_approver_cannot_approve(self) -> None:
        self.proposal("post-a", "alice")
        with self.as_user("dave", MCP_ALLOW_APPROVE="1", MCP_APPROVERS="carol,bob@example.com"):
            with self.assertRaisesRegex(ValueError, "not an approver"):
                srv.tool_approve_proposal({"slug": "post-a", "deploy": False})
        self.assertEqual(self.merges(), [])

    def test_stdio_single_user_may_approve_own_proposal(self) -> None:
        self.proposal("post-a", "alice")
        with self.as_user("alice", transport="stdio"):
            out = srv.tool_approve_proposal({"slug": "post-a", "deploy": False})
        self.assertEqual(out["count"], 1)
        with self.as_user("alice", transport="stdio", REQUIRE_DISTINCT_APPROVER="1"):
            with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                srv.tool_approve_proposal({"slug": "post-a", "deploy": False})

    # ---- revert_proposal ----------------------------------------------

    def test_approval_off_blocks_revert(self) -> None:
        before = self.shipped("post-a", "alice")
        for env in ({}, {"MCP_ALLOW_APPROVE": "0"}):
            with self.subTest(env=env), self.as_user("carol", **env):
                with self.assertRaisesRegex(ValueError, "merging is disabled"):
                    srv.tool_revert_proposal({"slug": "post-a", "deploy": False})
        self.assertEqual(self.origin_main(), before)

    def test_revert_by_non_approver_is_refused(self) -> None:
        before = self.shipped("post-a", "alice")
        with self.as_user("dave", MCP_ALLOW_APPROVE="1", MCP_APPROVERS="carol"):
            with self.assertRaisesRegex(ValueError, "not an approver"):
                srv.tool_revert_proposal({"slug": "post-a", "deploy": False})
        self.assertEqual(self.origin_main(), before)
        self.assertEqual(self.gh_calls, [])

    def test_revert_by_change_author_is_refused(self) -> None:
        before = self.shipped("post-a", "alice", "bob")
        for who in ("alice", "bob"):
            for env in ({}, {"REQUIRE_DISTINCT_APPROVER": "0"}):
                with self.subTest(who=who, env=env), \
                        self.as_user(who, MCP_ALLOW_APPROVE="1", **env):
                    with self.assertRaisesRegex(ValueError, "MCP_ALLOW_SELF_REVERT"):
                        srv.tool_revert_proposal({"slug": "post-a", "deploy": False})
        self.assertEqual(self.origin_main(), before)

    def test_valid_approver_reverts(self) -> None:
        before = self.shipped("post-a", "alice", merger="bob")
        with self.as_user("carol", MCP_ALLOW_APPROVE="1", MCP_APPROVERS="carol"):
            out = srv.tool_revert_proposal({"slug": "post-a", "deploy": False})
        self.assertTrue(out["reverted"])
        self.assertNotIn("break_glass", out)
        self.assertEqual(self.git(None, "--git-dir", str(self.origin),
                                  "rev-parse", "main~1"), before)

    def test_break_glass_self_revert_is_logged(self) -> None:
        self.shipped("post-a", "alice")
        err = io.StringIO()
        with self.as_user("alice", MCP_ALLOW_APPROVE="1", MCP_ALLOW_SELF_REVERT="1"), \
                contextlib.redirect_stderr(err):
            out = srv.tool_revert_proposal({"slug": "post-a", "deploy": False})
        self.assertTrue(out["reverted"])
        self.assertEqual(out["break_glass"], "MCP_ALLOW_SELF_REVERT")
        self.assertIn("BREAK-GLASS", err.getvalue())
        comments = [c for c in self.gh_calls if c[1:3] == ["pr", "comment"]]
        self.assertTrue(any("Break-glass" in " ".join(c) for c in comments))

    def test_stdio_single_user_may_revert_own_change(self) -> None:
        self.shipped("post-a", "alice")
        with self.as_user("alice", transport="stdio"):
            out = srv.tool_revert_proposal({"slug": "post-a", "deploy": False})
        self.assertTrue(out["reverted"])
        self.assertNotIn("break_glass", out)


if __name__ == "__main__":
    unittest.main()
