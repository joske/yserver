#!/bin/sh
# Layout rules from the source-layout cleanup
# (docs/superpowers/plans/2026-10-08-source-layout-cleanup.md, "Acceptance").
set -eu

cd "$(dirname "$0")/.."
fail=0

# 1. File-size ceilings: 5,000 lines for code, 6,000 for test files.
#    Exceptions: "path max-lines" lines in tools/layout-size-exceptions.txt.
exceptions=tools/layout-size-exceptions.txt
for f in $(git ls-files 'crates/*.rs'); do
    lines=$(wc -l < "$f")
    case "$f" in
        */tests/* | */tests.rs | *_tests.rs) limit=6000 ;;
        *) limit=5000 ;;
    esac
    if [ -f "$exceptions" ]; then
        allowed=$(awk -v p="$f" '$1 == p { print $2 }' "$exceptions")
        [ -n "$allowed" ] && limit=$allowed
    fi
    if [ "$lines" -gt "$limit" ]; then
        echo "size: $f has $lines lines (limit $limit)"
        fail=1
    fi
done

# 2. backend/portable/ stays free of KMS, fd and platform code.
portable=crates/yserver/src/kms/render/backend/portable
if grep -rnE 'crate::drm|\bgbm\b|OwnedFd|RawFd|libc::|nix::|platform::|backend::kms::' "$portable"; then
    echo "portable: $portable names KMS, fd or platform code (matches above)"
    fail=1
fi

# 2b. ...and reaches the platform only through the seam (`P::` hooks).
if grep -rnE 'PlatformBackend|pstate\.|CrtcKey|DrmDeviceKey|crate::input|crate::vt\b|vk::scanout|\binput::' "$portable"; then
    echo "portable: $portable names the KMS platform or its state (matches above)"
    fail=1
fi

# 3. yserver-render does not depend on the KMS stack or on yserver-core.
tree=$(cargo tree -p yserver-render -e normal --prefix none --format '{p}')
forbidden=$(echo "$tree" | awk '{ print $1 }' | sort -u |
    grep -xE 'drm|drm-sys|drm-ffi|gbm|gbm-sys|input|input-sys|udev|libudev-sys|yserver-core' || true)
if [ -n "$forbidden" ]; then
    echo "render deps: yserver-render depends on: $(echo "$forbidden" | tr '\n' ' ')"
    fail=1
fi

# 4. The surface platform is OS-neutral; window-system code lives in src/macos/.
if grep -rnE 'objc2|AppKit|\bMetal|target_os' crates/yserver/src/surface; then
    echo "surface: crates/yserver/src/surface names a window system or target_os (matches above)"
    fail=1
fi

# 5. The Linux build pulls in no Objective-C runtime crates.
linux_tree=$(cargo tree -p yserver -e normal --target x86_64-unknown-linux-gnu --prefix none --format '{p}')
objc=$(echo "$linux_tree" | awk '{ print $1 }' | sort -u | grep -E '^(objc2.*|block2|dispatch2)$' || true)
if [ -n "$objc" ]; then
    echo "linux deps: yserver depends on: $(echo "$objc" | tr '\n' ' ')"
    fail=1
fi

# 6. Behaviour differences come from hooks and capabilities, not target_os.
if grep -rn 'target_os' "$portable"; then
    echo "portable: $portable names target_os (matches above)"
    fail=1
fi

[ "$fail" -eq 0 ] && echo "layout: ok"
exit "$fail"
