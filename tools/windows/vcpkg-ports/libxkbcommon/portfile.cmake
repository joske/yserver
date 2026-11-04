# vcpkg's libxkbcommon port (1.13.2) without xkbcommon-x11: upstream's port
# depends on libx11 and libxcb, whose autotools builds need MSYS2, and
# yserver does not link xkbcommon-x11.
vcpkg_from_github(
    OUT_SOURCE_PATH SOURCE_PATH
    REPO xkbcommon/libxkbcommon
    REF "xkbcommon-${VERSION}"
    SHA512 261af9e02ad533bb6560e2711171812c7d92094db8b54eaa19b477ab68270e6d62b315e18d42527ca9c944e87f36f063d003d74aeb4829b21ecb96ab1b0c1ce7
    HEAD_REF master
    PATCHES
        disable-test.patch
        build.patch
)

vcpkg_find_acquire_program(FLEX)
get_filename_component(FLEX_DIR "${FLEX}" DIRECTORY)
vcpkg_add_to_path(PREPEND "${FLEX_DIR}")

vcpkg_find_acquire_program(BISON)
get_filename_component(BISON_DIR "${BISON}" DIRECTORY)
vcpkg_add_to_path(PREPEND "${BISON_DIR}")

vcpkg_configure_meson(
    SOURCE_PATH "${SOURCE_PATH}"
    OPTIONS
        -Denable-x11=false
        -Denable-xkbregistry=false
        -Denable-wayland=false
        -Denable-docs=false
        -Denable-tools=false
)
vcpkg_install_meson()
vcpkg_fixup_pkgconfig()
vcpkg_copy_pdbs()

file(REMOVE_RECURSE "${CURRENT_PACKAGES_DIR}/debug/include")
file(REMOVE_RECURSE "${CURRENT_PACKAGES_DIR}/debug/share")
if(VCPKG_LIBRARY_LINKAGE STREQUAL "static")
    file(REMOVE_RECURSE "${CURRENT_PACKAGES_DIR}/bin" "${CURRENT_PACKAGES_DIR}/debug/bin")
endif()

vcpkg_install_copyright(FILE_LIST "${SOURCE_PATH}/LICENSE")
