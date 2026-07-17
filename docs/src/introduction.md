# Introduction

TAIRiX is a security-first, multi-user, multi-core operating system written
entirely in Rust. It targets bare-metal x86_64, AArch64, RISC-V 64, and a
browser-hosted `wasm32` profile from a single workspace.

This book is the long-form reference for the project. It is updated **in the
same commit** as the code it describes; see the [`AGENTS.md`][agents] charter
and the staged [`PLAN.md`][plan] for the contract every change is held to.

The Stage 0 deliverable is the repository foundation: the workspace, the
`cargo xtask` build orchestrator, this mdBook, and the CI pipeline. Subsequent
stages introduce the shared libraries, the kernel, architecture ports,
drivers, the filesystem, userland, the window manager, and the installer.

[agents]: https://github.com/tairix-project/tairix/blob/main/AGENTS.md
[plan]: https://github.com/tairix-project/tairix/blob/main/PLAN.md
