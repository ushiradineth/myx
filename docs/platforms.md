# Platform scope

The active release target is `aarch64-apple-darwin`, on Apple Silicon with macOS
15 or newer. The pinned ELFUSE helper uses macOS 15 Hypervisor APIs. The Mac
runtime runs the user-provisioned Linux AArch64 SDK ABI through that helper and
private PulseAudio; the Linux shared-object closure is a Mac runtime asset, not
a Linux application package or port.

Nix packages, CI software gates and release assembly target this Mac delivery.
There is no backend selection feature, MXC mode or librespot decoder fallback.
The release script preserves the required ad-hoc Hypervisor entitlement without
changing OS security policy or enabling OS features. Developer ID notarization
and operating-system trust approval are not supplied by ad-hoc signing.

Windows x86_64 development has started after delivery of the frozen local Mac
release. It is not yet a supported or runnable player. The first work separates
credential-free executable preflight from the Mac adapter; native Windows PCM,
IPC, process ownership and private-storage adapters remain unimplemented. See
[Windows port status](windows-port.md).

Linux application delivery and Intel macOS qualification remain deferred. Source
or metadata for a platform is not a support claim or a release artifact. No
Windows fixtures are needed to initialize or statically check a Mac configuration.

Software compilation and fixture tests are separate from native audio, real
account, copied-layout, GUI, media-key and LaunchAgent acceptance. All of those
Mac acceptance receipts must be completed before calling the Mac release fully
qualified. The owner has chosen to use the local Mac release with documented
limitations and proceed with Windows development; the remaining Mac checks are
not silently passed and no longer block that work.
