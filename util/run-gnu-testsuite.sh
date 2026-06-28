#!/bin/bash
# This file is part of the uutils awk package.
#
# For the full copyright and license information, please view the LICENSE
# file that was distributed with this source code.
#
# Run the upstream GNU awk (gawk) testsuite against the Rust awk implementation.
#
# Unlike grep/sed, gawk does not ship a gnulib init.sh test framework. Its
# testsuite is a (GPL) make-driven suite: test/Makefile.am + a generated
# Maketests, where each test target runs `$(AWK)` and compares the output
# against a committed `<name>.ok` file, leaving a `_<name>` file behind on
# mismatch. We drive gawk's own Makefile with `make check AWK=<wrapper>`, where
# the wrapper execs our Rust `awk` — the faithful analog of grep injecting its
# binary via PATH. gawk's Makefile is never copied into our repo; it is fetched
# fresh at test time. Classification mirrors gawk's own `pass-fail` target: a
# leftover `_<name>` file is a FAIL, its absence a PASS; tests that never ran
# (group-skipped: locale/MPFR/shared-lib) are SKIP.
#
# Get the GNU awk sources with:
#   mkdir -p ../gnu.awk && (cd ../gnu.awk && bash ../awk/util/fetch-gnu.sh)
#
# Usage: ./util/run-gnu-testsuite.sh [options]
#
# Options:
#   -h, --help                Show this help message
#   -v, --verbose             Show diagnostics (diffs) for failing tests
#   -q, --quiet               Only print failures and the final summary
#   --json-output FILE        Write results to FILE as JSON
#
# Environment variables:
#   GNU_AWK_DIR               Path to the extracted GNU awk source tree
#                             (default: ../gnu.awk)
#   PER_RUN_TIMEOUT           Overall timeout in seconds for `make check`
#                             (default: 1800)

# Don't exit on failure since test failures are expected.
set -o pipefail

# Configuration
RUST_AWK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GNU_AWK_DIR="${GNU_AWK_DIR:-${RUST_AWK_DIR}/../gnu.awk}"
GNU_TESTS_DIR=""
VERBOSE=false
QUIET=false
JSON_OUTPUT_FILE=""
PER_RUN_TIMEOUT="${PER_RUN_TIMEOUT:-1800}"
DETAILED_RESULTS=()

# Statistics
TOTAL_TESTS=0
PASSED_TESTS=0
FAILED_TESTS=0
SKIPPED_TESTS=0

usage() {
    echo "Usage: $0 [options]"
    echo
    echo "Options:"
    echo "  -h, --help                Show this help message"
    echo "  -v, --verbose             Show diagnostics (diffs) for failing tests"
    echo "  -q, --quiet               Only print failures and the final summary"
    echo "  --json-output FILE        Write results to FILE as JSON"
    echo
    echo "Environment variables:"
    echo "  GNU_AWK_DIR               Path to the extracted GNU awk source tree"
    echo "                            (default: ../gnu.awk)"
    echo "  PER_RUN_TIMEOUT           Overall timeout in seconds (default: 1800)"
    echo
    echo "Setup:"
    echo "  mkdir -p ../gnu.awk && (cd ../gnu.awk && bash ../awk/util/fetch-gnu.sh)"
}

log_info()    { [[ "$QUIET" != "true" ]] && echo "[INFO] $1"; return 0; }
log_success() { [[ "$QUIET" != "true" ]] && echo "[PASS] $1"; return 0; }
log_skip()    { [[ "$QUIET" != "true" ]] && echo "[SKIP] $1"; return 0; }
log_warning() { echo "[WARN] $1"; }
log_error()   { echo "[FAIL] $1"; }

