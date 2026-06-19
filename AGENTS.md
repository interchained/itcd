# AGENTS.md — itcd Agent Handoff Notes

This file is for Claude, Hyperagent, and any future coding agents working in `Eth-Interchained/itcd`.

It captures what was learned while fixing the Linux portable glibc build for the NEDB-backed ITC node. Read this before changing the NEDB FFI, `src/Makefile.am`, or `.github/workflows/linux-static.yml`.

## Current status

As of PR #9, the portable glibc build passes with NEDB FFI linked into the final binaries.

Merged fix:

- PR: `https://github.com/Eth-Interchained/itcd/pull/9`
- Merge SHA: `9754e64839196be0ed47be17a8b798cdd2040972`
- Final successful pre-merge commit: `4731031467e0d15afc106937277100ab928586be`
- Passing workflow: `Linux Build (portable glibc) — wallet + miniupnpc + zmq`
- Passing job: `glibc-portable • x86_64`

## What this repo is doing

`itcd` replaces the Bitcoin-style LevelDB backend with NEDB through a Rust static library exposed over a C FFI:

- C++ node storage wrappers call declarations from `nedb-ffi/nedb.h`.
- Rust implements the exported functions in `nedb-ffi/src/lib.rs` using `#[no_mangle] pub extern "C" fn ...`.
- The Rust crate builds `nedb-ffi/target/release/libnedb_ffi.a`.
- The portable glibc workflow then links that static archive into NEDB-consuming final binaries such as `interchainedd` and `interchained-wallet`.

The hard part is not compiling Rust. The hard part is getting Automake, libtool, GNU ld, C++ name linkage, and Rust native dependencies to agree during the final executable link.

## The failure chain we already solved

Do not restart from old assumptions. The failures below happened in order, and each revealed the next real blocker.

### 1. Rust archive contained LLVM bitcode wrappers

Symptom:

- Archive looked large enough, but GNU ld could not consume the extracted objects correctly.
- The root cause was release `lto=true`, which caused Rust to embed bitcode in a way that broke the portable static link path.

Fix pattern in CI:

- Patch `nedb-ffi/Cargo.toml` at runtime from `lto = true` to `lto = false`.
- Build with `RUSTFLAGS="-C embed-bitcode=no"`.
- Verify extracted archive members are native ELF and not LLVM bitcode.
- Verify the archive exports real `T nedb_*` symbols with `nm`.

Keep these checks. They fail early and save time.

### 2. Hand-rolled `.la` wrapper was not accepted by libtool

Symptom:

```text
libtool: error: ... libnedb_ffi.la is not a valid libtool archive
```

Lesson:

- libtool expects more than `old_library='libnedb_ffi.a'`.
- It also expects a normal libtool archive shape and marker comments.

Fix:

- Generate a libtool-like `libnedb_ffi.la` with the standard header comments and fields:
  - `dlname`
  - `library_names`
  - `old_library`
  - `inherited_linker_flags`
  - `dependency_libs`
  - `current`, `age`, `revision`
  - `installed`, `shouldnotlink`
  - `dlopen`, `dlpreopen`
  - `libdir`

Do not simplify the `.la` file back down to a minimal two-line wrapper.

### 3. Final link still did not resolve `nedb_*`

Symptom:

```text
undefined reference to `nedb_get(...)'
undefined reference to `nedb_free_value(...)'
undefined reference to `nedb_iter_* (...)'
```

Lesson:

- GNU ld resolves static archives left to right.
- Automake and libtool can reorder `.la` old libraries or place dependency libraries somewhere other than expected.
- Merely having `$(NEDB_FFI_LA)` in `LDADD` was not sufficient for this build.

Fix:

- The CI workflow rewrites generated `src/Makefile` after `./configure`.
- It groups the NEDB-consuming final executable links with:

```text
-Wl,--start-group ... /absolute/path/to/libnedb_ffi.a ... -Wl,--end-group
```

- This applies to:
  - `interchainedd_LDADD`
  - `interchained_node_LDADD`
  - `interchained_wallet_LDADD`

Do not remove the group-link rewrite unless you replace it with a tested source-level Makefile solution that preserves final executable link order.

### 4. Wrong archive path inside generated Makefile

Symptom:

```text
g++: error: /__w/itcd/itcd/../nedb-ffi/target/release/libnedb_ffi.a: No such file or directory
```

Lesson:

- `src/Makefile` paths are easy to get wrong because the make process runs inside `src` but path expansion may happen relative to the top build directory.

Fix:

- Compute and export the absolute archive path in the workflow:

```sh
export NEDB_LIB="$(pwd)/nedb-ffi/target/release/libnedb_ffi.a"
test -f "$NEDB_LIB"
```

- Pass that exact already-validated path into the embedded Python Makefile rewrite through `os.environ['NEDB_LIB']`.

Do not reconstruct this path manually with `../nedb-ffi`.

### 5. C++ callers referenced mangled FFI symbols

Symptom:

```text
undefined reference to `nedb_get(NedbHandle*, unsigned char const*, unsigned long, unsigned char**, unsigned long*)'
```

