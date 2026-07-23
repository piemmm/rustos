#!/usr/bin/env bash
# shellcheck shell=bash
# install-qemu.sh — provision the pinned QEMU the QEMU integration tests need.
#
# The runner (`tools/qemu`) spawns `qemu-system-<arch>` by bare name, so it
# resolves through PATH. A distro QEMU older than 9.1 lacks the RISC-V `svade`
# CPU property the riscv64 vertical pins (`tools/qemu/src/riscv64.rs`: `-cpu
# rv64,svade=true,svadu=false`, which exercises the kernel's *software* A/D
# page-fault path). On such a host every riscv64 QEMU test fails with
# "Property 'rv64-riscv-cpu.svade' not found". Weakening the pin would delete
# that coverage, so the environment is fixed instead of the test.
#
# QEMU ships no official prebuilt binary (unlike the llvm.org clang/lld
# release), so — like every other external dependency (the charter's "roll
# your own; do not trust external code") — it is built from the GPG-signed
# official source and pinned. The build lands ONCE in the runner's persistent
# cache; every later run finds the pinned version already present and only puts
# it on PATH. This needs no root at run time: the build-time toolchain (meson,
# ninja, a C compiler, and the glib/pixman development libraries) is a
# documented one-time host prerequisite the admin installs, exactly like
# `rustup` (see tools/ci/github-runner/README.md).
#
# Idempotent and safe to call on every CI run. Fails closed with a clear
# diagnostic if the source cannot be verified or a build prerequisite is
# missing — it never silently falls back to an unverified or stale QEMU.

set -euo pipefail

# --- Pin (single source of truth) -------------------------------------------
# Exact QEMU release. >= 9.1 is required for the RISC-V `svade` CPU property;
# 11.0.2 is the current stable series. Bumping QEMU is editing this one line
# (and re-checking the fingerprint below is still QEMU's release key).
QEMU_VERSION="11.0.2"

# QEMU release signing key (Michael Roth <mdroth@…>), the key
# download.qemu.org signs every release tarball with. Pinned by full
# fingerprint so a substituted key cannot pass: the signature is the
# authoritative supply-chain check on the downloaded source.
QEMU_SIGNING_FPR="CEACC9E15534EBABB82D3FA03353C9CEF108B584"

# The system-emulation targets the runner drives (the three Tier-1 QEMU
# arches). Kept in step with `tools/qemu::Arch`; adding a QEMU-tested arch
# adds its `<arch>-softmmu` target here.
QEMU_TARGETS="riscv64-softmmu,x86_64-softmmu,aarch64-softmmu"

# Where the pinned build is cached. Defaults to the same persistent cache dir
# the workflows use for the pinned C toolchain and `CARGO_TARGET_DIR`, so it
# survives `actions/checkout` (which only wipes the workspace). Overridable so
# a differently-laid-out host or a local developer can relocate it.
CACHE_DIR="${TAIRIX_CACHE_DIR:-/var/lib/actions-runner/tairix-cache}"
PREFIX="${CACHE_DIR}/qemu-${QEMU_VERSION}"
BINDIR="${PREFIX}/bin"

QEMU_BASE_URL="https://download.qemu.org"
TARBALL="qemu-${QEMU_VERSION}.tar.xz"

log() { printf 'install-qemu: %s\n' "$*" >&2; }
die() {
    printf 'install-qemu: error: %s\n' "$*" >&2
    exit 1
}

# Put the pinned bin on PATH for the rest of the job. Under GitHub Actions
# that means appending to $GITHUB_PATH (affects later steps); otherwise print
# a line the caller can `eval` or read. Idempotent: never double-adds.
publish_path() {
    if [ -n "${GITHUB_PATH:-}" ]; then
        printf '%s\n' "$BINDIR" >>"$GITHUB_PATH"
        log "added $BINDIR to \$GITHUB_PATH"
    else
        log "not under GitHub Actions; add it yourself:"
        printf 'export PATH="%s:$PATH"\n' "$BINDIR"
    fi
}

# Already provisioned? The pinned binary reporting the pinned version is the
# whole success condition — skip the (minutes-long) rebuild.
if "${BINDIR}/qemu-system-riscv64" --version 2>/dev/null | grep -qF "version ${QEMU_VERSION}"; then
    log "qemu ${QEMU_VERSION} already installed at ${PREFIX}"
    publish_path
    exit 0
