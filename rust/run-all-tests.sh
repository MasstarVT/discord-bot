#!/usr/bin/env bash
# Runs every crate's tests against the mock database, one crate at a time, each with a fresh private database.
# From the repository root: bash rust/run-all-tests.sh [crate ...]
cd "$(dirname "$0")/.." || exit 1
node rust/mockdb/mockdb.mjs start >/dev/null || exit 1
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-8}"
crates=("$@")
[ ${#crates[@]} -eq 0 ] && crates=($(ls rust/crates rust/bins 2>/dev/null | grep -v ':' | grep .))
fail=0
for c in "${crates[@]}"; do
  eval "$(node rust/mockdb/mockdb.mjs db "${c//-/_}")"
  out=$(cd rust && cargo test -p "$c" 2>&1)
  code=$?
  pass=$(echo "$out" | awk '/^test result:/{for(i=1;i<=NF;i++)if($i=="passed;")s+=$(i-1)}END{print s+0}')
  bad=$(echo "$out" | awk '/^test result:/{for(i=1;i<=NF;i++)if($i=="failed;")s+=$(i-1)}END{print s+0}')
  echo "$c: exit $code, ${pass:-0} passed, ${bad:-0} failed"
  if [ $code -ne 0 ]; then fail=1; echo "$out" | grep -E '^(test .* FAILED|error|---- )' | head -20; echo "$out" > "rust/target/test-$c.log"; [ -n "$CI" ] && echo "$out" | tail -150; fi
done
exit $fail
