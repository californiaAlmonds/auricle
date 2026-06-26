# Contributing to Auricle

Thanks for working on Auricle. This document describes the branch model, the
versioning rules enforced by CI, and how releases are produced. Please read it
before opening a pull request.

## Using GitHub Copilot for development

This repository ships agent guidance in
[.github/copilot-instructions.md](.github/copilot-instructions.md). If you use
GitHub Copilot (chat or agent mode) for development, it automatically reads that
file and will follow the project's architecture, build commands, and coding
conventions. Keep it in mind when reviewing Copilot-generated changes, and update
it when project-wide conventions change.

## Branch model

- `main` — always equals the latest **stable** release. Protected; never commit directly.
- `release/x.y.z` — staging branch for an upcoming version. Betas are cut from here.
- `feature/*` — your working branches. Cut from the relevant `release/x.y.z` branch.

```
feature/* ──PR──► release/x.y.z ──PR──► main
```

## Versioning rules (enforced by CI — non-bypassable)

The version in [src-tauri/Cargo.toml](src-tauri/Cargo.toml) and
[src-tauri/tauri.conf.json](src-tauri/tauri.conf.json) must always match the
`release/x.y.z` branch involved in a pull request.

- When a `release/x.y.z` branch is created, both files are set to `x.y.z`.
- **Feature branches must NOT change the version.** A PR that does is blocked by
  the `check-version` status check.
- A PR into `main` is only allowed from a `release/*` branch (enforced by the
  `source-branch-guard` status check).

These two checks live in rulesets with **no bypass list** — nobody, including
admins, can merge past a version mismatch or a non-release source branch.

## Daily workflow

1. Make sure the right `release/x.y.z` branch exists (cut from `main`).
2. Branch off it: `git checkout -b feature/my-change release/x.y.z`
3. Do your work. Do **not** edit the version fields.
4. Open a PR into `release/x.y.z` and wait for `check-version` to pass.
5. Get it reviewed and merged.

## Releasing

### Beta (from a release branch) — tag-triggered

```bash
git checkout release/0.1.0
git pull
git tag -a v0.1.0-beta.1 -m "Beta 1 for 0.1.0"
git push origin v0.1.0-beta.1   # triggers the beta build + GitHub pre-release
```

### Stable (release → main) — PR-merge-triggered

1. Open a PR: `release/0.1.0` → `main`.
2. CI must pass (`check-version`, `source-branch-guard`).
3. Owner approval is required via [CODEOWNERS](.github/CODEOWNERS).
4. Merging the PR triggers the stable release build and publishes `v0.1.0`.

## Branch protection summary

| Action                    | `main`                            | `release/*`     |
| ------------------------- | --------------------------------- | --------------- |
| Direct push               | blocked                           | blocked         |
| Force push / delete       | blocked                           | blocked         |
| Source of incoming PRs    | only `release/*`                  | any `feature/*` |
| Required checks           | `check-version`, `source-branch-guard` | `check-version` |
| Approval                  | owner (CODEOWNERS)                | optional        |

## Do not

- Commit directly to `main` or `release/*`.
- Change version numbers inside feature branches.
- Open PRs to `main` from anything other than a `release/*` branch.

## Building locally

```bash
npm run build     # debug build
npm run run       # run the native shell
npm run release   # release build
```

See [README.md](README.md) for full prerequisites and architecture notes.
