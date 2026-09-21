# Nitro Toolkit source import

Nitro Toolkit is maintained directly in `services/opensecret/nitro-toolkit/`.
It is ordinary Maple source, not a submodule or a separately versioned backend
dependency. Changes belong in Maple pull requests. Existing backend Nix and
Just paths continue to read this directory.

## Preserved history and source

The import uses `OpenSecretCloud/nitro-toolkit` at
`dcfea5f66c3f0aea232b649da2ce3661be54cc14`, the revision already pinned by
Maple. It preserves all 17 reachable source commits.

The unmodified import merge is `3fc87f371a364430a3a8c8077dcaf1a034c4f930`.
Its first parent is Maple `f78c5b08053562697cd350d95b7a6b5970e9ed38`; its
second parent is the toolkit source commit above. The imported subtree exactly
matches the original root tree, `ab51e0a1dd7a956b945be3fd57ea0808840f8438`,
including file modes and the MIT license. Setup and documentation adaptations
follow separately, without changing Python, Dockerfiles, or dependency files.

Retain this merge ancestry when merging the import PR; squash/rebase merging
would discard the preserved source ancestry.

Open pull requests and branches from the standalone repository are not part of
this import. In particular, pending toolkit changes must be replayed directly
under this directory rather than updating the former gitlink. Retiring the
standalone repository is a separate step after open work has been accounted for.

## Updating an existing checkout

A fresh clone has the toolkit files immediately. The backend's remaining
`privatemode-public` submodule still needs initialization:

```sh
git submodule update --init --recursive -- services/opensecret/privatemode-public
```

For an older checkout with the toolkit submodule initialized, inspect and
preserve any local toolkit work first. While still on the older revision,
unregister its worktree without forcing away modifications:

```sh
git -C services/opensecret/nitro-toolkit status --short
git submodule deinit -- services/opensecret/nitro-toolkit
```

Then update the Maple checkout normally. If deinitialization refuses because
of local files or modifications, preserve that work before retrying; do not
use force or delete the directory to bypass it. The import does not authorize
changing another workspace's checkout or generated configuration.

The backend still uses Nix's `?submodules=1` and recursive CI checkout for
`privatemode-public`. Existing build, PCR approval, signing, and deployment
boundaries remain in place. This source import does not deploy an enclave or
update signed PCR approvals.
