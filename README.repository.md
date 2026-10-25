# Standalone Amaterasu audio encoder source proof

Extracted from Amaterasu commit `a6b0db9e36a1a59328ab4b4785b27b210f8182b7`. The original module README
is `README.md`. The cdylib/rlib uses the existing native module ABI and media
table pinned to local foundation commit `51ef23b1923335e0bb0c6af9eef685c6f4807520`. No remote is
configured. The `file://` Git URL is only a local proof and must be replaced by
an approved stable repository or registry publication for other machines.

The original README's cross-compilation link points to the monorepo's
`docs/cross-compilation.md`; it is not included in this source proof. Platform
toolchains and bundled Opus licensing/build requirements need independent CI
qualification before publishing this repository as a release artifact.

The host's signed release manifest still binds the built artifact's bytes,
component identity, ABI major, load order and grants. This extraction does not
activate the module or change any live audio path.
