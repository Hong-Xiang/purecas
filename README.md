# purecas

Content-addressable storage for large binary datasets and model weights.

purecas is **filesystem-first**: you organize content as an ordinary
directory hierarchy using whatever tools you already use (`cp`, `mkdir`,
`rsync`, Nix, dataloaders, etc.), and `pcas index` builds an immutable,
content-addressed lookup index alongside it using hard links. There is no
copy step and no database standing between you and your files.

> An older, database-backed design (`<root>/sha256/...` plus a
> `purecas.db` SQLite file) is being phased out. See
> [Legacy surfaces and the transition](#legacy-surfaces-and-the-transition)
> for what still uses it and why it cannot be mixed with the model below.

## Installation

### With Nix (recommended)

```bash
# Run directly
nix run github:Hong-Xiang/purecas#pcas -- --help

# Install into profile
nix profile install github:Hong-Xiang/purecas#pcas

# Or add to a flake as an input (see "Building hierarchy with Nix" below)
```

### From source

```bash
git clone git@github.com:Hong-Xiang/purecas.git
cd purecas
nix develop   # enter dev shell with Rust toolchain
cargo build --release
# binary at target/release/pcas
```

## Configuration

The CAS root directory is resolved in this order:

1. `--root <path>` flag (per-command override)
2. `$CAS_ROOT` environment variable
3. `~/data/blob` (default)

Design notes and this README sometimes write `PCAS_ROOT` as shorthand for
"whichever root the command above resolves to." It is prose notation only
— there is no `PCAS_ROOT` environment variable. The real, implemented
resolution is `--root`, then `CAS_ROOT`, then the `~/data/blob` default.

## The filesystem-first model

```text
<root>/                        # resolved as described above
  datasets/
    dataset=kinetics/
      split=train/
        video-001.mp4          # ordinary file, organize however you like
  models/
    model=resnet50/
      weights.safetensors
  .pcas/
    sha256/
      <first 2 hex chars>/
        <64-hex-digest>--<UTC index time>[.<suffix>]
    index.lock
    tmp/
```

- **Visible tree** — everything under the root except `.pcas`. These are
  ordinary files: read them, copy them, feed them to a dataloader, keep
  them under version control metadata, whatever you'd do with any other
  file. Directory structure and naming are yours to choose; purecas has
  no opinion about it.
- **`.pcas/sha256/<first2>/<digest>--<index-time>[.<suffix>]`** — exactly
  one entry per indexed SHA-256 digest. This entry is a **hard link**,
  not a copy: it shares an inode (and therefore bytes, ownership,
  permissions, and timestamps) with every visible file indexed to that
  digest. `<index-time>` is a UTC timestamp recording when the content
  was first hashed to this digest, not a filesystem ctime/mtime.
  `<suffix>` is an optional, sanitized, best-effort extension hint taken
  from the first visible filename indexed for that digest.
- **`.pcas/index.lock`** — an exclusive advisory lock held for the
  duration of a `pcas index` run so two indexing processes can't race.
- **`.pcas/tmp/`** — scratch space used internally for atomic
  rename-based deduplication.
- **`.pcas` is entirely derived state.** Deleting it loses lookup
  acceleration, never visible content; the next `pcas index` rebuilds it.
- **Indexed content is immutable by contract.** Because a digest name and
  every deduplicated visible path share one inode, editing a file in
  place changes every link to it simultaneously and invalidates the old
  digest name. purecas treats in-place mutation as exceptional damage,
  not a supported workflow — replace a file via write-to-temp-then-rename
  and re-run `pcas index` instead.

## Core workflow: build hierarchy, then index

Use ordinary filesystem tools to create the layout you want, then index it:

```bash
mkdir -p ~/data/blob/datasets/train
cp video-001.mp4 ~/data/blob/datasets/train/
pcas index
```

```text
a3f2c1dead...  datasets/train/video-001.mp4  /home/user/data/blob/.pcas/sha256/a3/a3f2c1dead...--20260722T130016.139Z.mp4
indexed=1 reused=0 deduplicated=0 repaired=0 pruned=0 failed=0
```

### `pcas index`

```bash
pcas [--root ROOT] index [PATTERN] [--rehash]
```

`pcas index` is the single reconciliation command: it discovers regular
files under the root, hashes and deduplicates their content onto one
hard-linked object entry per distinct SHA-256 digest, repairs object
entries whose bytes changed, and prunes entries with no remaining hard
link. It never reads or creates `purecas.db`.

- Omit `PATTERN` to index every regular file in the visible tree.
- A pattern without `/` (e.g. `*.mp4`) matches basenames recursively.
- A pattern containing `/` (e.g. `datasets/train/*.bin`) matches paths
  relative to the root.
- `.pcas` is always excluded. Symlinks, directories, sockets, devices,
  and FIFOs are never indexed, and symlinks are not followed.

```bash
pcas index                          # index everything
pcas index '*.mp4'                  # index matching basenames, recursively
pcas index 'datasets/train/*.bin'   # index a root-relative pattern
pcas index --rehash                 # force full content verification
```

The command takes an exclusive, non-blocking lock on `.pcas/index.lock`
for its entire run; a concurrent `pcas index` fails immediately instead of
racing. It prints one line per newly created object entry
(`<digest>  <relative-path>  <object-path>`), then a summary:

```text
indexed=1 reused=2 deduplicated=1 repaired=0 pruned=0 failed=0
```

Any independent per-path failure (an unreadable or unstable file, a
failed link/rename/verify, or a failed prune) is printed to stderr;
`pcas index` still processes every other path, but exits non-zero
whenever `failed > 0`.

**Non-adversarial mtime caveat.** To avoid rehashing unchanged content on
every run, an already-indexed file is trusted without hashing when its
mtime is not later than the time it was indexed. This is intentionally
not adversarial: a tool that preserves or backdates mtime across a
content change (`cp -p`, `rsync -a`, some archive extractors) can defeat
it silently. Run `pcas index --rehash` after using such a tool, or
whenever you need a guaranteed full verification — `--rehash` hashes
every matched visible file and every retained object entry, bypassing the
mtime fast path.

**Legacy root refusal.** `pcas index` refuses to run against any root
containing a top-level `purecas.db` (file, directory, or symlink),
checked before `.pcas` is created or any content is touched:

```text
Error: legacy SQLite store detected at /home/user/data/blob/purecas.db; migrate it or use a separate root before running `pcas index`
```

This exists because a legacy command can later rewrite `purecas.db` in
place; if it had already been hard-linked into `.pcas` as ordinary
content, that write would silently corrupt an entry served under an
immutable digest identity. See
[Legacy surfaces and the transition](#legacy-surfaces-and-the-transition).
A top-level `sha256/` directory alone is *not* rejected — it can be
legitimate visible hierarchy that has nothing to do with the legacy
layout.

### `pcas path`

```bash
pcas [--root ROOT] path <digest>
```

Resolves a hex SHA-256 digest to its packed `.pcas` object path. The
content must already have been indexed with `pcas index`; an unknown
digest fails.

```bash
pcas path a3f2c1dead...
# /home/user/data/blob/.pcas/sha256/a3/a3f2c1dead...--20260722T130016.139Z.mp4

cat "$(pcas path a3f2c1dead...)" > restored_file.mp4
```

### `pcas serve`

```bash
pcas [--root ROOT] serve [--bind ADDRESS] [--allow-ingest]
```

Exposes the visible hierarchy and immutable digest access over HTTP. The
server is read-only by default and binds to `127.0.0.1:8000` (loopback-only,
because it has no authentication or TLS):

```bash
pcas serve
# pcas serve: listening on http://127.0.0.1:8000

pcas --root /data/models serve --bind 0.0.0.0:9000
```

```bash
curl http://127.0.0.1:8000/datasets/train/video-001.mp4   # hierarchy route
curl http://127.0.0.1:8000/datasets/train/                # directory listing
curl http://127.0.0.1:8000/pcas/<64-hex-sha256>            # digest route
```

`serve` never opens or creates `purecas.db`, and never exposes it if it
already exists at the root: a top-level `purecas.db` is not served over
the hierarchy route. `.pcas` itself is never served either; a directory
that happens to be literally named `pcas` (not `.pcas`) elsewhere in the
tree remains visible.

Both routes open the resolved file exactly once, so representation
metadata and body bytes always come from the same descriptor. `GET`/`HEAD`
on either route return full representation metadata (`Content-Length`,
`Content-Type`, `Last-Modified`, `Accept-Ranges`) and support RFC 9110
conditional requests (`If-Match`, `If-Unmodified-Since`, `If-None-Match`,
`If-Modified-Since`) and RFC 9110 byte-range requests (`Range`,
`If-Range`), including single-range and `multipart/byteranges` responses.
Unsupported methods return `405 Method Not Allowed` with
`Allow: GET, HEAD`.

#### Opt-in HTTP ingestion

`--allow-ingest` additionally accepts a raw request body at an exact visible
destination:

```bash
pcas --root /data/models serve --allow-ingest

curl --fail-with-body \
  --data-binary @weights.safetensors \
  http://127.0.0.1:8000/models/resnet50/weights.safetensors
```

**Security warning:** `pcas serve` provides no authentication or TLS.
Write-enabled mode should bind only to a trusted interface, or run behind an
authenticated TLS proxy. Do not expose `--allow-ingest` directly to an
untrusted network.

**Threat model:** HTTP clients are untrusted. The local operator, repository
filesystem, and same-UID local processes are trusted; defending against a
malicious local co-owner racing filesystem entries during a transaction is
out of scope. Pre-existing symlink/path escapes and accidental replacement
are still rejected. Normal concurrent uploads and `pcas index`/`pcas serve`
operations coordinate through purecas's index lock.

- `POST /<visible/root-relative/path>` streams the raw body to that exact
  destination. It is not multipart. Missing parent directories are created.
  The body is processed chunk by chunk rather than accumulated in memory.
- Publication is create-only: an existing file returns `409 Conflict`; there
  is no overwrite, delete, resumable-upload, or query-controlled mode.
- The body is streamed into a temporary inode under `.pcas/ingest-tmp`,
  hashed, and synced before taking the global index lock. Per-upload advisory
  lock sidecars distinguish active streams from crash leftovers; writable
  server startup and later uploads reclaim stale temporary links.
- The same path rules as the hierarchy route apply. Traversal, malformed
  encoding, root escapes, directories, symlinked parent components,
  top-level `.pcas`/`purecas.db`, and the reserved top-level `pcas` digest
  namespace cannot be upload destinations.
- The server waits at most one second for `.pcas/index.lock`. A timeout
  returns `503 Service Unavailable` with `Retry-After: 1`; no visible file is
  published, so the same POST can be retried.
- While holding the lock, the server reconciles exactly the temporary inode
  into the canonical object index, then atomically publishes the visible path
  with create-only semantics. It does not discover unrelated visible files,
  prune globally, invoke a subprocess, or open/create `purecas.db`.
- Temporary, parent, internal-index, and destination access is
  descriptor-relative and does not follow symlinks. Source and destination
  must be on the same filesystem; a cross-device destination returns an
  explicit `409 Conflict` without copying.
- Body, write, lock, index, cross-device, and publication failures leave no
  temporary or new visible file. There is no visible-but-unindexed window.
- Success is `201 Created` with `Location` set to
  `/pcas/<lowercase-sha256>`, a strong digest `ETag`, and JSON:

  ```json
  {"digest":"<lowercase-sha256>","path":"models/resnet50/weights.safetensors"}
  ```

  `path` is the lossless, percent-encoded root-relative hierarchy path.
- With ingestion enabled, unsupported hierarchy methods return `405` with
  `Allow: GET, HEAD, POST`. Exact `/pcas/<digest>` routes remain read-only
  with `Allow: GET, HEAD`.

#### Opt-in process routes

```bash
pcas --root /data/models serve --process-routes ./process-routes.toml
```

Process routes are disabled unless an explicit TOML file is supplied. The
entire file is parsed and validated before the listener binds; one invalid,
duplicate, ambiguous, or built-in-colliding route aborts startup without
installing any route.

```toml
[[process_routes]]
path = "/decode/{digest:sha256}/{stream:u32}"
executable = "/nix/store/.../bin/va-video-decode"
args = [
  "url",
  "--origin", "http://127.0.0.1:8000",
  "--media-id", "{digest}",
  "--stream-index", "{stream}",
]
request_content_type = "application/vnd.apache.arrow.stream"
response_content_type = "application/vnd.apache.arrow.stream"
max_request_bytes = 16777216
max_concurrency = 1
timeout_seconds = 900
```

- Patterns contain literal segments and typed whole-segment captures only.
  S2 supports exactly `sha256` (lowercase 64-hex) and canonical decimal
  `u32`. Configured patterns must not overlap.
- Captures substitute only complete argv elements such as `"{digest}"`.
  Partial interpolation, capture-selected executables/flags/environment,
  string splitting, shells, and eval are not supported. Literal argv values
  and the absolute executable are trusted operator configuration.
- A matching process POST takes precedence over HTTP ingestion. GET/HEAD
  continue to use hierarchy/digest behavior. A typed-invalid process path is
  rejected rather than falling through to ingestion.
- The request `Content-Type` must exactly match the configured MIME type.
  Request bytes stream to child stdin with bounded memory and a hard
  `max_request_bytes` limit. Child stdin is closed immediately at request EOF.
- Per-route concurrency admission is nonblocking. Saturation returns `503`
  with `Retry-After: 1`. `timeout_seconds`, oversized/erroring request
  bodies, client disconnect, and response cancellation terminate the child
  process group and reap the direct child.
- Child stdout streams to the response with bounded backpressure while stderr
  is drained into a 64 KiB tail. Headers are withheld until stdout begins or
  the child exits. Early nonzero/no-output exits return `502`; zero/no-output
  exits return a clean empty `200`. After stdout commits `200`, a nonzero or
  signaled exit aborts the body stream instead of producing a clean EOF.
- purecas treats request/response bytes as opaque. It does not parse Arrow,
  media, or producer completeness metadata. A VA route supplies trusted
  static `--origin`; only digest/stream are captures. VA owns `expected_rows`
  and Arrow completeness semantics.

**Security warning:** process routes provide remote process execution through
an operator-defined allowlist, but still have no authentication or TLS. Bind
only to a trusted interface or place the server behind an authenticated TLS
reverse proxy. HTTP clients are untrusted; configuration, executable, local
operator, and same-UID filesystem are trusted.

The two routes differ only in identity, cache policy, and MIME hints:

- **Hierarchy route** (`/datasets/train/video-001.mp4`) — a weak `ETag`
  derived from the descriptor's device/inode/size/mtime,
  `Cache-Control: no-cache` (content at a path can be replaced), and MIME
  guessed from the visible filename. Directory requests redirect to a
  trailing slash, serve `index.html` when present, and otherwise return a
  deterministic read-only directory listing that excludes `.pcas`,
  HTML-escapes names, and percent-encodes links. Path resolution
  percent-decodes exactly once and rejects NUL, `..`, platform path
  prefixes, malformed encodings, and root escapes; symlinks may resolve
  only when their final canonical target stays inside the visible tree.
- **Digest route** (`/pcas/<64-hex-sha256>`, exact match only — a bare
  `/pcas`, a trailing slash, or an extra segment all `404`) — accepts
  either hex case and normalizes it, a strong `ETag` of exactly
  `"<lowercase-digest>"`, `Cache-Control: public, max-age=31536000,
  immutable` (content is immutable by contract; in-place mutation is
  store damage, repaired by the next `pcas index`), and MIME guessed from
  the packed object's suffix (falling back to `application/octet-stream`).
  An unknown or malformed digest is `404`; a corrupt or ambiguous packed
  entry is `500` without leaking host paths.

Because the digest route's `ETag` is strong, an entity-tag `If-Range`
there can be satisfied directly; the hierarchy route's `ETag` is always
weak, so an entity-tag `If-Range` there always falls back to the full
representation (an `HTTP-date`-based `If-Range` works on both routes).

## Filesystem behavior and limitations

- **Immutability.** Hard links make the digest name and every
  deduplicated visible path reference one inode; in-place writes change
  all of them simultaneously and invalidate the old digest name. Treat
  this as exceptional damage repaired by the next `pcas index`, not a
  supported mutation path. purecas does not change file modes as an
  enforcement mechanism — filesystem permissions remain an operator
  policy.
- **Same-filesystem only.** Hard links cannot cross filesystem
  boundaries. A mount nested under the root can violate this even though
  its path is lexically inside the root; `pcas index` reports the path
  and device mismatch, leaves the file unchanged, and exits non-zero
  after processing every other independent path. There is no silent
  copy fallback.
- **Shared inode metadata.** Hard links share ownership, permission
  bits, mtime/ctime, ACLs, xattrs, and other inode-level state. When two
  content-identical but metadata-different files are deduplicated, the
  existing object inode wins; the newer path adopts its metadata.
  Applications that need path-specific inode metadata are outside this
  storage model.
- **Deletion and pruning.** Deleting a visible path just decrements its
  inode's link count; the `.pcas` object entry keeps the bytes alive
  until the next `pcas index`, which removes an object entry once its
  freshly re-checked link count is exactly 1. Deleting `.pcas` entirely
  removes indexed lookup links (and may let now-unreferenced content be
  pruned later) but never invalidates visible paths.
- **Copies aren't references until indexed.** A byte-for-byte copy is
  just another ordinary file until the next `pcas index` hashes it and
  replaces it with a hard link to the existing object inode.
- **Packed lookup is a bounded shard scan, not a single `open`.** Because
  the index time and suffix follow the digest in the filename, `pcas
  path`/the digest HTTP route look up an entry by scanning the small
  `.pcas/sha256/<first2>/` shard directory for the matching 64-hex-digest
  prefix, rather than opening one exact path. Multiple matching entries
  in a shard are store corruption, reported rather than silently
  resolved — run `pcas index --rehash` to repair.

## Building hierarchy with Nix

Nix recipes materialize data into ordinary visible paths, and `pcas
index` indexes the result — the same "materialize, then index" pattern
as any other tool:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    purecas.url = "github:Hong-Xiang/purecas";
  };

  outputs = { self, nixpkgs, purecas, ... }:
    let
      pkgs = import nixpkgs { system = "x86_64-linux"; };
      pcas = purecas.packages.x86_64-linux.default;
    in {
      apps.x86_64-linux.fetch-sbd = {
        type = "app";
        program = toString (pkgs.writeShellScript "fetch-sbd" ''
          set -euo pipefail

          root="''${CAS_ROOT:-$HOME/data/blob}"
          mkdir -p "$root/datasets/sbd-rai/videos" "$root/datasets/sbd-rai/annotations"

          ${pkgs.curl}/bin/curl -fsSL -o "$root/datasets/sbd-rai/videos/001.mp4" \
            https://example.com/sbd-videos/001.mp4
          ${pkgs.curl}/bin/curl -fsSL -o "$root/datasets/sbd-rai/annotations/scene_001.txt" \
            https://example.com/sbd-annotations/scene_001.txt

          ${pcas}/bin/pcas --root "$root" index 'datasets/sbd-rai/*'
        '');
      };

      devShells.x86_64-linux.default = pkgs.mkShell {
        buildInputs = [ pcas ];
      };
    };
}
```

```bash
# Materialize the visible hierarchy and index it (recipe is reproducible via Nix)
nix run .#fetch-sbd