fi

log "provisioning qemu ${QEMU_VERSION} into ${PREFIX}"

# --- Build prerequisites (fail closed, no silent fallback) ------------------
# These are a one-time host prerequisite (see the README): the runner user
# cannot install them (no root), so a missing one is a host-setup error we
# report clearly rather than limp past.
missing=""
for tool in wget tar gpg gpgv python3 ninja meson pkg-config cc; do
    command -v "$tool" >/dev/null 2>&1 || missing="${missing} ${tool}"
done
for pc in glib-2.0 pixman-1; do
    pkg-config --exists "$pc" 2>/dev/null || missing="${missing} ${pc}(dev)"
done
if [ -n "$missing" ]; then
    die "missing build prerequisite(s):${missing}
  QEMU is built from source on this runner and these are a one-time host
  setup step (the runner user has no root). Install them once as admin, e.g.
  on Debian/Ubuntu:
    apt-get install -y build-essential ninja-build meson python3-venv \\
      pkg-config libglib2.0-dev libpixman-1-dev zlib1g-dev libfdt-dev \\
      flex bison
  See tools/ci/github-runner/README.md (Host prerequisites)."
fi

# --- Fetch, verify, build in a throwaway workdir ----------------------------
workdir="$(mktemp -d)"
# shellcheck disable=SC2064  # expand $workdir now: it must survive `set -u`.
trap "rm -rf '$workdir'" EXIT

log "downloading ${TARBALL} + signature"
wget -q -O "${workdir}/${TARBALL}" "${QEMU_BASE_URL}/${TARBALL}"
wget -q -O "${workdir}/${TARBALL}.sig" "${QEMU_BASE_URL}/${TARBALL}.sig"

# Verify the detached signature against the pinned release key in a private,
# throwaway keyring — never the caller's trust store. Fetching the key by its
# pinned full fingerprint means a hostile keyserver cannot substitute another.
log "verifying signature against pinned key ${QEMU_SIGNING_FPR}"
export GNUPGHOME="${workdir}/gnupg"
mkdir -p "$GNUPGHOME"
chmod 700 "$GNUPGHOME"
key_fetched=0
for ks in keyserver.ubuntu.com hkps://keys.openpgp.org; do
    if gpg --quiet --batch --keyserver "$ks" --recv-keys "$QEMU_SIGNING_FPR" 2>/dev/null; then
        key_fetched=1
        break
    fi
    log "keyserver ${ks} did not return the key; trying the next"
done
[ "$key_fetched" -eq 1 ] || die "could not fetch QEMU signing key ${QEMU_SIGNING_FPR} from any keyserver"

# Export the pinned key and verify with gpgv against exactly that key, so the
# check cannot be satisfied by any other key that happened into the keyring.
gpg --batch --yes --export "$QEMU_SIGNING_FPR" >"${workdir}/qemu-release.gpg"
gpgv --keyring "${workdir}/qemu-release.gpg" \
    "${workdir}/${TARBALL}.sig" "${workdir}/${TARBALL}" \
    || die "GPG signature verification of ${TARBALL} FAILED — refusing to build unverified source"
log "signature OK"

log "extracting"
tar -xf "${workdir}/${TARBALL}" -C "$workdir"
srcdir="${workdir}/qemu-${QEMU_VERSION}"
[ -d "$srcdir" ] || die "expected source directory ${srcdir} not found after extract"

# Build into a fresh prefix. Remove any partial/older install first so a failed
# earlier attempt can never leave a half-installed tree that masks the rebuild.
rm -rf "$PREFIX"
mkdir -p "${srcdir}/build"
(
    cd "${srcdir}/build"
    log "configuring (targets: ${QEMU_TARGETS})"
    ../configure \
        --prefix="$PREFIX" \
        --target-list="$QEMU_TARGETS" \
        --enable-fdt \
        --disable-docs \
        --disable-werror
    log "building (this takes a few minutes)"
    ninja
    log "installing into ${PREFIX}"
    ninja install
)

# Prove the pinned binary is the version we meant to install before we publish
# it — fail closed rather than put a wrong build on PATH.
"${BINDIR}/qemu-system-riscv64" --version | grep -qF "version ${QEMU_VERSION}" \
    || die "built qemu does not report version ${QEMU_VERSION}"

log "qemu ${QEMU_VERSION} installed"
publish_path
