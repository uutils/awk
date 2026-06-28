<div align="center">

![uutils logo](https://raw.githubusercontent.com/uutils/coreutils/refs/heads/main/docs/src/logo.svg)

# uutils AWK

[![Discord](https://img.shields.io/badge/discord-join-7289DA.svg?logo=discord&longCache=true&style=flat)](https://discord.gg/wQVJbvJ)
[![License](http://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/uutils/awk/blob/main/LICENSE-MIT)
[![License](https://img.shields.io/badge/license-APACHE%202.0-orange.svg)](https://github.com/uutils/awk/blob/main/LICENSE-APACHE)
[![dependency status](https://deps.rs/repo/github/uutils/awk/status.svg)](https://deps.rs/repo/github/uutils/awk)

</div>

---

uutils AWK is a WIP, cross-platform reimplementation of GNU AWK (a.k.a. `gawk`) in
[Rust](http://www.rust-lang.org).

## Goals

uutils AWK aims to be a drop-in replacement for `gawk`. Differences with GNU
are treated as bugs.

Our key objectives include:
- Matching GNU's output (stdout and error code) exactly
- Better error messages
- Best-in-class memory safety
- Improved performance
- Providing comprehensive internationalization support (UTF-8, etc.)
- Extensions when relevant

uutils AWK aims to work on as many platforms as possible, to be able to use the same
utils on Linux, macOS, *BSD, Windows, WASI and other platforms. This ensures, for example,
that scripts can be easily transferred between platforms.

## Requirements

- Rust (`cargo`, `rustc`)

### Rust Version

uutils AWK follows Rust's release channels and is tested against stable, beta and
nightly. The minimum supported Rust version at the moment is the previous stable
version, that is, 1.95.0 at the time of writing.

## State of the Repo

Check out https://github.com/uutils/awk/issues/16.

## Testing

### GNU awk (gawk) Compatibility Testing

Track compatibility against GNU awk by running the upstream gawk testsuite
against our Rust binary. Rather than reimplement gawk's test harness, we drive
gawk's own (GPL) test Makefile with `make check AWK=<wrapper>`, where the wrapper
execs our `awk` — the gawk sources are fetched fresh at test time and never
copied into this repo.

```bash
# Fetch the gawk testsuite (one-time setup)
mkdir -p ../gnu.awk && (cd ../gnu.awk && bash ../awk/util/fetch-gnu.sh)

# Run compatibility tests
./util/run-gnu-testsuite.sh

# Verbose mode shows the diff for each failing test
./util/run-gnu-testsuite.sh -v

# Generate JSON results for CI
./util/run-gnu-testsuite.sh --json-output results.json
```

The harness builds our `awk`, runs gawk's `make check` with a wrapper named
`gawk`, and classifies each test the way gawk's own `pass-fail` target does: a
leftover `_<name>` file is a failure, its absence a pass, and tests that never
run (group-skipped because of missing locales, MPFR, or shared-library support)
are reported as skipped.

### Unit Tests

```bash
cargo test --workspace
```

## Contributing

To contribute to uutils AWK, please see [CONTRIBUTING](https://github.com/uutils/coreutils/blob/main/CONTRIBUTING.md).

## License

uutils AWK is licensed under either the MIT License or the Apache v2.0 License - see the `LICENSE-MIT`, `LICENSE-APACHE` files for details.

GNU AWK is licensed under the GPL 3.0 or later.