That C++-typed signature means C++ saw a declaration with C++ linkage. Rust exported plain unmangled C ABI symbols.

Important subtlety:

- The committed `nedb-ffi/nedb.h` already had an `extern "C"` guard.
- But `nedb-ffi/build.rs` regenerates `nedb.h` during the Cargo build.
- cbindgen defaults `cpp_compat = false`, so the generated header could drop the C++ guard before C++ compilation.

Fix:

```rust
cbindgen::Builder::new()
    .with_crate(crate_dir)
    .with_language(cbindgen::Language::C)
    .with_cpp_compat(true)
    .with_include_guard("NEDB_FFI_H")
    .with_documentation(true)
    .generate()
```

The workflow also checks:

```sh
grep -q 'extern "C"' nedb.h || {
  echo "ERROR: generated nedb.h lacks extern C guard"
  exit 1
}
```

Keep this assertion. If it fails, fix `build.rs` or cbindgen configuration, not the C++ call sites.

### 6. Rust native system libraries were missing

Symptom:

```text
/usr/bin/ld: ... libnedb_ffi.a(...std...rcgu.o): undefined reference to symbol 'dlsym@@GLIBC_2.2.5'
/usr/bin/ld: /lib/x86_64-linux-gnu/libdl.so.2: error adding symbols: DSO missing from command line
```

This was good news. It proved ld was finally reading `libnedb_ffi.a`.

Fix:

- Add Rust native system libraries after the Rust archive in the final link group:

```text
libnedb_ffi.a -ldl -lpthread -lm
```

If a future Rust dependency introduces another native library, add it there, after confirming with the linker error and keeping the dependency in the same final-link group.

### 7. Codemagic macOS x86_64 wallet configure could not find BDB headers

Symptom:

```text
configure:34155: error: libdb_cxx headers missing, Interchained Core requires this library for wallet functionality (--disable-wallet to disable wallet functionality)
g++ ... -I/Users/builder/clone/db4/include ... conftest.cpp
conftest.cpp:67:18: fatal error: 'db_cxx.h' file not found
```

Important subtlety:

- The x86_64 Codemagic job builds Berkeley DB 4.8 under Rosetta and points configure at `$CM_BUILD_DIR/db4/include`.
- The first visible Interchained configure failure had the correct `-I` flag, so the issue was not lost `CPPFLAGS`.
- A follow-up Codemagic run showed the earlier root cause: Berkeley DB configure selected `UNIX/fcntl`, warned `NO SHARED LATCH IMPLEMENTATION FOUND FOR THIS PLATFORM`, and exited with `Unable to find a mutex implementation` before any usable install happened.

Fix pattern in `codemagic.yaml`:

- In the Rosetta BDB configure call, force the POSIX pthread backend with `--enable-posixmutexes --with-mutex=POSIX/pthreads/library`.
- Wrap BDB configure with `|| { cat config.log; exit 1; }` so a failed configure cannot be hidden by later `make` and header-copy noise.
- After `arch -x86_64 make install`, explicitly ensure `$CM_BUILD_DIR/db4/include` exists.
- Stage `db.h` and `db_cxx.h` from the BDB build/source tree into that include directory if install omitted them.
- Fail early with a directory listing if `db_cxx.h` or `libdb_cxx-4.8.a` is still missing.

Do not respond to this failure by disabling wallet support; the macOS artifacts are intended to ship wallet-enabled binaries.

### 8. Berkeley DB 4.8 atomic macros collided with modern Apple libc++

Symptom:

```text
/Applications/Xcode.app/.../include/c++/v1/atomic:...: error: expected unqualified-id
#define atomic_init(p, val) ((p)->value = (val))
```

Important subtlety:

