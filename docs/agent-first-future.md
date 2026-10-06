# Agent-first website of the future — an outside read

Date: 2026-10-05. This is a read of the tree, not a live run. I did not
start the origin, call `propose_change`, or open a pull request. Where a
conclusion needs an execution, it is marked [unverified].

The prototype is ahead of its front door. `README.md`,
`agents/CONVENTIONS.md`, and `BUILD_REPORT.md` still describe six
tools and drafts committed on the shared tree. The code has since
added `propose_change`, `propose_template`, typed collections, theme
previews, a public read-only MCP, and retrieval. An agent that follows
the checked-in conventions misses that. The split sits under every
section below.

`content/posts/chat-content-ops/index.mdx` states the thesis: CMS
features collapse into a prompt, the reviewer sees rendered HTML, and
the chat thread is the audit trail. The question is whether the code
still believes that.

## 1. What this stack gets right

**Content is a directory.** A post is
`content/posts/<slug>/{meta.yaml,index.mdx}`, a page the same shape
under `content/pages/`, a typed collection a sibling folder plus
`schema.yaml`. `content/integrations/schema.yaml` requires `title`,
`vendor`, and `tier`; `load_collections` in `crates/build/src/main.rs`
fails the build naming the file and field when one is missing or
mistyped (`tier` must be a string, `setup_minutes` a number). Drafts
(`published: false`) emit no HTML. The build is what decides the result
is still a site.

