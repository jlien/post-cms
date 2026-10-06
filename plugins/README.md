# Plugins: client-side snippets, injected at build time

A plugin is an analytics pixel, ad tag, or any other client-side snippet.
Static-first holds: the origin never serves or executes anything — the
build inlines the snippet into every rendered HTML page, so the bytes ride
the CDN like everything else.

## Anatomy

```
plugins/<name>/
  snippet.html     the raw snippet, mounted verbatim
  assets/          optional; copied to dist/assets/<name>/
```

## Declaration (site.yaml)

```yaml
plugins:
  - name: ga4
    mounts: [head]                # head (before </head>) / body_end (</body>)
    csp_script_src: ["https://www.googletagmanager.com"]
    require_consent: false        # true = snippet runs only after consent
```

## Rules the build enforces

- A declared plugin **must** have a snippet file — build fails otherwise.
- Every script host the snippet loads from must be in `csp_script_src`.
  The build computes one CSP meta tag from all plugins' domains; a plugin
  cannot phone home to an undeclared domain.
- `require_consent: true` wraps the snippet in a gate that only runs it
  after the visitor grants consent (cookie `pcms_consent=granted`, or
  dispatch a `pcms:grant` event from a consent banner).
- DRAFTS=1 builds (proposal review previews) never inject plugins —
  review traffic must not pollute analytics.

## How a plugin ships

A plugin is code, so it goes through the proposal flow: `propose_plugin`
writes the snippet + site.yaml entry on an `agent/plugin-<name>` branch
with `risk: tracking` — the PR diff IS the pixel being added, in plain
text. Approve ships it; `revert_proposal` un-ships it in one action.
