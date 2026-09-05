#!/usr/bin/env bash
set -euo pipefail

scope="${1:-full}"
case "$scope" in
  full|package) ;;
  *)
    echo "usage: $0 [full|package]" >&2
    exit 2
    ;;
esac

expected_glibc="2.17"
container_glibc="$(getconf GNU_LIBC_VERSION | awk '{print $2}')"
if [[ "$container_glibc" != "$expected_glibc" ]]; then
  echo "expected glibc $expected_glibc build container, found $container_glibc" >&2
  exit 1
fi
echo "Linux compatibility build container: glibc $container_glibc"

git config --global --add safe.directory /work
export RUSTUP_HOME="${RUSTUP_HOME:-/tmp/clew-rustup}"
export CARGO_HOME="${CARGO_HOME:-/tmp/clew-cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
  https://sh.rustup.rs | sh -s -- \
  -y --profile minimal --default-toolchain 1.96.0 --component rustfmt

rustc -Vv
cargo -V

if [[ "$scope" == "full" ]]; then
  cargo fmt -- --check
  cargo check --workspace --all-targets --locked
  cargo test --workspace --all-targets --locked
else
  cargo fmt -- --check
fi

rm -rf dist dist-client-flavors
cargo xtask package --out-dir dist

mapfile -t manifests < <(find dist -maxdepth 1 -type f -name '*.release.json' -print | sort)
if [[ "${#manifests[@]}" -ne 1 ]]; then
  echo "expected exactly one Linux release manifest, found ${#manifests[@]}" >&2
  exit 1
fi
manifest="${manifests[0]}"
cargo xtask verify-package --manifest "$manifest"
cargo xtask cache-client-flavor --manifest "$manifest" --cache-dir dist-client-flavors

mapfile -t archives < <(find dist -maxdepth 1 -type f -name '*.zip' -print | sort)
if [[ "${#archives[@]}" -ne 1 ]]; then
  echo "expected exactly one Linux release archive, found ${#archives[@]}" >&2
  exit 1
fi
archive="${archives[0]}"

extract_root="$(mktemp -d)"
trap 'rm -rf "$extract_root"' EXIT
unzip -q "$archive" -d "$extract_root"

archive_root="$extract_root/$(basename "$archive" .zip)"
binary="$archive_root/bin/clew"
if [[ ! -x "$binary" ]]; then
  echo "packaged Linux runtime is missing or not executable: $(basename "$archive" .zip)/bin/clew" >&2
  exit 1
fi

mapfile -t glibc_versions < <(
  objdump -T "$binary" \
    | sed -n 's/.*GLIBC_\([0-9][0-9.]*\).*/\1/p' \
    | sort -Vu
)
if [[ "${#glibc_versions[@]}" -eq 0 ]]; then
  echo "packaged Linux runtime exposes no versioned glibc dependencies" >&2
  exit 1
fi
max_glibc="${glibc_versions[${#glibc_versions[@]}-1]}"
if [[ "$(printf '%s\n%s\n' "$max_glibc" "$expected_glibc" | sort -V | tail -n 1)" != "$expected_glibc" ]]; then
  echo "packaged Linux runtime requires GLIBC_$max_glibc, newer than GLIBC_$expected_glibc" >&2
  exit 1
fi

echo "Packaged Linux runtime maximum GLIBC symbol version: GLIBC_$max_glibc"
sha256sum "$archive" "$manifest" dist/SHA256SUMS