# Use the stored files directly, or via their indexed digest
cat ~/data/blob/datasets/sbd-rai/videos/001.mp4
pcas path <digest-printed-by-index>
```

Do not combine this pattern with legacy ingestion commands
(`add-path`/`add-url`/`pkg`) in the same root — see the next section.

## Legacy surfaces and the transition

An earlier design stored content under `<root>/sha256/<first2>/<hash>`
(read-only, `0o444`) with all naming, package membership, tags, and
relations tracked only in a `purecas.db` SQLite file. That model is being
replaced by the filesystem-first model documented above. The two models
are **not interoperable in the same root**:

- `pcas index` refuses to run against a root containing a top-level
  `purecas.db` (see [`pcas index`](#pcas-index) above), because a legacy
  command can rewrite that database in place after it has been hard-linked
  as ordinary content, silently corrupting an entry served under an
  immutable digest identity.
- Legacy commands (below) know nothing about `.pcas`, hierarchy
  organization, or digest HTTP routes.

**Use a separate root for legacy commands until each surface below has an
adapt-or-remove follow-up completed and, if you have an existing legacy
store, until a migration tool ([#16](https://github.com/Hong-Xiang/purecas/issues/16))
has moved it into a filesystem-first root.**

| Legacy surface | Current behavior | Follow-up |
|---|---|---|
| `pcas add-path`, `pcas add-url` (incl. `--unzip`) | Copies into `<root>/sha256/<first2>/<hash>` and registers the file in `purecas.db` | [#20](https://github.com/Hong-Xiang/purecas/issues/20) |
| `pcas pkg`, `pcas tag`, `pcas meta`, `pcas rel`, `pcas export`, `pcas import` | Package membership, logical paths, tags, metadata, and relations are authoritative only in `purecas.db` | [#19](https://github.com/Hong-Xiang/purecas/issues/19) |
| `pcas lfs-agent` (Git LFS custom transfer agent) | Stores into legacy `sha256/<prefix>/<hash>` and best-effort registers in `purecas.db` | [#18](https://github.com/Hong-Xiang/purecas/issues/18) |
| Rust `purecas::Store`/`Blob`/`Package` library API | Entirely SQLite-backed; no awareness of `.pcas` or the visible hierarchy | [#17](https://github.com/Hong-Xiang/purecas/issues/17) |
| Python `purecas` extension (`purecas-python`) | A wrapper over the legacy Rust `Store` API above | [#17](https://github.com/Hong-Xiang/purecas/issues/17) |
| Migrating an existing legacy store | No tool yet; do not hand-migrate by mixing layouts in one root | [#16](https://github.com/Hong-Xiang/purecas/issues/16) |

These commands and APIs remain available as legacy behavior on this
branch, but they do not interoperate with filesystem-first path/digest
serving, and no permanent dual-layout compatibility is planned. See the
[design issue](https://github.com/Hong-Xiang/purecas/issues/2) for the
full architectural rationale and breaking-transition rules.

### Git LFS integration (legacy)

`pcas` can act as a
[Git LFS custom transfer agent](https://github.com/git-lfs/git-lfs/blob/main/docs/custom-transfers.md),
storing large files pushed/pulled through Git LFS in a legacy purecas
root instead of a remote server. Add to your repo's `.git/config` (or
global `~/.gitconfig`):

```gitconfig
[lfs "customtransfer.pcas"]
    path = pcas
    args = "lfs-agent"
