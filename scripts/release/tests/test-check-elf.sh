source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/check-elf.sh"
ls_bin=$(command -v ls)
ls_needed=$(readelf -d "$ls_bin" | sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\]$/\1/p' | tr '\n' ' ')

CHECK_ELF_ALLOW="$ls_needed" CHECK_ELF_GLIBC_MAX=99.0 expect_exit 0 bash "$s" "$ls_bin"
expect_contains "NEEDED: " "$OUT"
expect_contains "GLIBC max: 2." "$OUT"

# Ceiling below what ls needs.
CHECK_ELF_ALLOW="$ls_needed" CHECK_ELF_GLIBC_MAX=2.0 expect_exit 1 bash "$s" "$ls_bin"
expect_contains "exceeds" "$ERR"

# Numeric, not lexical: 2.10 > 2.9.
CHECK_ELF_ALLOW="$ls_needed" CHECK_ELF_GLIBC_MAX=2.9 \
  STUB_OBJDUMP="0 DF *UND* 0 (GLIBC_2.10) f" PATH="$FIXTURES/bin-elf:$PATH" \
  expect_exit 1 bash "$s" "$ls_bin"

# A library not on the allowlist.
CHECK_ELF_ALLOW="libc.so.6" CHECK_ELF_GLIBC_MAX=99.0 expect_exit 1 bash "$s" "$ls_bin"
expect_contains "not on the allowlist" "$ERR"

stub() { CHECK_ELF_ALLOW="$ls_needed" CHECK_ELF_GLIBC_MAX=99.0 PATH="$FIXTURES/bin-elf:$PATH" "$@"; }
STUB_OBJDUMP="0 DF *UND* 0 (GLIBC_PRIVATE) f" stub expect_exit 1 bash "$s" "$ls_bin"
expect_contains "GLIBC_PRIVATE" "$ERR"
STUB_OBJDUMP="0 DF *UND* 0 (GLIBC_ABI_DT_RELR) f" stub expect_exit 1 bash "$s" "$ls_bin"
expect_contains "unrecognized" "$ERR"
STUB_OBJDUMP="" stub expect_exit 1 bash "$s" "$ls_bin"
expect_contains "no GLIBC_" "$ERR"
STUB_READELF_FAIL=1 stub expect_exit 1 bash "$s" "$ls_bin"
expect_exit 1 bash "$s" /nonexistent
expect_exit 2 bash "$s"

finish
