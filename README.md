# Accordion Drive

Files and folders for Accordion: a JSON API for Accordion's web, desktop, and mobile apps, and WebDAV for standard clients such as File Explorer on Windows and Finder on a Mac.

This repo is part of [Accordion](https://github.com/accordion-suite/accordion), a self-hosted, zero-telemetry replacement for Google Workspace. Please open issues in the [main repo](https://github.com/accordion-suite/accordion/issues).

## How it fits together

Drive is a library crate, `accordion-drive`, that the server in the `accordion` repo builds in. It implements the `Module` trait from `accordion-module` ([decision 7](https://github.com/accordion-suite/accordion/blob/main/docs/decisions/0007-module-interface.md)).

Drive keeps no files of its own. Core's storage service holds every file, version, and the trash ([decision 6](https://github.com/accordion-suite/accordion/blob/main/docs/decisions/0006-file-storage.md)), and Drive is how people reach them. Each person sees only their own files. Sharing is planned.

| Path | What it's for | How it signs in |
|---|---|---|
| `/api/drive/` | The JSON API, described in `src/api.rs` | Session cookie or bearer token |
| `/dav/drive/` | WebDAV, with each person's top folder at the root | Username and app password |

A few things work differently from a plain file server:

- **Deleting moves to the trash,** over WebDAV too. A deleted folder goes to the trash whole.
- **Every save is a new version.** Saving part of a file isn't supported, so WebDAV requests that change only part of a file get `501 Not Implemented`.
- **WebDAV locks always succeed.** Windows and macOS need lock support to save files, and versions keep every save, so two people saving at once can't lose either copy.

## Building

Install Rust with [rustup](https://rustup.rs), then run these commands from the repo root:

```sh
cargo build --workspace
cargo test --workspace
```

To build against your own checkout of `accordion-core`, clone the workspace with `scripts/clone-workspace.sh` in the `accordion` repo. Before you commit `Cargo.lock`, push `accordion-core` and run `scripts/pin-versions.sh ../accordion-drive` from the `accordion` repo.

## License

[AGPL-3.0 or later](LICENSE)
