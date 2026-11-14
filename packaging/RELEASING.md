# Releasing Joy

English | [简体中文](RELEASING.zh.md)

Every channel, in dependency order. **CI builds everything; humans publish.**
The only thing a tag triggers automatically is artifact creation
(`.github/workflows/release.yml`): four platform binaries and a GitHub Release holding them. The npm upload runs
on a machine with credentials — tokens do not live in CI.

## 0. Preconditions

- [ ] `just check` green on main (fmt / clippy / tests / schema drift / ts / py / eval)
- [ ] `./scripts/smoke-sdk-python.sh` passes (the SDK really talks to a real app-server)
- [ ] Version numbers bumped everywhere they live (see §1)
- [ ] You are a maintainer with: PyPI account (twine token), npm account
      (`NPM_TOKEN` or `npm login`), and a `homebrew-tap` repo you can push to

## 1. Bump versions

Version lives in three places — all must agree:

| Where | File |
|---|---|
| Homebrew formula | `packaging/homebrew/joy.rb` (url tag + sha256 — §4) |
| npm shim | `packaging/npm/package.json` |
| Cargo | `joy-rs/joyczl-cli/Cargo.toml` (drives `joy --version`) |

`CHANGELOG.md` + `CHANGELOG.zh.md`: new section at the top, real notes (what
changed, what breaks, what to reconfigure).

## 2. Tag and build

```sh
git tag v0.8.0 && git push origin v0.8.0
# CI now builds the four binaries + wheels + GitHub Release.
# Watch it: gh run watch
```

When the run is green, the GitHub Release page holds:

```
joy-v0.8.0-aarch64-apple-darwin.tar.gz
joy-v0.8.0-x86_64-apple-darwin.tar.gz
joy-v0.8.0-x86_64-unknown-linux-gnu.tar.gz
joy-v0.8.0-aarch64-unknown-linux-gnu.tar.gz
joyczl_agent_bin-<ver>-<tags>.whl (per platform)
```

## 3. Homebrew

```sh
just release-hash v0.8.0     # rewrites url + sha256 in packaging/homebrew/joy.rb
git add packaging/homebrew/joy.rb && git commit -m "chore: brew formula v0.8.0" && git push
```

Then update your tap (a repo named `homebrew-tap`):

```sh
cp packaging/homebrew/joy.rb <tap-repo>/Formula/joy.rb
cd <tap-repo> && git commit -am "joy 0.8.0" && git push
# verify from a clean machine:
brew tap ChrisVip001/tap && brew install joy && brew test joy
```

## 5. npm

```sh
cd packaging/npm
npm publish              # runs `install.js` on every user machine — test it first:
npm pack                 # inspect the tarball: install.js + bin/joy.js + package.json + README.md
```

Verify in a scratch dir: `npm i -g joyczl-agent@0.8.0 && joy --version`.

## 6. After publishing

- [ ] README install instructions still match reality
- [ ] Open a release issue listing what `models-catalog.json` / env vars changed, if any
- [ ] Next version's CHANGELOG section starts empty
