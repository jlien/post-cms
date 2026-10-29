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
- Exposure: the YAML is read from files in the site root at build time and
  at server start. post-cms never parses YAML from a request body or an
  upload. Two paths carry other values into YAML: a Sanity source writes
  each document's fields to `meta.yaml` through `serde_yaml`'s serializer,
  and the MCP tools (`mcp/server.py`) write `meta.yaml` from values the
  calling agent supplies, an agent that already has write access to the site
  repository.
- Why not replace it now: the drop-in forks (`serde_yaml_ng`, `serde_norway`)
  still run the same c2rust translation of libyaml (`serde_yaml_ng` uses this
  very crate; `unsafe-libyaml-norway` has had no release since 2024-12), so
  they move the warning without removing the code it is about. The safe-Rust
  parsers the advisory suggests (`serde-saphyr`, `noyalib`) have no
  `serde_yaml::Value` equivalent, so switching touches every YAML call site in
  both crates and can change how existing sites parse (tags, anchors,
  duplicate keys, number and boolean forms). That belongs in its own change,
  checked by comparing parsed output over real site data.
- Revisit when: a site starts accepting YAML from outside its repository, a
  RustSec advisory reports an actual bug in `unsafe-libyaml`, or the YAML
  layer is replaced.

### Accepted: `paste` (RUSTSEC-2024-0436, unmaintained)

- A procedural macro: it runs only at compile time, generating identifiers,
  and ships no code in `build-site` or `server`.
- Pulled in transitively by `image` (through `exr` and `pulp`, and through
  `ravif` and `rav1e`), `rav1d`, and `fastembed` (through `tokenizers`).
  post-cms does not depend on it directly, and each of those is at its latest
  release compatible with our version requirements.
- Revisit when: those crates move to `pastey` or drop the macro; then
  `cargo update` removes it.
