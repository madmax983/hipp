#!/usr/bin/env bash
# Build the pinned differential oracle (decision D1): the official
# SQLite 3.54.1 amalgamation + its CLI shell, checksum-verified.
#
#   ci/build-oracle.sh [OUT_DIR]     (default: ../hipp-design/oracle)
#
# The tests find the oracle at ../hipp-design/oracle/sqlite3 or via
# $HIPP_ORACLE. Compile options mirror the official CLI build
# (math functions, FTS, R-Tree, JSON is built in).
set -euo pipefail

VERSION_NUM=3540100
VERSION=3.54.1
YEAR=2026
SHA3=92281c86e2957f9b9305890f9ac04204caa7b677cc868dc326d942689df28e30
ZIP="sqlite-amalgamation-${VERSION_NUM}.zip"
URL="https://www.sqlite.org/${YEAR}/${ZIP}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${1:-${here}/../hipp-design/oracle}"
mkdir -p "${out}"
out="$(cd "${out}" && pwd)"

if [[ -x "${out}/sqlite3" ]] && "${out}/sqlite3" --version | grep -q "^${VERSION} "; then
  echo "oracle already built: $("${out}/sqlite3" --version)"
  exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
curl -fsSL --retry 4 --retry-delay 2 -o "${work}/${ZIP}" "${URL}"
got="$(openssl dgst -sha3-256 -r "${work}/${ZIP}" | cut -d' ' -f1)"
if [[ "${got}" != "${SHA3}" ]]; then
  echo "SHA3-256 mismatch for ${ZIP}: got ${got}, want ${SHA3}" >&2
  exit 1
fi
(cd "${work}" && unzip -q "${ZIP}")
src="${work}/sqlite-amalgamation-${VERSION_NUM}"
"${CC:-cc}" -O2 -I"${src}" \
  -DSQLITE_THREADSAFE=0 \
  -DSQLITE_ENABLE_MATH_FUNCTIONS \
  -DSQLITE_ENABLE_FTS4 -DSQLITE_ENABLE_FTS5 \
  -DSQLITE_ENABLE_RTREE \
  -DSQLITE_ENABLE_DBSTAT_VTAB \
  -DSQLITE_ENABLE_EXPLAIN_COMMENTS \
  -DSQLITE_ENABLE_PERCENTILE \
  "${src}/shell.c" "${src}/sqlite3.c" \
  -o "${out}/sqlite3" -lm -ldl
"${out}/sqlite3" --version
"${out}/sqlite3" --version | grep -q "^${VERSION} "
