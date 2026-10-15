#!/usr/bin/env bash
# Real-repository benchmark: Scion against git on a pinned slice of rust-lang/rust.
#
#   docker compose -f deploy/docker-compose.yml up -d
#   scripts/bench-real.sh                 # writes docs/bench/real.csv
#
# Linux (GNU date). Needs git, git-filter-repo, cargo, protoc and the compose stack.
# The input is fixed: the last $DEPTH commits of rust-lang/rust ending at $RUST_SHA
# (side branches included), reduced with `git filter-repo --subdirectory-filter` to the
# history of $PREFIX, minus the paths that were ever submodules (Scion rejects them). Each workload runs $RUNS times on
# both systems and the median is recorded.
#
# Scion runs as the real binaries (scion-server on Postgres + S3, scn, scion-ingest),
# so its numbers include the network round trips to the stores; git works on local
# disk. Every git operation that has a server counterpart goes through a local bare
# repository (`--no-local` / file://), never through hardlinks.
set -euo pipefail

RUST_SHA=91e9c2688eb23af9a5b77f056156e756550c7c99
DEPTH=8
PREFIX=library
SPARSE_DIR=core
RUNS=${RUNS:-5}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=${WORK:-$ROOT/target/bench-real}
OUT=${OUT:-$ROOT/docs/bench/real.csv}
LISTEN=127.0.0.1:7471
SERVER=http://$LISTEN

# The compose stack's credentials, set outright: a shell that already exports other
# AWS_* values (CI, cloud sandboxes) must not leak them into the benchmark.
export DATABASE_URL=postgres://scion:scion@localhost:5432/scion
export AWS_ENDPOINT=http://localhost:9000 AWS_BUCKET=scion-objects AWS_REGION=us-east-1
export AWS_ACCESS_KEY_ID=scion AWS_SECRET_ACCESS_KEY=scionscion AWS_ALLOW_HTTP=true
unset AWS_SESSION_TOKEN AWS_PROFILE
export SCN_AUTHOR="Bench <bench@example.com>"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=Bench GIT_AUTHOR_EMAIL=bench@example.com
export GIT_COMMITTER_NAME=Bench GIT_COMMITTER_EMAIL=bench@example.com

mkdir -p "$WORK"
SUBSET=$WORK/subset

# --- input -------------------------------------------------------------------

