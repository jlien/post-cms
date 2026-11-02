"""Forge backends for the authoring MCP server.

A forge is where a proposal becomes a reviewable change: a pull request on
GitHub, a merge request on GitLab. The server talks to one through the
`Forge` interface below and never calls `gh` or a forge API directly.

    FORGE=github   (default) wraps the `gh` CLI, exactly as before.
    FORGE=gitlab   GitLab REST API v4 over urllib; no CLI needed.
                   GITLAB_URL      https base URL (http only for a loopback
                                   host with GITLAB_ALLOW_HTTP_LOOPBACK=1,
                                   which exists for tests)
                   GITLAB_PROJECT  numeric id or path (group/project, plain
                                   or url-encoded)
                   GITLAB_TOKEN    sent in the PRIVATE-TOKEN header only;
                                   never in a URL, a log line or an error

The interface is deliberately narrow: create_change, get_change (head sha,
state, and the commit list with every author and committer), merge_change
(pinned to a head sha), close_change, comment and revert_change, plus the
queue and identity reads the server already made through gh (list_changes,
last_comment, current_user).

Standard library only.
"""

from __future__ import annotations

import json
import os
import re
import socket
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable


class ForgeError(ValueError):
    """A forge call failed. The message is safe to show a tool caller: it
    never carries a credential. `status` is the HTTP status when there was
    one."""

    def __init__(self, message: str, status: int | None = None) -> None:
        super().__init__(message)
        self.status = status


@dataclass
class Change:
    """One pull request or merge request."""

    id: int | None = None          # PR number (GitHub) or MR iid (GitLab)
    url: str = ""
    branch: str = ""               # source branch
    target: str = ""               # target branch
    title: str = ""
    body: str = ""
    state: str = ""                # open | merged | closed
    head_sha: str | None = None    # the commit a merge would land
    merge_sha: str | None = None   # merge or squash commit on the target
    author: str = ""               # display name of whoever opened it
    created_at: str | None = None
    closed_at: str | None = None
    # Every commit on the change, newest first:
    # {sha, author_name, author_email, committer_name, committer_email}.
    commits: list[dict[str, str]] = field(default_factory=list)

    # True only when the change's source is the target repository itself;
    # changes from forks are never looked up, merged, closed or listed.
    same_repo: bool | None = None

    @property
    def people(self) -> list[tuple[str, str]]:
        """Every (name, email) that authored or committed a commit on the
        change, in first-seen order. Empty unless fetched with_commits."""
        out: list[tuple[str, str]] = []
        for c in self.commits:
            for pair in ((c.get("author_name", ""), c.get("author_email", "")),
                         (c.get("committer_name", ""), c.get("committer_email", ""))):
                if any(pair) and pair not in out:
                    out.append(pair)
        return out


class Forge:
    """The seam between the MCP tools and a forge. `ref` is a change id
    (int) or the change's source branch name (str); a branch resolves to
    its open change unless `state` says otherwise."""

    name = "forge"

    def create_change(self, branch: str, title: str, body: str, *,
                      cwd: Path | None = None, head: str | None = None) -> Change:
        raise NotImplementedError

    def get_change(self, ref: int | str, *, state: str = "open",
                   with_commits: bool = False, cwd: Path | None = None) -> Change | None:
        raise NotImplementedError

    def merge_change(self, ref: int | str, *, sha_pin: str | None = None,
                     expect_branch: str | None = None, delete_branch: bool = True,
                     cwd: Path | None = None) -> Change:
        """Merge only when the change is open, comes from this repository,
        is on `expect_branch` (when given) and its head is exactly
        `sha_pin`, the head that was reviewed. The pin also goes to the
        forge, so a push between the check and the merge fails too.
        `sha_pin` is required."""
        raise NotImplementedError

    def close_change(self, ref: int | str, *, delete_branch: bool = True,
                     cwd: Path | None = None) -> None:
        raise NotImplementedError

    def comment(self, ref: int | str, body: str, *, cwd: Path | None = None) -> None:
        raise NotImplementedError

    def revert_change(self, change: Change, *, title: str = "", body: str = "") -> Change:
        raise NotImplementedError

    def list_changes(self, state: str, *, limit: int | None = None) -> list[Change]:
        raise NotImplementedError

    def last_comment(self, ref: int | str) -> str:
        raise NotImplementedError

    def current_user(self) -> dict[str, str]:
        raise NotImplementedError


_SHA_RE = re.compile(r"[0-9a-f]{7,64}")


def _check_sha(sha: str) -> str:
    if not isinstance(sha, str) or not _SHA_RE.fullmatch(sha):
        raise ForgeError("invalid commit sha")
    return sha


