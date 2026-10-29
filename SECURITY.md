# Security

## Dependency audit

Run both before changing `Cargo.lock`, and again after:

```bash
cargo audit                                  # RustSec advisory database
osv-scanner scan source --lockfile Cargo.lock
```

A vulnerability is fixed by upgrading the crate. Two unmaintained-crate
warnings remain and are accepted for now; the reasoning is below. Review them
whenever a dependency named here gets a release.

### Accepted: `unsafe-libyaml` (RUSTSEC-2024-0449, unmaintained)

- Pulled in by `serde_yaml` 0.9 (also deprecated), used by both crates to read
  `site.yaml`, `redirects.yaml`, collection schemas and front matter.
- Exposure: `build-site` and `server` parse YAML from files under the site
  root (`site.yaml`, `redirects.yaml`, schemas, each item's `meta.yaml`).
  Neither parses YAML from a request body or an upload. Two writers put
  outside text into those files before `build-site` reads them back:
  - CMS content. A Sanity source fetches documents as JSON and writes each
    document's fields to `<collection>/<slug>/meta.yaml` with
    `serde_yaml::to_string` (`crates/build/src/sanity.rs`); the same build
    then parses those files (`crates/build/src/main.rs`). CMS editors control
    the string contents, but the serializer emits them as quoted or escaped
    scalars, so libyaml receives YAML the engine generated, with editor text
    only inside scalar values. Editors cannot add keys, tags, anchors or
    documents; `materialize_escapes_cms_strings_in_meta_yaml` in
    `sanity.rs` checks this with strings shaped like YAML structure.
  - MCP tools. `mcp/server.py` writes `meta.yaml` from values the calling
    agent supplies, an agent that already has write access to the site
    repository. Scalars are JSON-quoted (`_yaml_value`); list items are
    written unquoted.
- Why not replace it now: the drop-in forks (`serde_yaml_ng`, `serde_norway`)
  still run the same c2rust translation of libyaml (`serde_yaml_ng` uses this
  very crate; `unsafe-libyaml-norway` has had no release since 2024-12), so
  they move the warning without removing the code it is about. The safe-Rust
  parsers the advisory suggests (`serde-saphyr`, `noyalib`) have no
  `serde_yaml::Value` equivalent, so switching touches every YAML call site in
  both crates and can change how existing sites parse (tags, anchors,
  duplicate keys, number and boolean forms). That belongs in its own change,
  checked by comparing parsed output over real site data.
- Replace `serde_yaml` (or revisit this note) when any of these holds:
  - YAML from outside the site repository reaches the parser as raw text
    (a request, an upload, a fetched `.yaml` file, a CMS field copied into
    YAML verbatim).
  - CMS-controlled or other outside strings are written into YAML without
    escaping, including through a new writer.
  - A RustSec advisory reports an actual bug in `unsafe-libyaml`.
  - The YAML layer is replaced for other reasons.

### Accepted: `paste` (RUSTSEC-2024-0436, unmaintained)

- A procedural macro: it runs only at compile time, generating identifiers,
  and ships no code in `build-site` or `server`.
- Pulled in transitively by `image` (through `exr` and `pulp`, and through
  `ravif` and `rav1e`), `rav1d`, and `fastembed` (through `tokenizers`).
  post-cms does not depend on it directly, and each of those is at its latest
  release compatible with our version requirements.
- Revisit when: those crates move to `pastey` or drop the macro; then
  `cargo update` removes it.
