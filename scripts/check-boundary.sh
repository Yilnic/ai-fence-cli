#!/usr/bin/env sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

required='Cargo.toml README.md LICENSE crates/ai-fence-cli-core/Cargo.toml crates/ai-fence-contract/Cargo.toml crates/ai-fence-model-metadata/Cargo.toml'
for path in $required; do
    if [ ! -f "$path" ]; then
        echo "public boundary: missing required file: $path" >&2
        exit 1
    fi
done

if find . -type l -print | grep -q .; then
    echo "public boundary: symbolic links are not allowed in the public subtree" >&2
    find . -type l -print >&2
    exit 1
fi

if grep -RInE --exclude='Cargo.lock' \
    '(ai[_-]fence[_-]backend|src/backend|\.\./\.\./public|\.\./\.\./src)' \
    crates Cargo.toml; then
    echo "public boundary: private dependency or source reference found" >&2
    exit 1
fi

if find . -type f \( -name '*.pem' -o -name '*.key' -o -name '.env' -o -name 'config.toml' \) -print | grep -q .; then
    echo "public boundary: secret-bearing file type found" >&2
    exit 1
fi

cargo metadata --format-version 1 --no-deps >/dev/null
echo "public boundary: ok"