[lfs]
    standalonetransferagent = pcas
```

Pass a non-default root via `args = "--root /path/to/cas lfs-agent"` or
the `CAS_ROOT` environment variable. This root is a legacy `sha256/` +
`purecas.db` root today (see [#18](https://github.com/Hong-Xiang/purecas/issues/18))
and must not be the same root you run `pcas index`/`pcas serve` against.

## Development

```bash
nix develop                                        # enter dev shell
cargo build                                         # build
cargo test --workspace --exclude purecas-python     # run tests
cargo clippy --workspace --exclude purecas-python --all-targets -- -D warnings   # lint
cargo fmt --all                                     # format
nix build .#pcas                                    # nix build
```

## Design Decisions

- **SHA-256 only** — no multi-algorithm complexity, consistent with Nix
  conventions.
- **Hard links, not copies** — `.pcas` object entries share an inode with
  the visible file(s) they were indexed from, so indexing never
  duplicates data on disk.
- **No authoritative database** — the filesystem is the complete source
  of truth for both content and organization; `.pcas` is disposable,
  rebuildable state, not a required component for correctness.
- **`pcas serve` on axum/Tokio** — the hierarchy and digest HTTP routes
  use current stable `axum`/Tokio, already present transitively through
  `reqwest`. Every file or resolved digest is opened exactly once, and
  representation metadata/body bytes both come from that same
  descriptor, so `tower-http`'s path-only `ServeFile`/`ServeDir` (which
  would reopen a path after deriving metadata) are deliberately not used.
  The async runtime is entered only for this command. Byte-range parsing
  is a small hand-written grammar rather than
  `headers::Range::satisfiable_ranges` or `http-range-header`: both were
  evaluated and found to reject or mishandle required cases (clamping,
  coalescing overlapping ranges, a suffix range longer than the
  representation, distinguishing malformed syntax from an unsatisfiable
  set).
- **No garbage collection beyond pruning (yet)** — `pcas index` prunes
  object entries with no remaining hard link; broader garbage collection
  across arbitrary retention policies is not yet implemented.
