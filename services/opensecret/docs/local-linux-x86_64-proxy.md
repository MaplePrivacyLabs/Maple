# Local Linux x86_64 Continuum proxy

This recipe builds and runs the Continuum proxy for local development on
Linux x86_64. It writes `.local/bin/continuum-proxy-linux-amd64` and does not
replace the checked-in aarch64 `continuum-proxy` binary that enclave images
copy. The macOS recipes, `just build-continuum-proxy-macos` and
`just run-continuum-proxy-macos`, stay the local path on macOS.

Secret resolution matches the [local macOS stack](local-macos-stack.md). The
`continuum` scope provides `CONTINUUM_API_KEY`. Do not pass
`--sharedPromptCache`. OpenSecret injects user-bound `cache_salt` values.

Initialize the public dependency from the monorepo root, then build from
`services/opensecret/`:

```sh
git -C ../.. submodule update --init --recursive -- services/opensecret/privatemode-public
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-update-lock-file '.?submodules=1' -c just build-continuum-proxy-linux
```

Run it the same way:

```sh
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-update-lock-file '.?submodules=1' -c just run-continuum-proxy-linux
```

`CONTINUUM_PROXY_PORT` defaults to `8092`. `CONTINUUM_PROXY_WORKSPACE`
defaults to `.local/continuum`. Maple Dev Env sets both and starts these
recipes on Linux x86_64. Linux on another architecture still has no local
proxy recipe. The generated binary is gitignored with the rest of `.local/`.