def _check_mergeable(ch: Change, ref: Any, sha_pin: str, expect_branch: str | None) -> None:
    if expect_branch is not None and ch.branch != expect_branch:
        raise ForgeError(f"change {ref!r} is on {ch.branch!r}, not {expect_branch!r}; "
                         "refusing to merge")
    if ch.state != "open":
        raise ForgeError(f"change {ref!r} is {ch.state or 'not open'}; nothing to merge", 405)
    if (ch.head_sha or "") != sha_pin:
        raise ForgeError(
            f"merge refused, the head moved: change {ref!r} is at "
            f"{(ch.head_sha or '?')[:12]}, not the reviewed {sha_pin[:12]}. Review the "
            "new commits and approve again.", 409)


# ---------------------------------------------------------------- GitHub


class GitHubForge(Forge):
    """Wraps the `gh` CLI with the same commands the server always ran.
    `git` runs git in the shared tree (the server's _git); revert uses it,
    since a GitHub revert is a local `git revert` pushed to main."""

    name = "github"

    _PR_FIELDS = ("number,url,headRefName,headRefOid,state,isCrossRepository,"
                  "headRepository,headRepositoryOwner")

    def __init__(self, root: Path, git: Callable[..., subprocess.CompletedProcess] | None = None):
        self.root = root
        self._git = git
        self._base: tuple[str, str] | None = None

    def _base_repo(self, cwd: Path | None = None) -> tuple[str, str]:
        if self._base is None:
            res = self._gh(["repo", "view", "--json", "name,owner"], cwd)
            if res.returncode != 0:
                raise ForgeError(f"gh repo view failed: {res.stderr.strip()[:300]}")
            try:
                j = json.loads(res.stdout or "{}")
                self._base = (str(j["owner"]["login"]).lower(), str(j["name"]).lower())
            except (ValueError, KeyError, TypeError):
                raise ForgeError("gh repo view returned unexpected JSON")
        return self._base

    def _same_repo(self, p: dict[str, Any], cwd: Path | None = None) -> bool:
        """The PR's head lives in the base repository (not a fork). Fails
        closed: a missing field counts as cross-repository."""
        if p.get("isCrossRepository") is not False:
            return False
        owner = str((p.get("headRepositoryOwner") or {}).get("login") or "").lower()
        name = str((p.get("headRepository") or {}).get("name") or "").lower()
        return (owner, name) == self._base_repo(cwd)

    def _gh(self, args: list[str], cwd: Path | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(["gh", *args], cwd=str(cwd or self.root),
                              capture_output=True, text=True)

    def create_change(self, branch, title, body, *, cwd=None, head=None):
        cmd = ["pr", "create", "--title", title, "--body", body]
        if head:
            cmd += ["--head", head]
        res = self._gh(cmd, cwd)
        if res.returncode != 0:
            raise ForgeError(res.stderr.strip()[:300])
        lines = res.stdout.strip().splitlines()
        return Change(url=lines[-1] if lines else "", branch=head or branch, state="open")

    @staticmethod
    def _from_json(p: dict[str, Any]) -> Change:
        state = str(p.get("state") or "").lower()
        return Change(
            id=p.get("number"), url=p.get("url") or "",
            branch=str(p.get("headRefName") or ""),
            title=p.get("title") or "", body=p.get("body") or "",
            state={"open": "open", "merged": "merged", "closed": "closed"}.get(state, state),
            head_sha=p.get("headRefOid"),
            author=(p.get("author") or {}).get("name") or "",
            created_at=p.get("createdAt"), closed_at=p.get("closedAt"))

    def get_change(self, ref, *, state="open", with_commits=False, cwd=None):
        if isinstance(ref, int):
            res = self._gh(["pr", "view", str(ref), "--json", self._PR_FIELDS], cwd)
            if res.returncode != 0:
                raise ForgeError(f"gh pr view failed: {res.stderr.strip()[:300]}")
            try:
                p = json.loads(res.stdout or "{}")
            except ValueError:
                raise ForgeError("gh pr view returned non-JSON")
            if not isinstance(p, dict) or not self._same_repo(p, cwd):
                raise ForgeError(f"PR #{ref} comes from another repository (a fork); "
                                 "refusing to act on it")
            ch = self._from_json(p)
        else:
            res = self._gh(["pr", "list", "--head", ref, "--state", state,
                            "--json", self._PR_FIELDS, "--limit", "20"], cwd)
            if res.returncode != 0:
                raise ForgeError(f"gh pr list failed: {res.stderr.strip()[:300]}")
            try:
                prs = json.loads(res.stdout or "[]")
            except ValueError:
                raise ForgeError("gh pr list returned non-JSON")
            exact = [p for p in prs if isinstance(p, dict) and p.get("headRefName") == ref]
            same = [p for p in exact if self._same_repo(p, cwd)]
            if not same:
                if exact:
                    raise ForgeError(f"the only PR on {ref!r} comes from another "
                                     "repository (a fork); refusing to act on it")
                return None
            ch = self._from_json(same[0])
        ch.same_repo = True
        if with_commits and ch.id is not None:
            ch.commits = self._commits(ch.id, cwd)
        return ch

    def _commits(self, number: int, cwd: Path | None) -> list[dict[str, str]]:
        jq = (".[] | {sha: .sha, author_name: .commit.author.name, "
              "author_email: .commit.author.email, committer_name: "
              ".commit.committer.name, committer_email: .commit.committer.email}")
        res = self._gh(["api", "--paginate", f"repos/{{owner}}/{{repo}}/pulls/{number}/commits",
                        "--jq", jq], cwd)
        if res.returncode != 0:
            raise ForgeError(f"gh api commits failed: {res.stderr.strip()[:300]}")
        out: list[dict[str, str]] = []
        for line in res.stdout.splitlines():
            if line.strip():
                try:
                    c = json.loads(line)
                except ValueError:
                    raise ForgeError("gh api commits returned non-JSON")
                out.append({k: str(v or "") for k, v in c.items()})
        out.reverse()  # the API lists oldest first; Change.commits is newest first
        return out

    def merge_change(self, ref, *, sha_pin=None, expect_branch=None, delete_branch=True,
                     cwd=None):
        if not sha_pin:
            raise ForgeError("merge refused: no reviewed head sha to pin to")
        _check_sha(sha_pin)
        ch = self.get_change(ref, cwd=cwd)  # refuses fork PRs
        if ch is None or ch.id is None:
            raise ForgeError(f"no open PR for {ref!r}")
        _check_mergeable(ch, ref, sha_pin, expect_branch)
        cmd = ["pr", "merge", str(ch.id), "--merge"]
        if delete_branch:
            cmd.append("--delete-branch")
        cmd += ["--match-head-commit", sha_pin]
        res = self._gh(cmd, cwd)
        if res.returncode != 0:
            raise ForgeError(res.stderr.strip()[:300])
        ch.state = "merged"
        return ch

    def close_change(self, ref, *, delete_branch=True, cwd=None):
        cmd = ["pr", "close", str(ref)]
        if delete_branch:
            cmd.append("--delete-branch")
        res = self._gh(cmd, cwd)
        if res.returncode != 0:
            raise ForgeError(res.stderr.strip()[:300])

    def comment(self, ref, body, *, cwd=None):
        res = self._gh(["pr", "comment", str(ref), "--body", body], cwd)
        if res.returncode != 0:
            raise ForgeError(res.stderr.strip()[:300])

    def revert_change(self, change, *, title="", body=""):
        """Today's GitHub revert: `git revert -m 1 <merge>` on the shared
        tree's main and push it. The caller has already found the merge
        commit in `git log origin/main --merges`."""
        if self._git is None:
            raise ForgeError("GitHub revert needs a git runner")
        if not change.merge_sha:
            raise ForgeError("no merge commit to revert")
        git = self._git
        switch = git("switch", "main")
        if switch.returncode != 0:
            git("switch", "-")
        dirty = git("status", "--porcelain", "--untracked-files=no")
        if dirty.stdout.strip():
            raise ForgeError(f"shared tree is dirty: {dirty.stdout.strip()[:200]}")
        pull = git("pull", "--ff-only", "origin", "main")
        if pull.returncode != 0:
            raise ForgeError(f"pull failed: {pull.stderr.strip()[:300]}")
        revert = git("revert", "--no-edit", "-m", "1", change.merge_sha)
        if revert.returncode != 0:
            raise ForgeError(f"revert failed: {revert.stderr.strip()[:400]}")
        push = git("push", "origin", "main")
        if push.returncode != 0:
            raise ForgeError(f"push of revert failed: {push.stderr.strip()[:300]}")
        return Change(state="merged", target="main")

    _LIST_FIELDS = {
        "open": "number,title,headRefName,url,author,createdAt,body,isCrossRepository",
        "closed": "number,title,headRefName,url,closedAt,isCrossRepository",
        "merged": "number,title,headRefName,url,closedAt,isCrossRepository",
    }

    def list_changes(self, state, *, limit=None):
        cmd = ["pr", "list", "--state", state, "--json", self._LIST_FIELDS.get(
            state, "number,title,headRefName,url,isCrossRepository")]
        if limit is not None:
            cmd += ["--limit", str(limit)]
        res = self._gh(cmd)
        if res.returncode != 0:
            raise ForgeError(f"gh pr list failed: {res.stderr.strip()[:300]}")
        try:
            prs = json.loads(res.stdout or "[]")
        except ValueError:
            raise ForgeError(f"gh pr list returned non-JSON: {res.stdout[:200]}")
        # Fork PRs never enter the queue, rejections or revert lookups.
        return [self._from_json(p) for p in prs
                if isinstance(p, dict) and p.get("isCrossRepository") is False]

    def last_comment(self, ref):
        res = self._gh(["pr", "view", str(ref), "--json", "comments",
                        "--jq", ".comments[-1].body // \"\""])
        return res.stdout.strip()

    def current_user(self):
        res = self._gh(["api", "user"])
        if res.returncode != 0:
            raise ForgeError(f"`gh api user` failed ({res.stderr.strip()[:200]})")
        try:
            u = json.loads(res.stdout)
        except ValueError:
            raise ForgeError("could not parse `gh api user` output")
        return {"login": u.get("login") or "", "name": u.get("name") or "",
                "email": u.get("email") or "",
                "source": f"github:{u.get('login')} via gh session"}


# ---------------------------------------------------------------- GitLab


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Never follow a redirect: the request carries the token, and a
    redirect could hand it to another host."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: D401
        return None


_LOOPBACK = {"127.0.0.1", "localhost", "::1"}
_PROJECT_PATH_RE = re.compile(r"[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)+")


def gitlab_base_url(raw: str, allow_http_loopback: bool = False) -> str:
    """Validate GITLAB_URL: https, a host, no credentials, query or
    fragment. Plain http only for a loopback host with the test override."""
    raw = (raw or "").strip()
    if not raw:
        raise ForgeError("GITLAB_URL is not set")
    try:
        u = urllib.parse.urlsplit(raw)
        host = u.hostname
        _ = u.port
    except ValueError:
        raise ForgeError("GITLAB_URL is not a valid URL")
    if not host:
        raise ForgeError("GITLAB_URL has no host")
    if u.username or u.password:
        raise ForgeError("GITLAB_URL must not carry credentials; use GITLAB_TOKEN")
    if u.query or u.fragment:
        raise ForgeError("GITLAB_URL must not have a query or fragment")
    if u.scheme == "https":
        pass
    elif u.scheme == "http" and allow_http_loopback and host in _LOOPBACK:
        pass
    else:
        raise ForgeError("GITLAB_URL must use https")
    return urllib.parse.urlunsplit((u.scheme, u.netloc, u.path.rstrip("/"), "", ""))


def gitlab_project_id(raw: str) -> str:
    """GITLAB_PROJECT as a URL path segment: a numeric id, or a project
    path (group/sub/project) given plain or url-encoded."""
    raw = (raw or "").strip()
    if not raw:
        raise ForgeError("GITLAB_PROJECT is not set")
    if raw.isdigit():
        return raw
    path = urllib.parse.unquote(raw) if "%" in raw else raw
    if not _PROJECT_PATH_RE.fullmatch(path) or any(
            seg in (".", "..") for seg in path.split("/")):
        raise ForgeError("GITLAB_PROJECT must be a numeric id or a group/project path")
    return urllib.parse.quote(path, safe="")


_JSON_TOKEN_RE = re.compile(r'"(?:\\.|[^"\\])*"|[\[\]{}]')


def _json_depth_ok(text: str, limit: int) -> bool:
    """True when the JSON nests no deeper than `limit` (strings skipped)."""
    depth = 0
    for m in _JSON_TOKEN_RE.finditer(text):
        t = m.group(0)
        if t in "[{":
            depth += 1
            if depth > limit:
                return False
        elif t in "]}":
            depth -= 1
    return True


class GitLabForge(Forge):
    """GitLab REST API v4. Every request carries the token in the
    PRIVATE-TOKEN header, has a socket timeout, an overall deadline and a
    response size cap, ignores proxy settings and refuses redirects. Only
    merge requests whose source and target are this project are ever
    looked up, listed, merged or closed; fork merge requests are refused."""

    name = "gitlab"

    def __init__(self, base_url: str, project: str, token: str, *,
                 target: str = "main", timeout: float = 20.0, deadline: float = 60.0,
                 max_bytes: int = 4 * 1024 * 1024, max_total_bytes: int = 16 * 1024 * 1024,
                 max_pages: int = 50, max_depth: int = 64, merge_poll: float = 1.0) -> None:
        if not token or any(c in token for c in "\r\n\0"):
            raise ForgeError("GITLAB_TOKEN is not set or is malformed")
        self.base = base_url
        self.project = project
        self.__token = token
        self.target = target
        self.timeout = timeout
        self.deadline = deadline
        self.max_bytes = max_bytes
        self.max_total_bytes = max_total_bytes
        self.max_pages = max_pages
        self.max_depth = max_depth
        self.merge_poll = merge_poll
        self._project_info: dict[str, Any] | None = None
        # No proxies: the token must never go to a proxy (in clear over the
        # http loopback test override), and the API host is fixed config.
        self._opener = urllib.request.build_opener(urllib.request.ProxyHandler({}),
                                                   _NoRedirect)

    def __repr__(self) -> str:
        return f"GitLabForge(base={self.base!r}, project={self.project!r})"

    # -- transport

    def _scrub(self, text: str) -> str:
        return text.replace(self.__token, "[redacted]") if self.__token else text

    def _request(self, method: str, path: str, *, params: dict[str, Any] | None = None,
                 body: dict[str, Any] | None = None, op: str = "") -> tuple[Any, Any]:
        data, headers, _ = self._request_n(method, path, params=params, body=body, op=op)
        return data, headers

    def _request_n(self, method: str, path: str, *, params: dict[str, Any] | None = None,
                   body: dict[str, Any] | None = None, op: str = "") -> tuple[Any, Any, int]:
        url = f"{self.base}/api/v4{path}"
        if params:
            url += "?" + urllib.parse.urlencode(
                {k: v for k, v in params.items() if v is not None})
        if self.__token in url:  # defensive: the token never rides in a URL
            raise ForgeError("refusing to send a request with the token in its URL")
        data = None
        headers = {"PRIVATE-TOKEN": self.__token, "Accept": "application/json",
                   "User-Agent": "post-cms-mcp"}
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        op = op or f"{method} {path.split('?')[0]}"
        end = time.monotonic() + self.deadline
        chunks: list[bytes] = []
        total = 0
        try:
            with self._opener.open(req, timeout=self.timeout) as resp:
                resp_headers = resp.headers
                while True:
                    if time.monotonic() > end:
                        raise ForgeError(f"GitLab {op}: no complete response within "
                                         f"{self.deadline:g}s")
                    chunk = resp.read1(65536)
                    if not chunk:
                        break
                    total += len(chunk)
                    if total > self.max_bytes:
                        raise ForgeError(
                            f"GitLab {op}: response larger than {self.max_bytes} bytes")
                    chunks.append(chunk)
        except ForgeError:
            raise
        except urllib.error.HTTPError as e:
            try:
                detail = e.read(2048).decode("utf-8", "replace")
            except Exception:  # noqa: BLE001
                detail = ""
            finally:
                e.close()
            raise self._http_error(e.code, detail, op) from None
        except (TimeoutError, socket.timeout):
            raise ForgeError(f"GitLab {op}: timed out after {self.timeout:g}s") from None
        except urllib.error.URLError as e:
            reason = getattr(e, "reason", e)
            if isinstance(reason, (TimeoutError, socket.timeout)):
                raise ForgeError(f"GitLab {op}: timed out after {self.timeout:g}s") from None
            raise ForgeError(self._scrub(f"GitLab {op}: unreachable ({reason})")) from None
        except OSError as e:
            raise ForgeError(self._scrub(f"GitLab {op}: connection failed ({e})")) from None
        raw = b"".join(chunks)
        if not raw.strip():
            return None, resp_headers, total
        text = raw.decode("utf-8", "replace")
        if not _json_depth_ok(text, self.max_depth):
            raise ForgeError(f"GitLab {op}: response nests deeper than {self.max_depth}")
        try:
            return json.loads(text), resp_headers, total
        except (ValueError, RecursionError):
            raise ForgeError(f"GitLab {op}: response is not JSON") from None

    def _http_error(self, status: int, detail: str, op: str) -> ForgeError:
        msg = ""
        try:
            j = json.loads(detail)
            m = j.get("message") or j.get("error") if isinstance(j, dict) else None
            msg = m if isinstance(m, str) else json.dumps(m) if m else ""
        except (ValueError, RecursionError):
            msg = ""
        msg = self._scrub(msg)[:200]
        tail = f": {msg}" if msg else ""
        if 300 <= status < 400:
            text = (f"GitLab {op}: answered with a redirect ({status}); refusing to "
                    "follow it with credentials. Check GITLAB_URL")
        elif status == 401:
            text = (f"GitLab {op}: token rejected (401). GITLAB_TOKEN is missing, "
                    "expired or revoked")
        elif status == 403:
            text = (f"GitLab {op}: forbidden (403). The token lacks permission: it "
                    "needs the api scope and a role that may do this (Maintainer "
                    f"to merge into a protected branch){tail}")
        elif status == 404:
            text = (f"GitLab {op}: not found (404). Check GITLAB_PROJECT and that "
                    "the token can see the project")
        elif status == 405:
            text = (f"GitLab {op}: not allowed in its current state (405): the merge "
                    "request may be a draft, closed, blocked by a pipeline or missing "
                    f"approvals{tail}")
        elif status == 406:
            text = (f"GitLab {op}: the branch cannot be merged (406): it has "
                    f"conflicts or must be rebased{tail}")
        elif status == 409:
            text = f"GitLab {op}: conflict (409){tail}"
        elif status == 422:
            text = f"GitLab {op}: rejected (422){tail}"
        else:
            text = f"GitLab {op}: failed ({status}){tail}"
        return ForgeError(text, status)

    def _paged(self, path: str, params: dict[str, Any] | None = None, *,
               limit: int | None = None, op: str = "", max_pages: int | None = None) -> list[Any]:
        out: list[Any] = []
        page: str | None = "1"
        pages = 0
        total = 0
        cap = max_pages or self.max_pages
        per_page = 100 if limit is None else max(1, min(100, limit))
        while page:
            pages += 1
            if pages > cap:
                raise ForgeError(f"GitLab {op or path}: more than {cap} pages")
            data, headers, n = self._request_n(
                "GET", path, params={**(params or {}), "per_page": per_page, "page": page},
                op=op)
            total += n
            if total > self.max_total_bytes:
                raise ForgeError(f"GitLab {op or path}: more than {self.max_total_bytes} "
                                 "bytes across pages")
            if not isinstance(data, list):
                raise ForgeError(f"GitLab {op or path}: expected a list")
            out.extend(data)
            if limit is not None and len(out) >= limit:
                return out[:limit]
            nxt = (headers.get("X-Next-Page") or "").strip() if headers else ""
            page = nxt if nxt.isdigit() else None
        return out

    # -- helpers

    def _p(self, rest: str = "") -> str:
        return f"/projects/{self.project}{rest}"

    @staticmethod
    def _state(s: str) -> str:
        return {"opened": "open", "locked": "open", "merged": "merged",
                "closed": "closed"}.get(s or "", s or "")

    @staticmethod
    def _gl_state(state: str) -> str:
        return {"open": "opened", "merged": "merged", "closed": "closed",
                "all": "all"}.get(state, state)

    def project_info(self) -> dict[str, Any]:
        if self._project_info is None:
            data, _ = self._request("GET", self._p(), op="read project")
            self._project_info = data if isinstance(data, dict) else {}
        return self._project_info

    def _project_num(self) -> str:
        pid = self.project_info().get("id")
        if not isinstance(pid, int):
            raise ForgeError("GitLab read project: no numeric project id")
        return str(pid)

    def _same(self, mr: dict[str, Any]) -> bool:
        """Source and target are both this project: never a fork MR."""
        pid = self._project_num()
        return str(mr.get("source_project_id")) == pid == str(mr.get("target_project_id"))

    def _to_change(self, mr: dict[str, Any]) -> Change:
        return Change(
            id=mr.get("iid"), url=mr.get("web_url") or "",
            branch=mr.get("source_branch") or "", target=mr.get("target_branch") or "",
            title=mr.get("title") or "", body=mr.get("description") or "",
            state=self._state(mr.get("state") or ""),
            head_sha=mr.get("sha"),
            merge_sha=mr.get("merge_commit_sha") or mr.get("squash_commit_sha"),
            author=((mr.get("author") or {}).get("name") or ""),
            created_at=mr.get("created_at"),
            closed_at=mr.get("merged_at") or mr.get("closed_at"),
            same_repo=self._same(mr))

    def _checked_mr(self, iid: int, params: dict[str, Any] | None = None) -> dict[str, Any]:
        mr, _ = self._request("GET", self._p(f"/merge_requests/{int(iid)}"), params=params,
                              op="read merge request")
        if not isinstance(mr, dict) or not self._same(mr):
            raise ForgeError(f"merge request !{iid} comes from another project (a fork); "
                             "refusing to act on it")
        return mr

    def _iid(self, ref: int | str, state: str = "open") -> int:
        if isinstance(ref, int):
            return ref
        ch = self._find_by_branch(ref, state)
        if ch is None or ch.id is None:
            raise ForgeError(f"no {state} merge request for branch {ref!r}", 404)
        return ch.id

    def _find_by_branch(self, branch: str, state: str) -> Change | None:
        mrs = self._paged(self._p("/merge_requests"), {
            "source_branch": branch, "target_branch": self.target,
            "state": self._gl_state(state), "order_by": "updated_at", "sort": "desc"},
            limit=20, max_pages=1, op="find merge request")
        exact = [m for m in mrs if isinstance(m, dict) and m.get("source_branch") == branch]
        same = [m for m in exact if self._same(m)]
        if not same:
            if exact:
                raise ForgeError(f"the only merge request on {branch!r} comes from another "
                                 "project (a fork); refusing to act on it")
            return None
        return self._to_change(same[0])

    # -- interface

    def create_change(self, branch, title, body, *, cwd=None, head=None):
        data, _ = self._request("POST", self._p("/merge_requests"), body={
            "source_branch": head or branch, "target_branch": self.target,
            "title": title, "description": body, "remove_source_branch": True,
        }, op="create merge request")
        return self._to_change(data or {})

    def get_change(self, ref, *, state="open", with_commits=False, cwd=None):
        if isinstance(ref, int):
            ch = self._to_change(self._checked_mr(ref))
        else:
            ch = self._find_by_branch(ref, state)
            if ch is None:
                return None
        if with_commits and ch.id is not None:
            ch.commits = self.commits(ch.id)
        return ch

    def commits(self, iid: int) -> list[dict[str, str]]:
        """The merge request's commits from the MR commits API (newest
        first), every page: the author set an approval check reads."""
        rows = self._paged(self._p(f"/merge_requests/{iid}/commits"),
                           op="list merge request commits")
        return [{"sha": str(c.get("id") or ""),
                 "author_name": str(c.get("author_name") or ""),
                 "author_email": str(c.get("author_email") or ""),
                 "committer_name": str(c.get("committer_name") or ""),
                 "committer_email": str(c.get("committer_email") or "")}
                for c in rows if isinstance(c, dict)]

    def merge_change(self, ref, *, sha_pin=None, expect_branch=None, delete_branch=True,
                     cwd=None):
        if not sha_pin:
            raise ForgeError("merge refused: no reviewed head sha to pin to")
        _check_sha(sha_pin)
        iid = self._iid(ref)
        # One read checks the project, branch, state and head, and gives
        # the fast-forward status.
        mr = self._checked_mr(iid, {"include_diverged_commits_count": "true"})
        _check_mergeable(self._to_change(mr), ref, sha_pin, expect_branch)
        method = str(self.project_info().get("merge_method") or "merge")
        if method in ("ff", "rebase_merge"):
            # The project only accepts a branch that sits on top of the
            # target. The server never rebases for the approver: a rebase
            # makes a new head sha, which is not the one that was approved.
            behind = int(mr.get("diverged_commits_count") or 0)
            if behind > 0 or mr.get("detailed_merge_status") == "need_rebase" \
                    or mr.get("merge_status") == "cannot_be_merged_rebase":
                raise ForgeError(
                    f"GitLab merge: the project merges fast-forward only ({method}) and "
                    f"{mr.get('source_branch') or ref!r} is behind {self.target} by "
                    f"{behind} commit(s). Rebase the proposal onto {self.target} and "
                    "approve its new head; the server does not rebase, because that "
                    "would change the approved commit.", 406)
        try:
            data, _ = self._request("PUT", self._p(f"/merge_requests/{iid}/merge"), body={
                "sha": sha_pin, "should_remove_source_branch": delete_branch,
            }, op="merge")
        except ForgeError as e:
            if e.status == 409:
                raise ForgeError(
                    f"GitLab merge: refused, the head moved: merge request !{iid} is no "
                    f"longer at {sha_pin[:12]}. Review the new commits and approve "
                    "again.", 409) from None
            raise
        mr = data or {}
        for _ in range(5):
            if mr.get("state") == "merged":
                break
            time.sleep(self.merge_poll)
            mr, _ = self._request("GET", self._p(f"/merge_requests/{iid}"),
                                  op="read merge request")
            mr = mr or {}
        if mr.get("state") != "merged":
            raise ForgeError(f"GitLab merge: !{iid} was accepted but is not merged yet "
                             f"(state {mr.get('state')!r})")
        ch = self._to_change(mr)
        if not ch.merge_sha and method == "ff":
            ch.merge_sha = ch.head_sha
        return ch

    def _delete_branch(self, branch: str) -> None:
        try:
            self._request("DELETE", self._p(
                f"/repository/branches/{urllib.parse.quote(branch, safe='')}"),
                op="delete branch")
        except ForgeError as e:
            if e.status != 404:
                raise

    def close_change(self, ref, *, delete_branch=True, cwd=None):
        iid = self._iid(ref)
        mr = self._checked_mr(iid)  # a fork MR's branch name is not ours to delete
        self._request("PUT", self._p(f"/merge_requests/{iid}"),
                      body={"state_event": "close"}, op="close merge request")
        branch = mr.get("source_branch") or ""
        if delete_branch and branch:
            self._delete_branch(branch)

    def comment(self, ref, body, *, cwd=None):
        iid = self._iid(ref, "all")
        self._request("POST", self._p(f"/merge_requests/{iid}/notes"),
                      body={"body": body}, op="comment")

    def last_comment(self, ref):
        iid = self._iid(ref, "all")
        notes = self._paged(self._p(f"/merge_requests/{iid}/notes"),
                            {"sort": "desc", "order_by": "created_at"},
                            limit=20, op="list notes")
        for n in notes:
            if isinstance(n, dict) and not n.get("system"):
                return str(n.get("body") or "").strip()
        return ""

    def revert_change(self, change, *, title="", body=""):
        """Revert a merged change through a new merge request: a fresh
        branch from the target, GitLab's commit revert endpoint applied
        there for each commit the change landed, and an MR the caller
        merges pinned to the revert head. Nothing is pushed to the target
        directly, so protected-branch rules, pipelines and approval rules
        apply to the revert like any change. The returned head is the
        revert commit the server created; an MR reporting another head is
        refused."""
        if change.state and change.state != "merged":
            raise ForgeError(f"merge request !{change.id} is not merged; nothing to revert")
        if change.merge_sha:
            shas = [change.merge_sha]
        elif change.commits:
            shas = [c["sha"] for c in change.commits]  # newest first
        elif change.head_sha:
            shas = [change.head_sha]
        else:
            raise ForgeError(f"merge request !{change.id}: no commits to revert")
        for s in shas:
            _check_sha(s)
        target = change.target or self.target
        src = re.sub(r"[^A-Za-z0-9._-]+", "-", change.branch.removeprefix("agent/")) or "change"
        branch = f"revert/{src}-{int(time.time() * 1000)}"
        self._request("POST", self._p("/repository/branches"),
                      body={"branch": branch, "ref": target}, op="create revert branch")
        head: str | None = None
        try:
            for s in shas:
                data, _ = self._request("POST", self._p(f"/repository/commits/{s}/revert"),
                                        body={"branch": branch}, op="revert commit")
                head = str((data or {}).get("id") or "")
                _check_sha(head)
            data, _ = self._request("POST", self._p("/merge_requests"), body={
                "source_branch": branch, "target_branch": target,
                "title": title or f"Revert {change.branch or f'!{change.id}'}",
                "description": body or f"Reverts !{change.id}.",
                "remove_source_branch": True,
            }, op="create revert merge request")
        except ForgeError:
            try:
                self._delete_branch(branch)
            except ForgeError:
                pass
            raise
        ch = self._to_change(data or {})
        if ch.head_sha and ch.head_sha != head:
            raise ForgeError(
                f"revert merge request !{ch.id} reports head {ch.head_sha[:12]}, not the "
                f"revert commit {head[:12] if head else '?'} the server created; not "
                "merging it", 409)
        ch.head_sha = head
        return ch

    def list_changes(self, state, *, limit=None):
        rows = self._paged(self._p("/merge_requests"), {
            "state": self._gl_state(state), "target_branch": self.target,
            "order_by": "updated_at", "sort": "desc"}, limit=limit,
            max_pages=10, op="list merge requests")
        # Fork MRs never enter the queue, rejections or revert lookups.
        return [c for c in (self._to_change(r) for r in rows if isinstance(r, dict))
                if c.same_repo]

    def current_user(self):
        """Identity of the token's user. The email is the public one or
        GitLab's noreply form (<id>-<username>@users.noreply.<host>), never
        the private primary address, since it is posted in MR notes."""
        u, _ = self._request("GET", "/user", op="read user")
        u = u or {}
        login = str(u.get("username") or "")
        if not login:
            raise ForgeError("GitLab read user: no username in the response")
        uid = u.get("id")
        host = urllib.parse.urlsplit(self.base).hostname or "gitlab"
        noreply = f"{uid}-{login}@users.noreply.{host}" if isinstance(uid, int) \
            else f"{login}@users.noreply.{host}"
        return {"login": login, "name": str(u.get("name") or ""),
                "email": str(u.get("public_email") or noreply),
                "source": f"gitlab:{login} via GITLAB_TOKEN"}



# ---------------------------------------------------------------- factory


def from_env(root: Path, git: Callable[..., subprocess.CompletedProcess] | None = None,
             env: dict[str, str] | None = None) -> Forge:
    """The forge named by FORGE (github by default)."""
    env = os.environ if env is None else env
    kind = (env.get("FORGE") or "github").strip().lower()
    if kind == "github":
        return GitHubForge(root, git)
    if kind == "gitlab":
        base = gitlab_base_url(env.get("GITLAB_URL", ""),
                               allow_http_loopback=env.get("GITLAB_ALLOW_HTTP_LOOPBACK") == "1")
        project = gitlab_project_id(env.get("GITLAB_PROJECT", ""))
        return GitLabForge(base, project, env.get("GITLAB_TOKEN", ""))
    raise ForgeError("FORGE must be github or gitlab")
