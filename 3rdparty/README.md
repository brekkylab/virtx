# Third-party projects

The projects virtx's server is built on that virtx changes.
None is a fork: each is upstream's own repository, a submodule pinned to a commit, and each change of ours is a patch in [`_patches/`](_patches/), under the project's name, applied in order to that commit.
A project virtx comes to change is taken the same way.
The guest kernel, libkrunfw, is not among them: [`_scripts/build-libkrunfw.sh`](_scripts/build-libkrunfw.sh) builds it from upstream's tag as it is, with no submodule and no patch.

| Project | Its patches | Where they are applied |
| --- | --- | --- |
| [libkrun](https://github.com/libkrun/libkrun) | [`_patches/libkrun/`](_patches/libkrun/) | the submodule's own tree, by `cargo xtask init`: cargo takes its crates from there by path |
| [rutabaga_gfx](https://github.com/magma-gpu/rutabaga_gfx) | [`_patches/rutabaga_gfx/`](_patches/rutabaga_gfx/) | the submodule's own tree, by `cargo xtask init`, likewise |
| [virglrenderer](https://gitlab.freedesktop.org/virgl/virglrenderer) | [`_patches/virglrenderer/`](_patches/virglrenderer/) | a copy under `build/`, by [`_scripts/build-virglrenderer.sh`](_scripts/build-virglrenderer.sh) and [`.ps1`](_scripts/build-virglrenderer.ps1) |
| [MoltenVK](https://github.com/KhronosGroup/MoltenVK) | [`_patches/MoltenVK/`](_patches/MoltenVK/) | a copy under `build/`, by [`_scripts/build-moltenvk.sh`](_scripts/build-moltenvk.sh) |
| [SPIRV-Cross](https://github.com/KhronosGroup/SPIRV-Cross) | [`_patches/SPIRV-Cross/`](_patches/SPIRV-Cross/) | a copy under `build/`, by [`_scripts/build-moltenvk.sh`](_scripts/build-moltenvk.sh) |

## Making a patch

A patch is one change, as one commit `git format-patch` writes: `NNNN-name.patch`, where the number is its place in the order and the name is the first word of its title.
`git apply` reads only the diff, so everything above the `---` line is the patch's own comment, and it is markdown:

```
Subject: name: what the project does with the patch applied

## Why virtx needs it

What virtx does with it, what fails without it, and where in virtx the code that uses it is.

## What it changes

What a reader of the diff needs and the diff does not say: the bug as it showed, what was measured, why here and not elsewhere.

## When it can go

What upstream has to have for the file to be deleted, or that nothing upstream replaces it.
```

- A patch is the last resort. What virtx can do from its own side, with an option, an environment variable or an API the project already has, it does there.
- One patch is one reason. Two changes that can go at different times are two patches.
- A patch is as small as its reason: it changes what virtx needs and leaves the code around it as upstream has it, so that it still applies when upstream moves.
- *Why virtx needs it* is about virtx, not about the diff. *What it changes* is left out where the diff says it all.
- *When it can go* names a condition someone can check against upstream: a merge request, a release, a kernel version. Where one part can go before the rest, it says which.
- The title is one line with nothing before the name, and no line of the message is `---` alone, which is where git takes a message to end.

A patch is written, or changed, in a clone of the submodule:

```bash
git clone 3rdparty/virglrenderer /tmp/virglrenderer && cd /tmp/virglrenderer   # at the pinned commit
git config core.commentChar ';'   # or git drops each `## ` line of a message it opens in an editor
git am -k <virtx>/3rdparty/_patches/virglrenderer/*.patch
# the change, as a new commit or amended into the patch it belongs to
git format-patch -k --no-signature --zero-commit -o <virtx>/3rdparty/_patches/virglrenderer <pinned commit>..HEAD
```

`-k` keeps a title as it is, with no `[PATCH n/m]` before it, and `--zero-commit` leaves out the commit's hash: both would change in every file whenever one patch did.
Each file `format-patch` writes is renamed to `NNNN-name.patch`, and a `Subject:` it folded onto a second line is joined back into one.
A new patch takes a number after the last.

## Taking a patch out

- A patch is carried only while upstream lacks what it gives. Each says when that ends, and that is checked whenever the project's pin moves, which is also when a patch stops applying.
- A patch upstream has made unnecessary is deleted, not rebased. One upstream covers in part is cut down to what is left, and its message says what that is.
- Moving the pin is the way to shed patches: the same steps as above on the new commit, with what upstream now has dropped from each.
- A deleted patch's number is not given to another: the numbers only order the files, and a gap in them is fine.
- Taking one out is a change to what the server is built from, and is tested as one: `cargo xtask init`, the build, and the tests that boot machines.
- A project whose last patch is gone is no longer taken from here: its release is used, and its submodule removed.
