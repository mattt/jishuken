# Releasing

The `jishuken` crate includes the Rust library and the `ken` command.

## Trusted publishing setup

crates.io requires the first version of a crate to be published with an API
token. Publish `0.1.0` locally with `cargo publish -p jishuken --locked`, then
configure its trusted publisher in the crate's crates.io settings:

| Setting | Value |
| --- | --- |
| Provider | GitHub |
| Repository owner | `mattt` |
| Repository name | `jishuken` |
| Workflow filename | `release.yml` |
| Environment | `release` |

The GitHub repository must have an environment named `release`. Limit deployment
to version tags (`v*`). The workflow exchanges its GitHub OIDC identity for a
short-lived crates.io token; no Cargo token belongs in GitHub secrets.

See the [crates.io trusted publishing documentation](https://crates.io/docs/trusted-publishing).

## Release a version

1. Update `workspace.package.version` in `Cargo.toml` and regenerate `Cargo.lock`.
2. Run `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
   `cargo test --locked`, and `cargo publish -p jishuken --locked --dry-run`.
   Install Deno to exercise the script and handler tests.
3. Commit and push the changes, then wait for CI to pass.
4. Tag that commit `v<version>` and push the tag.
5. Publish a GitHub release for that tag with release notes.
6. Confirm the **Publish to crates.io** workflow succeeds and the version is
   available on crates.io.

The workflow checks that the tag matches the crate version, runs the checks,
and publishes using trusted publishing. It authenticates but skips the upload
if the version already exists, including the initial bootstrapped release.
To retry, rerun the workflow or manually dispatch it with the version tag
selected as the ref. Dispatches against branches are rejected.
