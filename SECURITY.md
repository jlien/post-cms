# Security

## Dependency audit

Run both before changing `Cargo.lock`, and again after:

```bash
cargo audit                                  # RustSec advisory database
osv-scanner scan source --lockfile Cargo.lock
```

A vulnerability is fixed by upgrading the crate. One unmaintained-crate
warning remains and is accepted for now; the reasoning is below. Review it
whenever a dependency named here gets a release.

### YAML parser: `yaml_serde`

- `serde_yaml` 0.9 is deprecated and pulled in `unsafe-libyaml`
  (RUSTSEC-2024-0449, unmaintained). The workspace now uses `yaml_serde`, the
  YAML Organization's maintained continuation of `serde_yaml`, with its
  maintained `libyaml-rs` parser. The workspace dependency keeps the name
  `serde_yaml` (`package = "yaml_serde"`), so call sites are unchanged.
- It is the same libyaml translation with the same `Value` API, so existing
  sites parse the same way; the switch was checked by building a real site
  with the old and new engine and comparing `dist/` byte for byte.
- Exposure is unchanged: `build-site` and `server` parse YAML only from files
  under the site root (`site.yaml`, `redirects.yaml`, schemas, each item's
  `meta.yaml`), never from a request body or an upload. A Sanity source
  writes CMS fields into `meta.yaml` with `serde_yaml::to_string`
  (`crates/build/src/sanity.rs`), which emits editor text only inside quoted
  or escaped scalars; `materialize_escapes_cms_strings_in_meta_yaml` checks
  this. `mcp/server.py` writes `meta.yaml` from values an agent with write
  access to the site repository supplies.
- Revisit if YAML from outside the site repository reaches the parser as raw
  text, if outside strings are written into YAML without escaping, or if an
  advisory names `yaml_serde` or `libyaml-rs`.

### Accepted: `paste` (RUSTSEC-2024-0436, unmaintained)

- A procedural macro: it runs only at compile time, generating identifiers,
  and ships no code in `build-site` or `server`.
- Pulled in transitively by `image` (through `exr` and `pulp`, and through
  `ravif` and `rav1e`), `rav1d`, and `fastembed` (through `tokenizers`).
  post-cms does not depend on it directly. The latest releases of `pulp`,
  `rav1e`, `rav1d` and `tokenizers` all still require `paste`, so no upgrade
  removes it, and `tokenizers` is required by `fastembed` (semantic search).
- Revisit when: those crates move to `pastey` or drop the macro; then
  `cargo update` removes it.
