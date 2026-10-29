# Experimental Blaze builds on Windows

This is an opt-in evaluation of Blaze for Zuban's edit/rebuild loop. A successful
Zuban build and two controlled edit/rebuild comparisons are recorded below.
Keep the normal Cargo format, check, and test gates: Blaze currently refuses
the custom `harness = false` tests used by this workspace.

The sandbox runtime's `wt/hot-dev-20261026` checkout pins PyPI Zuban 0.10.1
for its `python-boundary-types` proof lane and excludes dev dependencies from
its normal production image. It does not consume this local Blaze-built Zuban
executable. This evaluation improves the checker development loop only after
its executable and edit/rebuild parity have been proven; it is not an alpha
runtime latency result.

The evaluation uses upstream commit
`6a02fbfdc142007a6866b6bf00e93d77368cd33a` plus `windows-single-worker.patch`.
The patch makes Blaze's analysis, code generation, and build-script scheduling
honor a positive `CARGO_BUILD_JOBS`; it rejects invalid limits. At one worker,
edited-file checking and code generation run sequentially; name resolution runs
inline. The Windows engine gets a 16 MiB main-thread stack. It does not change
Zuban's code or replace correctness checks.

The patch also adds an optional native proc-macro server using the DLL loader
from [the pinned Rust toolchain source](https://github.com/rust-lang/rust/blob/16d1ffac17ad8b739e32483ac2e65fe7e1804977/src/tools/rust-analyzer/crates/proc-macro-srv/src/dylib.rs).
It fixes cross-file `Span::join` to return
`None`, as Rust specifies, while retaining valid same-file joins. Server choice
is part of the worker configuration, and a spare with another server is rejected.
The default Blaze macro loader remains available when the feature is disabled.

From the Zuban repository root, bootstrap a separate tool checkout once:

```powershell
git clone --recurse-submodules https://github.com/indexable-inc/blaze.git ..\blaze-eval
git -C ..\blaze-eval checkout 6a02fbfdc142007a6866b6bf00e93d77368cd33a
git -C ..\blaze-eval apply "$PWD\scripts\blaze\windows-single-worker.patch"
rustup toolchain install nightly-2026-10-27 --component rust-src --component rustc-dev --component rustfmt
```

In that tool checkout, build with `CARGO_BUILD_JOBS=1`:

```powershell
$env:CARGO_BUILD_JOBS = '1'
cargo +nightly-2026-10-27 build --locked -p blaze-engine --release
cargo +nightly-2026-10-27 build --locked --manifest-path vendor\proc-macro-srv\Cargo.toml -p proc-macro-srv-cli --features native-rustc-loader --release --target-dir target\proc-macro-srv-native
```

Then, back in Zuban:

```powershell
$blaze = '..\blaze-eval\target\release\blaze.exe'
$macroServer = '..\blaze-eval\target\proc-macro-srv-native\release\rust-analyzer-proc-macro-srv.exe'
.\scripts\blaze-dev.ps1 -BlazePath $blaze -NativeProcMacroServer $macroServer -CargoArgs @('--locked', '-p', 'zuban')
.\scripts\blaze-dev.ps1 -BlazePath $blaze -NativeProcMacroServer $macroServer -Command Run -CargoArgs @('--locked', '-p', 'zuban', '--', '--version')
.\scripts\blaze-dev.ps1 -BlazePath ..\blaze-eval\target\release\blaze.exe -Command Stop
```

The wrapper confines child processes to one allowed logical CPU, sets the compile
worker limit to one, sets `RUST_MIN_STACK=16777216` for spawned compiler threads,
and restores the caller's environment, location, and CPU
affinity. Blaze still uses a client, daemon, and worker processes. Its home and
build output stay under the ignored `target` directory. Windows requires the
explicit `BLAZE_SANDBOX=off` setting for build scripts and macros; the wrapper
scopes it to the invocation. A first invocation may download Blaze's toolchain.

This build uses Clang to assemble inline assembly on Windows. Build/Run look for
`clang.exe` on PATH and then under `Program Files\LLVM\bin`. Use `-ClangPath`
on the wrapper or comparison script to select another installation. Its directory
is added only to the invocation's PATH and restored afterward. Stop an existing
Blaze daemon before changing the assembler path so its workers inherit the change.

The wrapper explicitly sets `BLAZE_PROC_MACRO_SERVER=0`: Cargo/rustc builds the
real compile-time dependencies and macros, while Blaze compiles the main program.
This hybrid evaluation still uses rustc. The evaluated WASM
macro path refused `getrandom::imp::getrandom_inner`, and native expansion through
Blaze requires its own macro server, absent from the initial Windows build.
`-NativeProcMacroServer` selects the patched native DLL server through the scoped
`BLAZE_NATIVE_PROC_MACRO_SERVER` setting. It is required for Build/Run and is not
needed for Status/Stop. The installed rustup server is not modified. The hybrid
mode runs native macros rather than substituting deterministic randomness stubs.
The real workspace's macro expansion, executable build, focused diagnostic
comparison, and controlled edit/rebuild comparisons have passed.

## Current evidence

| Check | Result |
| --- | --- |
| Initial patched Blaze source bootstrap | Passed; 4,090.5 seconds, one Cargo job |
| Worker-limit regression | Passed |
| First Zuban build through Blaze | Failed after 400.692 seconds: native span assertion and worker stack overflow |
| Patched native server build and cross-file/same-file join regression | Passed |
| Patched server loading/listing real Rust-built `serde_derive` macros | Passed |
| Native `Serialize` expansion for `struct Probe;` using that DLL | Passed |
| Native `make_varule` expansion using Zuban's `zerovec_derive` DLL | Passed in legacy JSON protocol |
| Follow-up engine build with stack and native-server fixes | Found four missed configuration-error callers after 1,737.955 seconds; fixed |
| Engine retry with all eight configuration callers audited | Passed in 232.858 seconds; completed dependencies reused |
| Rebuilt Windows engine PE stack reserve | Verified: 16,777,216 bytes |
| Zuban workspace resolution and native macro expansion | 211 crates resolved in 29.4 seconds; 1,051 macros expanded in 2.92 seconds without the original crash |
| First build with patched native server | Failed after 625.674 seconds: 17 reachable functions needed Clang, absent from PATH |
| Build with scoped Clang discovery | Failed after 371.733 seconds on 138 borrow diagnostics; assembler issue cleared |
| Application code generation in that run | All 10,477 functions compiled (+98,140 instances), 254.8 seconds; not a validated executable |
| Cargo check using Blaze's exact nightly | Passed in 221.733 seconds; the 138 Blaze diagnostics are compiler divergences |
| Reduced mutable-cast regression | Pinned Cargo accepts all four cases; baseline Blaze emits four E0596 errors |
| Reduced grammar-promotion regression | Pinned Cargo accepts the actual separator macro pattern; baseline Blaze emits E0716 |
| Reduced nested-iterator lifetime regression | Pinned nightly accepts both recursive-argument and explicit-bound variants; baseline Blaze rejects them |
| Rebuilt engine differential regression | Seven real-file cases passed: four valid, three rejected controls; one process |
| IDE diagnostic regression suite | 795 passed, one ignored, one test thread |
| Full Zuban build through patched Blaze | Passed in 916.55 seconds on the contended host; `target\blaze-dev\debug\zuban.exe` produced |
| Cargo/Blaze CLI diagnostic smoke | Four cases matched exit codes and diagnostics |
| Controlled edit/rebuild comparison, run 1 | Same compiled diagnostic-literal edit: Cargo 21.192 s, Blaze 2.131 s (9.94x); revert: Cargo 15.989 s, Blaze 1.101 s (14.52x) |
| Controlled edit/rebuild comparison, run 2 | Same compiled diagnostic-literal edit: Cargo 23.155 s, Blaze 1.696 s (13.65x); revert: Cargo 16.946 s, Blaze 1.147 s (14.77x) |
| Separate warm baseline | Cargo 1.852/24.162 s; Blaze 275.424/318.860 s after daemon restart. Excluded from edit/rebuild ratios |

The bootstrap, failed first build, and warm baselines are setup measurements,
not edit/rebuild benchmarks. Both comparison runs used one allowed logical CPU
and one Cargo/Blaze compile worker, checked changed executable output and the
16,000,000-byte PE stack reserve, and restored the edited source byte-for-byte.
The separate result directories are
`target/blaze-evaluation/20261029-124834-21817dd7855a486593de890a4c82f44b`
and `target/blaze-evaluation/20261029-125416-be59d608b6e04fb29d90ea55b65be21b`.
This measures one small compiled edit in an already built program; it does not
establish first-build throughput, broad edit speedups, or sandbox alpha latency.
The normal Cargo format/check/test gates remain authoritative.

The follow-up patch addresses mutable coercion casts, promotion of immutable
constructor values inside macro-generated blocks, and declared return-type bounds
around nested opaque iterators. Hidden opaque types remain obligations rather
than supplying assumed bounds. The rebuilt engine passed the reduced differential
cases and the full Zuban build. The mutability pass runs after deferred cast checks;
missing cast types remain non-mutable rather than panicking during that walk.
Assembler identity now includes availability, backend, selected command/arguments,
and the tool's contents in both assembly and standard-library cache keys. This
prevents a new worker with Clang from reusing a partial standard-library unit made
without it. Tool identity is computed once per worker; restart workers after
changing installed tools. `BLAZE_ASSEMBLER` is part of build and worker identity,
so a reused daemon cannot silently retain a different requested assembler mode.
The full build exercises the current cache identity; explicit tool-toggle
validation of this cache change is still pending.

After a successful baseline Blaze build, run the diagnostic smoke before the
controlled rebuild comparison:

```powershell
.\scripts\blaze\diagnostic-smoke.ps1
.\scripts\blaze\compare.ps1 -BlazePath $blaze -NativeProcMacroServer $macroServer
```

Each script writes evidence to a unique `target\blaze-evaluation` directory.
The diagnostic smoke compares four small cases against the Cargo executable;
it supplements the full Cargo test suite. The comparison requires a clean
`crates/zuban_python/src/lib.rs`, saves its original bytes, changes a compiled
diagnostic string, and restores the bytes unless another process changed them.

For comparisons, stop Blaze during Cargo measurements, then warm Blaze separately.
Start timing before each tool's own source edit, change the same function body, verify that the
new behavior executes, and measure the same binary/profile on one logical CPU.
Check the resulting PE stack reserve is still 16,000,000 bytes. Do not compare a
cold Cargo build with a warm Blaze rebuild or call a comment-only edit a codegen
speedup. Report a new edit and a revert separately. The Windows worker does not
perform unrequested base rebuilds; verify its warm build has completed before
measuring an edit.

Upstream references: [platforms](https://github.com/indexable-inc/blaze/blob/6a02fbfdc142007a6866b6bf00e93d77368cd33a/docs/platforms.md),
[Cargo compatibility](https://github.com/indexable-inc/blaze/blob/6a02fbfdc142007a6866b6bf00e93d77368cd33a/docs/cargo.md).