**The write path and the public path are different programs.**
`mcp/server.py` is stdio JSON-RPC. Its `TOOLS` list is the write
surface: `list_posts`, `get_post`, `create_draft`, `update_post`,
`build_site`, `build_preview`, `add_media`, `set_theme`,
`propose_change`, `propose_template`, `publish_status`.
`create_draft`, `update_post`, and `set_theme` commit on the working
tree. `crates/server/src/main.rs`
serves a second MCP at `POST /mcp`. The comment there says the route is
unauthenticated because every tool is read-only over `dist/`. The
manifest is `list_posts`, `get_post`, and `search`. `get_post` reads
`dist/posts/<slug>/index.md` and rejects a slug that is not lowercase
ascii, digits, and dashes. Nothing on this route shells out or flips
`published`. Hosted CMSs are productizing the same split. Sanity's
role-scoped MCP versus its read-only Context product is that shape
(https://www.sanity.io/mcp).

**The build leaves a trail for a stranger's agent.** `render_llms_txt`,
`render_mcp_card` (`dist/.well-known/mcp/server-card.json`, protocol
`2024-11-05`, `auth: "none"`, `readOnly: true`), and
`render_posts_markdown` publish a text index, a discovery card, and an
`index.md` per post. `render_search_index` feeds both
`templates/base.html` and the public `search` tool (title 3, tags 2,
body 1). `render_embeddings` writes `BGESmallENV15` vectors or skips
the file if the model is missing; `/api/ask` uses them or inlines
markdown up to `LLM_MAX_RAG_CHARS` (default 120,000). That read path
is ahead of the editorial loop.

**The origin already has a few production instincts.** HTML is cached
`public, max-age=0, s-maxage=86400, stale-while-revalidate=604800`. Gzip
siblings are preferred, `..` is rejected, `nosniff` is set.
`/api/ask` is capped per IP (`RATE_LIMIT_PER_MIN`, default 10) and in
flight (`ASK_MAX_CONCURRENT`, default 4). `lint_contrast` fails the
build under 3.0:1. `build_preview` renders themes under
`dist/preview/<theme>/` without flipping `site.yaml`. `mcp/media.py`
keeps bytes out of git (widths 480 / 960 / 1600, SigV4).
`propose_template` writes `templates/<name>.html` on
`agent/template-<name>`, runs `cargo run -q -p build`, and opens a PR
only if that build passes. `tenant_worktree` checks out
`.tenants/<tenant>/` at `origin/main` so two names do not share a
directory. Prove the build, isolate the write, then ask a human.

## 2. The interface with human maintainers

### What the code does when an agent "proposes"

`tool_propose_change` (`mcp/server.py`) is documented as the path for a
shared repo. As written:

1. `author_name` and `author_email` are caller strings copied into
   git's environment. No directory, token, or GitHub lookup.
2. If `agent/<slug>-1` is absent the name is `agent/<slug>`. The loop
   never checks that unnumbered name. `git switch -C` then resets it.
   The lookup runs in the main repo and skips remote-only branches.
3. A new item is `title`, `slug`, `published: false`, optional
   `description`, and a `date` only for `posts`. Body is written only
   if passed. An existing item gets a regex patch on `description` and
   a body overwrite. `title` becomes the PR title and is not written
   back into `meta.yaml`. Tags, `published`, `vendor`, `tier`, and
   `setup_minutes` have no parameter.
4. Commit, push, `gh pr create`. The body is the claimed author plus
   `cargo run -q -p build && python3 -m http.server 8000 -d dist`.
5. `merge: true` runs `gh pr merge --merge --delete-branch`.

`agents/CONVENTIONS.md` never names this function. It still teaches
`create_draft`, then `update_post(published: true)`, then `ci/deploy.sh`
and `ci/purge.sh`, and puts the gate in a sentence the tools do not
read. `update_post` will set `published: true` and commit. The
self-test does that on a temp copy and scores it a pass.
`propose_change` and `propose_template` are not in the self-test. The
PR is the gate unless the agent passes `merge: true` or calls
`update_post`. A gate with a side door is a convention.

### The preview hides the proposal

New items are `published: false`. `render_posts` and
`render_collections` skip unpublished items, and the build log counts
"drafts skipped." The preview command runs that build and serves
`dist/`. A reviewer who follows it gets the previous site. The new post
is absent from HTML, RSS, the sitemap, `llms.txt`, the search index,
and `embeddings.json`. The page under review 404s. [The click is
unverified. The skip is what those functions do.]

An update to a post that is already public can render, because the flag
stays true. A title change still would not: the tool never writes it. A
new collection item is a third case. `load_collections` validates every
item with a `meta.yaml`, published or not. `propose_change` supplies
neither `vendor` nor `tier`, and unlike `propose_template` it does not
build before opening the PR. The PR can exist while the preview command
fails schema validation. [Unverified at runtime. It follows from the
two functions.]

The chat-content-ops post says the reviewer sees the real page. The
review tool hands them a command whose build hides drafts. That is the
central bug in the human interface.

### Is a pull request the right gate?

For a template, a theme, a schema, or Rust, yes. `propose_template` is
close: diff, build, someone who can read the failure merges it.

For editorial copy past "the author reviews their own agent," a pull
request is the wrong object. A maintainer wants the page and a field
change ("tier: partner → verified"), then a yes that means it may go
live. This repo has no `.github/` workflows, so the forge never builds
the branch, and the page is a shell command that assumes Rust.

The gate can stay a merge. The screen in front of it has to be the
page. TinaCMS is git-backed, opens a PR per branch, and still ships
`previewUrl(branch)` so reviewers leave the diff
(https://tina.io/docs/tinacloud/editorial-workflow). Sanity's Content
Agent does not publish; a person confirms into drafts or a content
release
(https://www.sanity.io/docs/content-agent/introduction). Their June
2026 essay says content is more volatile than code, and a forge already
strained by agent-written code will not review pages one commit at a
time
(https://www.sanity.io/blog/what-agents-need-to-operate-your-content).
Git remains a good store. LightCMS's marketing describes the waiting
room this stack almost built — sandboxes, a per-field diff, an audit
ledger, provenance, a public read-only MCP
(https://metavert.io/lightcms, vendor page, 2026-10-05, not
independently verified). Still missing here: a diff a non-engineer can
read, a credential that is not a free-form string, and that ledger.

### What breaks around 50 proposals a week

Fifty a week is one agent on a small team. I found no published count
of this failing at that rate (none found as of October 2026). The
breaks are already in the code:

- Reviewers who can judge a claim often will not run `cargo`. They
  stamp the queue or leave it.
- New drafts, the common case, are missing from the preview they were
  told to open. Typed collection edits fail the build or fall back to
  `update_post`, because the PR path cannot write `vendor` or `tier`.
- A second proposal can reset `agent/<slug>` via `switch -C` and then
  lose the push. Nothing expires stale `agent/*` refs. One tenant name
  is one checkout, and the refresh toward `origin/main` ignores errors.
- No batch: a page, twelve posts, and a schema are fourteen decisions.
  Authorship is a client-supplied string. Tina puts the GitHub user on
  the commit and requires both app and user to be allowed
  (https://tina.io/docs/tinacloud/git-co-authoring).
- `set_theme` still commits and rebuilds immediately. `merge: true` has
  no role check. Merge does not deploy: `ci/deploy.sh` rsyncs to
  `ORIGIN_TARGET` (default `./deploy-target`), `ci/purge.sh` appends to
  `ci/purge.log`, and the Cloudflare call is a comment. A bad deploy
  can sit behind `s-maxage=86400`.

### What the queue has to show

Keep git. Stop using the PR list as the UI. Each row needs a URL that
renders `published: false` and says so, a field diff beside the prose
diff, and a risk class (copy, new post, schema, template, theme,
publish flip, legal) so copy can be batched and the last three page a
person. Several branches should share one preview. Provenance has to be
checked — principal, sponsor, model, session — and a rejection has to
be readable by the agent; there is no `list_proposals` tool now. The
row stays open through deploy and purge. One action reverts, rebuilds,
deploys, and purges. The inbox is the product.

### Who carries the pager

Nobody is named. No `CODEOWNERS`, no reviewer argument on
`gh pr create`, no alert. `GET /health` returns `{"status":"ok"}`.
Five jobs fall out, and they collapse into one person only by luck.
The approver hears about a PR only if `gh` succeeded and someone was
requested. The release operator is separate, because merge does not
deploy or purge, and `s-maxage=86400` makes a stale edge a quiet
incident. The model operator pays for `/api/ask`: `client_ip` trusts
the first `X-Forwarded-For` hop, the comment admits a spoof gets a new
bucket, and the semaphore (4) is the real ceiling, with a 60-second
timeout, no spend cap, and no auth. The default `LLM_BASE_URL` is a
private-network address and `LLM_MODEL` defaults to `glm-5.3-flash`.
The media operator inherits the local endpoint, key, and secret written
in `mcp/media.py`. The build firefighter hears from `propose_template` when a bad template
is refused. `propose_change` can still open the PR. Chat covers all
five while one owner watches the agent. A second person, or an agent
left running, ends that. Unassigned,
the pager is silence.

## 3. What's missing for 2027

**[buildable now]** fits this design. **[needs org decision]** is
policy or vendor. **[unsolved]** has no settled answer, or no path
from here.

**Hosted write auth.** Write access is stdio on a machine that already
has the repo. The public card's `auth: "none"` and `readOnly: true`
match `POST /mcp`. [buildable now]: a bearer token, and no `merge: true`
until a role exists. [needs org decision]: the identity provider, and
whether an agent is its own principal. [unsolved]: identity that
survives a change of chat client. No MCP OAuth in this repo.

**Preview deploys.** [buildable now]: CI builds the branch, renders
drafts, comments a URL. [needs org decision]: the host. `ci/purge.sh`
names Cloudflare in a comment and does not call it.

**Draft hole and schema-blind proposals.** [buildable now], first. A
preview flag. Collection fields on `propose_change`, or drop the
argument. The pre-PR build `propose_template` already runs. Conventions
that match the tools. `update_post` unable to flip `published` on the
shared checkout.

**Analytics.** No visit counter and no proposal ledger. [buildable now]:
both, including opened / merged / rejected / reverted. [needs org
decision]: which of reject-rate, time-to-approve, and stale `/api/ask`
citations means the agent is doing the job. [unsolved]: feeding that
back without a scoreboard the agent games.

**Brand.** `lint_contrast` fails unreadable pairs, and
`themes/astra/theme.yaml` says the look is an approximation. Posts have
no schema (`load_posts` only parses). [buildable now]: a markdown lint
after someone writes the rules. [needs org decision]: the rules, and
whether legal pages are another stage. Sanity's September 2026 note
makes stages queryable data
(https://www.sanity.io/blog/everything-we-shipped-so-you-can-trust-agents-with-your-content).

**Multi-tenant hosting.** `.tenants/<name>` has no domain, quota, or
auth boundary. [buildable now]: one worktree per session, refresh
errors checked. [needs org decision]: hosting anyone else's site.
[unsolved] as a growth story. I would leave that product unstarted.

**Discovery.** The card comment cites SEP-1649 / SEP-1960. Those paths
were still moving in 2026; a later writeup prefers
`/.well-known/ai-catalog.json`
(https://github.com/modelcontextprotocol/ext-server-card/blob/main/docs/discovery.md,
https://chatforest.com/guides/mcp-registry-server-discovery/).
[buildable now] to keep the card and follow a settled spec.
Clients auto-connecting is [unsolved], and arrival because `llms.txt`
exists is [unverified].

**Corpus, injection, clock, tests, docs.** Search, embeddings,
`llms.txt`, and `load_corpus` cover posts only. `render_sitemap` skips
collection URLs. [buildable now]. Retrieved text is placed in the
`/api/ask` system message; a post can address the model. Untested
[unverified]. Fencing is [buildable now] and incomplete; the class is
[unsolved]. A scheduled bucket in `publish_status` is [buildable now];
`TASK.md` asked for one. Contact is a mailto (`site.yaml`); a form is [needs org
decision]. `BUILD_REPORT.md` (2026-10-02) verifies the v0.1 surface
only. A `propose_change` test against a temporary remote is [buildable
now] and would have caught section 2. Rewriting `README.md` and
`CONVENTIONS.md` so agents stop learning the side door is [buildable
now], and it is policy, not chores.

## 4. The contrarian take

The strongest case against this architecture: the editorial loop is the
product, and the file store is a detail a vendor has already productized.
You would be maintaining the detail.

Sanity's June 2026 essay (cited above) describes agents as content
operators and reports on the order of millions of MCP calls across tens
of thousands of orgs. Treat the number as a vendor figure. The
requirements they list are a backend both sides can query, changes under
real permission, a preview of the future state, a batch that ships or
rolls back, mass edits that land as drafts inside an existing approval
flow, a public agent that sees only approved content, a spend limit that
interrupts a large run, and stages stored as data so "what skipped legal
review" is a query. The September workflows note is the same idea.

This tree asks one person to supply each of those by hand. Preview is a
shell command. Permission is whoever can launch `mcp/server.py`.
Rollback is git, then rsync, then `ci/purge.sh`. Failing the build on a
bad schema is a real strength. Next to it, `mcp/server.py` hand-parses
YAML, and `BUILD_REPORT.md` records that parser wiping tags on a
round-trip. Two parsers for one format is how the subscription starts
to look cheap. `mcp/media.py` is SigV4 and ffmpeg for an image problem
hosted CMSs closed years ago.

What you give up with a host is rent and portability. What you get is a
screen a non-engineer already understands, preview URLs, and a second
person who can approve a page while you are away. The bus factor here
is one person who holds the Rust build, the MCP server, worktrees, ONNX
embeddings, and request signing. Theirs is a company. For a personal
site, stay. For a brand, a lawyer, or a hire who edits copy, the
company is the feature.

TinaCMS keeps markdown in the repo and still grew a visual editor,
preview links, and human-authored commits, because the forge was not
the review tool (section 2). An inbox plus previews plus real identity
is a product that already exists. What it does not hand you is this
origin, this cache policy, and this public read-only MCP. Keep those.
The chat-content-ops claim — chat replaces the CMS UI — holds while the
governor is the person in the chat. A queue is a CMS, and a homegrown
one starts behind.

## 5. Verdict

I would run a real site on this today if all of the following hold.

- One maintainer, at home in git, able to read a Rust build failure.
  They approve, they deploy, and they watch `/api/ask`.
- A few posts a week, reviewed in the session that wrote them. No
  unattended agent opening PRs overnight.
- The write MCP stays on stdio on that machine. `merge: true` stays
  unused. `create_draft`, `update_post`, and `set_theme` are not aimed
  at the production checkout.
- "Publish" means that person builds, reads the HTML, deploys, and
  treats `ci/purge.sh` as a stub until a real CDN purge replaces it.
- Readers can live with a static site: no accounts, no comments,
  contact as email, analytics absent or added at the host.
- Pages that hurt if wrong — privacy, terms, any legal claim — are
  edited by the human, in the file.

Under those conditions the output is dull in the right way: HTML, a
cache header, a feed, a sitemap, markdown for agents, a read-only tool
endpoint, drafts kept out of the live `dist/`. A marketing team, an
approver without a terminal, a second unattended writer, write tools on
`POST /mcp`, or a PR body treated as review before the preview shows
the draft, all sit outside that list. Showing the draft is a small
change and the difference between a gate and a ritual.

The read path is in good shape for 2027. The write path works as far as
one careful person. Keep the files, the build, the public read-only
MCP, and binaries in a bucket. Put the human in front of a queue that
shows the page. Until then, follow the conventions file with one
correction: confirmation happens in front of rendered HTML, and the
tools cannot skip it.
