# Zuban

Zuban is a high-performance Python Language Server and type checker implemented
in Rust, by the author of [Jedi](https://github.com/davidhalter/jedi).
Zuban is 20–200× faster than Mypy, while using roughly half the memory and CPU
compared to Ty and Pyrefly. It offers both a PyRight-like mode and a
Mypy-compatible mode, which behaves just like Mypy; supporting the same config
files, command-line flags, and error messages.

Most important LSP features are supported. Features include diagnostics,
completions, goto, references, rename, hover and document highlights.

Zuban passes over 95% of Mypy’s relevant test suite and offers comprehensive
support for Python's [type system](https://htmlpreview.github.io/?https://github.com/python/typing/blob/main/conformance/results/results.html).

- [Documentation](https://docs.zubanls.com)
- [Website](https://zubanls.com)
- [Releases](https://pypi.org/project/zuban/)

## Installation / Usage

```
pip install zuban  # Installation

zuban check   # PyRight-like checking
zuban mypy    # Mypy compatibility mode
zmypy         # An alias for zuban mypy
zuban server  # An LSP server
```

If you want Zuban to pick up your dependencies, please activate the virtual env first.

### Local Installation

You can install zuban **locally** by running:

```
pip install maturin
git clone --recursive https://github.com/zubanls/zuban
bash scripts/install-locally.sh
```

Note that your build will not properly work if submodules are not cloned.

### Faster test rebuilds on Windows

For a full, single-threaded development test run with less optimization during
compilation, run `./scripts/test-fast.ps1`. This runs the same Cargo test suite
with debug assertions and overflow checks enabled; it does not replace the normal
`cargo test`, `cargo fmt --check`, and `cargo check --all-targets` checks.
The first unoptimized build needs its own compiled artifacts, and the tests run
more slowly, so this mode is most useful when iterating on compiled Rust code.

On one Windows host, a warm compiled-code edit to the `zuban_python` library
test target rebuilt in 8.7 seconds with this setting versus 134.7 seconds with
the standard optimized test profile, using one Cargo job in both cases. These
are single measurements for one edit, not a promise for other changes. In one
complete run, the two unoptimized mypy-like passes took 28.9 and 19.1 seconds;
the corresponding optimized passes in another run took 7.5 and 9.3 seconds.

## License

This project is dual licensed:

- **Open Source License**: [GNU Affero General Public License v3.0](LICENSE) (AGPL-3.0).
  You may use, modify, and distribute this project under the terms of the AGPL-3.0.
- **Commercial License**: Available for organizations that prefer not to comply with the AGPL.
  Contact info (at) zubanls.com for commercial licensing options.
