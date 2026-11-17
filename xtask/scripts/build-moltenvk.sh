# MoltenVK, built from 3rdparty/MoltenVK against 3rdparty/SPIRV-Cross, for virglrenderer on
# macOS. Sourced by build-deps.sh, with ROOT and REBUILD set; it sets MoltenVK_src, MoltenVK_lib
# and MoltenVK_want, the stamp of what the built one is built from.

echo "Setting up MoltenVK"

MoltenVK_src="$ROOT/3rdparty/MoltenVK"
MoltenVK_lib="$MoltenVK_src/Package/Release/MoltenVK/static/MoltenVK.xcframework/macos-arm64_x86_64/libMoltenVK.a"
MoltenVK_stamp="$MoltenVK_src/Package/.virtx-stamp"

if [ ! -f "$MoltenVK_src/Makefile" ]; then
    echo "MoltenVK is not checked out; run: git submodule update --init" >&2
    exit 1
fi

if ! xcodebuild -version >/dev/null 2>&1; then
    echo "MoltenVK needs Xcode, not only the Command Line Tools." >&2
    echo "Install Xcode, then: sudo xcode-select -s /Applications/Xcode.app" >&2
    exit 1
fi

if [ "$REBUILD" = 1 ]; then
    rm -f "$MoltenVK_stamp"
fi

SPIRV_Cross_src="$ROOT/3rdparty/SPIRV-Cross"
SPIRV_Cross_pin="$(cat "$MoltenVK_src/ExternalRevisions/SPIRV-Cross_repo_revision")"
# A shallow checkout, CI's among them, has no history to answer this with.
if [ "$(git -C "$SPIRV_Cross_src" rev-parse --is-shallow-repository)" = true ]; then
    git -C "$SPIRV_Cross_src" fetch -q --unshallow
fi
# The fork's branch is that revision and our commits on top of it.
if ! git -C "$SPIRV_Cross_src" merge-base --is-ancestor "$SPIRV_Cross_pin" HEAD 2>/dev/null; then
    echo "3rdparty/SPIRV-Cross is not on the revision MoltenVK pins ($SPIRV_Cross_pin)." >&2
    echo "Rebase the fork's virtx branch onto that revision and pin it." >&2
    exit 1
fi

MoltenVK_want="$(for dir in "$MoltenVK_src" "$SPIRV_Cross_src"; do (cd "$dir" && {
    git rev-parse HEAD
    { git diff HEAD; git ls-files -o --exclude-standard -z | xargs -0 git hash-object; } | shasum | cut -d' ' -f1
}); done)"
if [ -f "$MoltenVK_lib" ] && [ "$(cat "$MoltenVK_stamp" 2>/dev/null)" = "$MoltenVK_want" ]; then
    echo "MoltenVK up to date ($(git -C "$MoltenVK_src" rev-parse --short HEAD))"
else
    echo "Fetching MoltenVK dependencies ($(git -C "$MoltenVK_src" rev-parse --short HEAD))"
    (cd "$MoltenVK_src" && ./fetchDependencies --macos --spirv-cross-root "$SPIRV_Cross_src" >/dev/null)

    echo "Compiling MoltenVK "
    make -C "$MoltenVK_src" macos

    if [ ! -f "$MoltenVK_lib" ]; then
        echo "MoltenVK build finished but $MoltenVK_lib is missing" >&2
        exit 1
    fi
    printf '%s\n' "$MoltenVK_want" > "$MoltenVK_stamp"
    echo "MoltenVK built $MoltenVK_lib"
fi