- Berkeley DB 4.8 predates modern Apple Clang/libc++ and defines legacy atomic helper names in the global macro namespace.
- The workflow already renamed `__atomic_compare_exchange`, but Xcode 26 also trips over BDB's `atomic_init(...)` macro because libc++ declares a real `atomic_init` in `<atomic>`.
- Patch the downloaded BDB source before configuring or compiling it. Do not patch Interchained C++ sources to route around this vendor macro.

Fix pattern in `codemagic.yaml`:

- After cloning `/tmp/bdb-src`, run a source-wide Perl rewrite across BDB headers and C/C++ sources.
- Rename `__atomic_compare_exchange` to `__db_atomic_compare_exchange`.
- Rename every `atomic_init(` call/macro to `db_atomic_init(`.
- Fail early if a `#define atomic_init` line survives under `/tmp/bdb-src`.

## Current portable glibc link strategy

The working strategy is layered:

1. Build Rust staticlib as native ELF, no embedded bitcode.
2. Generate and keep a libtool-compatible `.la` wrapper because `src/Makefile.am` references `$(NEDB_FFI_LA)`.
3. Verify C ABI header generation includes `extern "C"`.
4. After `./configure`, rewrite generated `src/Makefile` for final app links only.
5. Use absolute `NEDB_LIB` in the rewrite.
6. Wrap final app archives, NEDB archive, and Rust native syslibs in a GNU ld group.

This is intentionally CI-scoped because the portable packaging workflow is the place where we know the Rust archive has already been built and validated.

## Guidelines for future agents

### Do

- Work on a feature branch and open a PR.
- Preserve all early validation checks in `.github/workflows/linux-static.yml`.
- Treat each new linker error as a clue about the next layer, not proof the previous layer failed.
- Use `nm` to distinguish missing archive, missing symbol, ABI mismatch, and native dependency issues.
- Keep final executable link fixes scoped to NEDB-consuming binaries.
- Prefer small PRs with clear commit messages.
- Wait for CI before merging.

### Do not

- Do not force-push.
- Do not commit directly to `main`.
- Do not publish packages unless explicitly instructed.
- Do not remove the `.la` wrapper because it looks redundant.
- Do not remove `with_cpp_compat(true)` from `nedb-ffi/build.rs`.
- Do not use `../nedb-ffi/...` for the final archive path in generated `src/Makefile`.
- Do not assume `undefined reference to nedb_get` always means archive ordering. Check whether the symbol is C++-mangled, whether the archive is present, and whether ld is actually reading it.

## Quick diagnosis table

| Log signature | Likely meaning | First place to inspect |
| --- | --- | --- |
| `not a valid libtool archive` | `.la` wrapper malformed | `.github/workflows/linux-static.yml` `.la` generation |
| `No such file or directory ... libnedb_ffi.a` | Wrong final archive path | `NEDB_LIB` export and generated Makefile rewrite |
| `undefined reference to nedb_get(NedbHandle*, ...)` | C++ mangled caller reference | `nedb-ffi/build.rs` cbindgen `with_cpp_compat(true)` |
| `libnedb_ffi.a(...): undefined reference to dlsym` | Rust archive is linked, but native syslib missing | Rust syslibs in final link group |
| `libdb_cxx headers missing` with `-I.../db4/include` present | BDB install prefix lacks `db_cxx.h` | `codemagic.yaml` BDB header staging/checks |
| `#define atomic_init(p, val)` near libc++ `<atomic>` errors | BDB legacy atomic macro collided with Apple libc++ | `codemagic.yaml` BDB source rewrite before configure |
| `no nedb_* T symbols` | Rust archive did not export expected FFI | `nedb-ffi/src/lib.rs`, Rust build flags, LTO/bitcode checks |
| LLVM bitcode magic in extracted objects | Rust archive unusable by GNU ld path | `lto=false`, `RUSTFLAGS=-C embed-bitcode=no` |

## Suggested next cleanup

The current CI fix is proven. A future cleanup PR can move more of the link behavior from workflow-generated Makefile rewrites into `src/Makefile.am`, but only if CI proves it still preserves these properties:

- final app link order is correct;
- `libnedb_ffi.a` appears after archives that reference `nedb_*`;
- Rust native syslibs appear after `libnedb_ffi.a`;
- generated `nedb.h` keeps C++ compatibility;
- portable glibc packaging still passes.

Do not attempt that cleanup in the same PR as unrelated behavior changes.