# Generate JSON output (schema shared with ../grep and ../sed so
# compare_test_results.py works across projects).
generate_json_output() {
    cd "$RUST_AWK_DIR" || return

    local timestamp
    timestamp=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    local rust_version
    rust_version=$(cargo metadata --no-deps --format-version 1 2>/dev/null | jq -r '.packages[0].version // "unknown"')

    local tests_json="[]"
    if [[ ${#DETAILED_RESULTS[@]} -gt 0 ]]; then
        local temp_file
        temp_file=$(mktemp)
        printf "%s\n" "${DETAILED_RESULTS[@]}" > "$temp_file"
        tests_json=$(jq -s '.' < "$temp_file" 2>/dev/null) || tests_json="[]"
        rm -f "$temp_file"
    fi

    jq -n \
        --arg timestamp "$timestamp" \
        --argjson total "$TOTAL_TESTS" \
        --argjson passed "$PASSED_TESTS" \
        --argjson failed "$FAILED_TESTS" \
        --argjson skipped "$SKIPPED_TESTS" \
        --argjson duration "$duration" \
        --arg rust_version "$rust_version" \
        --arg gnu_testsuite_dir "$GNU_TESTS_DIR" \
        --argjson tests "$tests_json" \
        '{
            timestamp: $timestamp,
            summary: {
                total: $total,
                passed: $passed,
                failed: $failed,
                skipped: $skipped,
                duration_seconds: $duration
            },
            environment: {
                rust_awk_version: $rust_version,
                gnu_testsuite_dir: $gnu_testsuite_dir
            },
            tests: $tests
        }' > "$JSON_OUTPUT_FILE"

    log_info "JSON results written to: $JSON_OUTPUT_FILE"
}

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        -h|--help) usage; exit 0 ;;
        -v|--verbose) VERBOSE=true; shift ;;
        -q|--quiet) QUIET=true; shift ;;
        --json-output) JSON_OUTPUT_FILE="$2"; shift 2 ;;
        *) echo "Unknown argument: $1"; usage; exit 1 ;;
    esac
done

# Validate environment
if [[ -d "$GNU_AWK_DIR" ]]; then
    GNU_AWK_DIR="$(cd "$GNU_AWK_DIR" && pwd)"
    GNU_TESTS_DIR="$GNU_AWK_DIR/test"
fi

if [[ ! -f "$GNU_TESTS_DIR/Makefile.am" ]]; then
    log_error "GNU awk testsuite not found at: $GNU_AWK_DIR"
    log_error "Fetch it with:"
    log_error "  mkdir -p ${RUST_AWK_DIR}/../gnu.awk && (cd ${RUST_AWK_DIR}/../gnu.awk && bash ${RUST_AWK_DIR}/util/fetch-gnu.sh)"
    exit 1
fi

if [[ ! -f "$RUST_AWK_DIR/Cargo.toml" ]]; then
    log_error "Not in a Rust project directory: $RUST_AWK_DIR"
    exit 1
fi

# Build the Rust awk implementation
log_info "Building Rust awk implementation..."
cd "$RUST_AWK_DIR" || exit 1
if ! cargo build --release --quiet; then
    log_error "Failed to build Rust awk implementation"
    exit 1
fi

RUST_AWK_BIN="$RUST_AWK_DIR/target/release/awk"
if [[ ! -x "$RUST_AWK_BIN" ]]; then
    log_error "Built awk binary not found at: $RUST_AWK_BIN"
    exit 1
fi
log_info "Using Rust awk binary: $RUST_AWK_BIN"

# gawk's test/Makefile is generated by configure. Generate it once and cache it.
if [[ ! -f "$GNU_TESTS_DIR/Makefile" ]]; then
    log_info "Configuring GNU awk to generate test/Makefile (one-time)..."
    if ! ( cd "$GNU_AWK_DIR" && ./configure >/dev/null 2>&1 ); then
        log_error "Failed to configure GNU awk in $GNU_AWK_DIR"
        exit 1
    fi
fi
if [[ ! -f "$GNU_TESTS_DIR/Makefile" ]]; then
    log_error "test/Makefile still missing after configure: $GNU_TESTS_DIR"
    exit 1
fi

# Create a temporary wrapper that makes our Rust binary look like `gawk`.
# The wrapper is deliberately named `gawk`: gawk's testsuite assumes it is
# invoked under that name (several tests print ARGV[0]), so naming it `awk`
# would spuriously fail those tests on argv[0] alone.
TEST_WORK_DIR=$(mktemp -d)
trap 'rm -rf "$TEST_WORK_DIR"' EXIT
WRAPPER="$TEST_WORK_DIR/gawk"
cat > "$WRAPPER" <<WRAPPER_EOF
#!/bin/sh
exec "$RUST_AWK_BIN" "\$@"
WRAPPER_EOF
chmod +x "$WRAPPER"
log_info "Wrapper awk: $WRAPPER"

GAWK_VERSION=$(basename "$GNU_AWK_DIR" | sed 's/^gawk-//')
[[ "$GAWK_VERSION" == "$(basename "$GNU_AWK_DIR")" ]] && GAWK_VERSION="unknown"

