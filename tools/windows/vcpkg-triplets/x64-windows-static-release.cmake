# Static libraries on the static CRT, release only: what the Windows zip
# links, built with -C target-feature=+crt-static (build-env.ps1).
set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE static)
set(VCPKG_LIBRARY_LINKAGE static)
set(VCPKG_BUILD_TYPE release)
