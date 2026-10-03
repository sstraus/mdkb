#!/usr/bin/env bash
# Inspect imported symbol versions without executing the Linux binary.
set -euo pipefail
export LC_ALL=C

if [[ $# != 2 || ! $2 =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]]; then
  echo "Usage: $0 <binary> <max-glibc-version> (for example 2.35)" >&2
  exit 2
fi

binary=$1
maximum=$2
# OBJDUMP also allows cross-binutils when the host cannot inspect this ELF.
symbols=$("${OBJDUMP:-objdump}" -T "$binary") || {
  echo "Cannot inspect dynamic symbols: $binary" >&2
  exit 2
}

printf '%s\n' "$symbols" | awk -v binary="$binary" -v maximum="$maximum" '
  function greater(a, b,    left, right, n, m, i) {
    n = split(a, left, ".")
    m = split(b, right, ".")
    for (i = 1; i <= n || i <= m; i++) {
      if (left[i] + 0 != right[i] + 0)
        return left[i] + 0 > right[i] + 0
    }
    return 0
  }
  /\*UND\*/ {
    for (i = 1; i <= NF; i++) {
      version = $i
      gsub(/[()]/, "", version)
      if (version ~ /^GLIBC_[0-9]+\.[0-9]+(\.[0-9]+)?$/) {
        sub(/^GLIBC_/, "", version)
        if (highest == "" || greater(version, highest)) highest = version
      }
    }
  }
  END {
    if (highest == "") {
      print "No imported GLIBC symbol versions found: " binary > "/dev/stderr"
      exit 2
    }
    message = binary " requires GLIBC_" highest " (maximum GLIBC_" maximum ")"
    if (greater(highest, maximum)) {
      print "FAIL: " message > "/dev/stderr"
      exit 1
    }
    print "PASS: " message
  }
'