# Universe of tests = every committed <name>.ok expected-output file.
log_info "Discovering tests from $GNU_TESTS_DIR/*.ok"
declare -A IS_TEST=()
for ok in "$GNU_TESTS_DIR"/*.ok; do
    [[ -e "$ok" ]] || continue
    name=$(basename "$ok" .ok)
    IS_TEST["$name"]=1
done
log_info "Found ${#IS_TEST[@]} known tests"

# Clean leftover failure markers from any previous run so our count is accurate.
( cd "$GNU_TESTS_DIR" && rm -f _* )

# Drive gawk's own testsuite with our binary. -k keeps going past failures.
RUN_LOG="$TEST_WORK_DIR/make.log"
log_info "Running GNU awk testsuite (this can take a while)..."
start_time=$(date +%s)

timeout --kill-after=30 "$PER_RUN_TIMEOUT" \
    make -k -C "$GNU_TESTS_DIR" check \
        AWK="$WRAPPER" \
        LC_ALL=C \
    </dev/null >"$RUN_LOG" 2>&1
make_exit=$?

end_time=$(date +%s)
duration=$((end_time - start_time))

if [[ $make_exit -eq 124 || $make_exit -eq 125 ]]; then
    log_warning "make check hit the ${PER_RUN_TIMEOUT}s timeout; results may be partial"
fi

# Record a test result (for JSON output)
record_result() {
    if [[ -n "$JSON_OUTPUT_FILE" ]]; then
        DETAILED_RESULTS+=("$(jq -n \
            --arg name "$1" --arg status "$2" --arg error "$3" \
            '{name: $name, status: $status, error: $error}')")
    fi
}

# Attempted tests = test names gawk echoed (one bare name per line) that are in
# our universe. Group-skipped tests never echo, so they fall out as SKIP.
declare -A ATTEMPTED=()
while IFS= read -r line; do
    [[ -n "${IS_TEST[$line]:-}" ]] && ATTEMPTED["$line"]=1
done < "$RUN_LOG"

# Failed tests = leftover `_<name>` markers (gawk only removes them on a match).
declare -A FAILED=()
for marker in "$GNU_TESTS_DIR"/_*; do
    [[ -e "$marker" ]] || continue
    name=$(basename "$marker")
    name="${name#_}"
    [[ -n "${IS_TEST[$name]:-}" ]] && FAILED["$name"]=1
done

# Some recipes build their `_<name>` output indirectly (e.g. `sed < prog.out >
# _<name>`); when the awk-under-test never produces the intermediate file, that
# `_<name>` is never created and `cmp` errors with "No such file" instead of
# leaving a mismatch marker. gawk's plain marker count would miss these, so we
# also mine the run log for those errors and count them as failures — otherwise
# a non-functional binary would be credited with spurious passes.
while IFS= read -r name; do
    [[ -n "${IS_TEST[$name]:-}" ]] && FAILED["$name"]=1
done < <(sed -n 's/^cmp: _\([^:]*\): No such file.*/\1/p' "$RUN_LOG")

# Classify every known test.
for name in $(printf '%s\n' "${!IS_TEST[@]}" | sort); do
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    if [[ -n "${FAILED[$name]:-}" ]]; then
        FAILED_TESTS=$((FAILED_TESTS + 1))
        log_error "$name"
        if [[ "$VERBOSE" == "true" ]]; then
            head -10 "$GNU_TESTS_DIR/_$name" 2>/dev/null | sed 's/^/  | /'
        fi
        record_result "$name" "FAIL" "Output differs from $name.ok"
    elif [[ -n "${ATTEMPTED[$name]:-}" ]]; then
        PASSED_TESTS=$((PASSED_TESTS + 1))
        log_success "$name"
        record_result "$name" "PASS" ""
    else
        SKIPPED_TESTS=$((SKIPPED_TESTS + 1))
        log_skip "$name"
        record_result "$name" "SKIP" "Not run (group-skipped: locale/MPFR/shared-lib)"
    fi
done

# Tidy up the markers we created in the (shared) GNU tree.
( cd "$GNU_TESTS_DIR" && rm -f _* )

# Print summary
echo
echo "========================================="
echo "GNU awk testsuite results"
echo "========================================="
echo "Total tests:   $TOTAL_TESTS"
echo "Passed:        $PASSED_TESTS"
echo "Failed:        $FAILED_TESTS"
echo "Skipped:       $SKIPPED_TESTS"
echo "Duration:      ${duration}s"

if [[ -n "$JSON_OUTPUT_FILE" ]]; then
    generate_json_output
fi

if [[ $((PASSED_TESTS + FAILED_TESTS)) -gt 0 ]]; then
    pass_rate=$(( (PASSED_TESTS * 100) / (PASSED_TESTS + FAILED_TESTS) ))
    echo "Pass rate:     ${pass_rate}%"
fi

# Mirror the exit convention of ../grep and ../sed: nonzero if anything failed.
[[ $FAILED_TESTS -eq 0 ]] && exit 0 || exit 1
