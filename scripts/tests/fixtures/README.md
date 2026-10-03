# GLIBC gate fixtures

These are full `objdump -T` captures (Apple LLVM 21.0.0, 2026-10-03).
Only the absolute filename in the header was replaced with a stable name.
No Linux binary was executed on macOS.

- `mdkb-v3.11.1-x64.objdump`: downloaded using
  `gh release download v3.11.1 --repo sstraus/mdkb --pattern mdkb-linux-x64`.
  Binary SHA256: `7811fb9d0570d17023996f73e5930fbf3b193dcb3662a0ce0b1f9c0c88b41653`
  (matches the published checksum). Maximum imported version: GLIBC_2.39.
- `ubuntu-jammy-true.objdump`: `bin/true` extracted from
  <https://archive.ubuntu.com/ubuntu/pool/main/c/coreutils/coreutils_8.32-4.1ubuntu1_amd64.deb>.
  Package SHA256: `b4bef42afe93036b1010a8b4cb03f0d3e715eed64d0cd88f7a945be40d0316f6`.
  Binary SHA256: `1d20d8b1bbc861a2e9e0216efb7945fba664a5e6ba5f6a93febd6612a92551a8`.
  Maximum imported version: GLIBC_2.34.

CI replays these captures to test version policy without downloading assets.
The build and release jobs also inspect their actual built binaries using the
runner's GNU objdump; this is the authoritative compatibility gate.