if [ ! -d "$SUBSET" ]; then
  echo "fetching rust-lang/rust@${RUST_SHA:0:12}, depth $DEPTH" >&2
  rm -rf "$WORK/rust"
  git init -q "$WORK/rust"
  git -C "$WORK/rust" fetch -q --depth "$DEPTH" https://github.com/rust-lang/rust "$RUST_SHA"
  git -C "$WORK/rust" branch -q main "$RUST_SHA"
  git clone -q --no-checkout --no-local --single-branch --branch main "$WORK/rust" "$SUBSET"
  rm -rf "$WORK/rust"
  echo "keeping only the history of $PREFIX" >&2
  git -C "$SUBSET" filter-repo --quiet --force --subdirectory-filter "$PREFIX"
  # Scion has no submodules: drop every path that was ever a gitlink in the slice.
  gitlinks=()
  while IFS= read -r path; do
    gitlinks+=(--path "$path")
  done < <(git -C "$SUBSET" log --format= --raw main | awk '$1 ~ /160000/ || $2 ~ /160000/ {print $NF}' | sort -u)
  if [ ${#gitlinks[@]} -gt 0 ]; then
    echo "dropping former submodules: ${gitlinks[*]}" >&2
    git -C "$SUBSET" filter-repo --quiet --force --invert-paths "${gitlinks[@]}"
  fi
fi
COMMITS=$(git -C "$SUBSET" rev-list --count main)
FILES=$(git -C "$SUBSET" ls-tree -r main | wc -l | tr -d ' ')
# The file with the longest history in the slice: the blame target.
BLAME_FILE=$(git -C "$SUBSET" log --format= --name-only main | sort | uniq -c | sort -rn \
  | awk 'NR==1 {print $2}')
echo "input: $COMMITS commits, $FILES files, blame target $BLAME_FILE" >&2

# A bare copy serves as git's "server"; partial clones need filters enabled on it.
ORIGIN=$WORK/origin.git
rm -rf "$ORIGIN"
git clone -q --bare --no-local "$SUBSET" "$ORIGIN"
git -C "$ORIGIN" config uploadpack.allowFilter true
git -C "$ORIGIN" config uploadpack.allowAnySHA1InWant true

# --- binaries and server -----------------------------------------------------

(cd "$ROOT" && cargo build -q --release -p scion-server -p scn -p scion-ingest)
BIN=$ROOT/target/release
SCN=$BIN/scn
INGEST=$BIN/scion-ingest

SCION_LISTEN=$LISTEN "$BIN/scion-server" 2>"$WORK/server.log" &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
  grep -q "listening" "$WORK/server.log" 2>/dev/null && break
  sleep 0.1
done

# --- measurement -------------------------------------------------------------

now_ns() { date +%s%N; }

# median <values...>
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END {print (NR%2 ? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2)}'; }

RESULTS=()
# record <workload> <tool> <ms...>
record() {
  local workload=$1 tool=$2
  shift 2
  local m
  m=$(median "$@")
  RESULTS+=("$workload,$tool,$m")
  printf '%-22s %-6s %10.1f ms   (runs: %s)\n' "$workload" "$tool" "$m" "$*" >&2
}

# timed <command...>: run once and set MS to the elapsed milliseconds. Runs in this
# shell (not in `$(...)`), so a failing command stops the script.
timed() {
  local t0 t1
  t0=$(now_ns)
  "$@" >/dev/null
  t1=$(now_ns)
  MS=$(awk -v d=$((t1 - t0)) 'BEGIN {printf "%.1f", d / 1e6}')
}

RUN_ID=bench-$(date +%s)

# Import the history.
git_import() { rm -rf "$WORK/g-import"; git clone -q --bare --no-local "$ORIGIN" "$WORK/g-import"; }
g=(); s=()
for i in $(seq 1 "$RUNS"); do
  timed git_import
  g+=("$MS")
  timed "$INGEST" import "$SUBSET" "$RUN_ID-$i" main
  s+=("$MS")
done
record "import history" git "${g[@]}"
record "import history" scion "${s[@]}"
REPO=$RUN_ID-1
# Untimed: the numbers below are only worth something if the import is right.
"$INGEST" verify "$SUBSET" "$REPO" --sample 20 >&2

# Sparse clone of one directory.
git_sparse() {
  rm -rf "$WORK/g-sparse"
  git clone -q --no-local --filter=blob:none --sparse "file://$ORIGIN" "$WORK/g-sparse"
  git -C "$WORK/g-sparse" sparse-checkout set "$SPARSE_DIR"
}
scion_sparse() { rm -rf "$WORK/s-sparse"; "$SCN" clone "$SERVER" "$REPO" "$WORK/s-sparse" --sparse "$SPARSE_DIR"; }
g=(); s=()
for _ in $(seq 1 "$RUNS"); do
  timed git_sparse
  g+=("$MS")
  timed scion_sparse
  s+=("$MS")
done
record "sparse clone" git "${g[@]}"
record "sparse clone" scion "${s[@]}"

# Full working copies for the remaining workloads.
rm -rf "$WORK/g-wc" "$WORK/s-wc"
git clone -q --no-local "file://$ORIGIN" "$WORK/g-wc"
"$SCN" clone "$SERVER" "$REPO" "$WORK/s-wc" >/dev/null

g=(); s=()
for _ in $(seq 1 "$RUNS"); do
  timed git -C "$WORK/g-wc" log -n 100
  g+=("$MS")
  timed sh -c "cd '$WORK/s-wc' && '$SCN' log -n 100"
  s+=("$MS")
done
record "log (100)" git "${g[@]}"
record "log (100)" scion "${s[@]}"

g=(); s=()
for _ in $(seq 1 "$RUNS"); do
  timed git -C "$WORK/g-wc" blame -- "$BLAME_FILE"
  g+=("$MS")
  timed sh -c "cd '$WORK/s-wc' && '$SCN' blame '$BLAME_FILE'"
  s+=("$MS")
done
record "blame" git "${g[@]}"
record "blame" scion "${s[@]}"

# Land one changed file: commit plus the step that publishes it to main.
git_land() {
  echo "// bench $1" >>"$WORK/g-wc/$BLAME_FILE"
  git -C "$WORK/g-wc" commit -q -am "bench $1"
  git -C "$WORK/g-wc" push -q origin HEAD:main
}
scion_land() {
  echo "// bench $1" >>"$WORK/s-wc/$BLAME_FILE"
  (cd "$WORK/s-wc" && "$SCN" commit -m "bench $1" && "$SCN" land)
}
g=(); s=()
for i in $(seq 1 "$RUNS"); do
  timed git_land "$i"
  g+=("$MS")
  timed scion_land "$i"
  s+=("$MS")
done
record "land one file" git "${g[@]}"
record "land one file" scion "${s[@]}"

# --- output ------------------------------------------------------------------

mkdir -p "$(dirname "$OUT")"
{
  echo "# rust-lang/rust@$RUST_SHA depth $DEPTH, subtree $PREFIX: $COMMITS commits, $FILES files"
  echo "# sparse dir $SPARSE_DIR, blame $BLAME_FILE, $RUNS runs, median ms"
  echo "# $(uname -sr), $(nproc 2>/dev/null || sysctl -n hw.ncpu) cpus, $(git --version), $(rustc --version)"
  echo "# scion $(git -C "$ROOT" rev-parse --short HEAD)"
  echo "workload,tool,median_ms"
  printf '%s\n' "${RESULTS[@]}"
} >"$OUT"
echo "wrote $OUT" >&2
