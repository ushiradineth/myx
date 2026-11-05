# Public native dependencies only. The SDK and user runtime stay outside Nix.
let
  locked = (builtins.fromJSON (builtins.readFile ../flake.lock)).nodes.nixpkgs.locked;
  nixpkgs = builtins.fetchTree { inherit (locked) type owner repo rev narHash; };
  native = import nixpkgs.outPath { system = "aarch64-darwin"; };
  linux = import nixpkgs.outPath { system = "aarch64-linux"; };
  source = builtins.fetchTree {
    type = "github";
    owner = "sysprog21";
    repo = "elfuse";
    rev = "ba455f45592cd12488519e207c44c32fa8e167df";
    narHash = "sha256-twKqOFUr7+HWabxcOaAKdYIPkwm1AOIOVq2Ru6U4Dt0=";
  };
  packages = rec {
    audioTools = native.pulseaudio.overrideAttrs (old: {
      patches = (old.patches or [ ]) ++ [
        ./compat/pulseaudio-darwin-reset-priority.patch
        ./compat/pulseaudio-json-utf8.patch
      ];
      doInstallCheck = true;
      installCheckPhase = ''
        runHook preInstallCheck
        $CC ${./tests/pulse-priority.c} \
          "$out/lib/pulseaudio/libpulsecommon-17.0.dylib" -o pulse-priority-test
        ./pulse-priority-test
        $CC ${./tests/pulse-json.c} \
          "$out/lib/pulseaudio/libpulsecommon-17.0.dylib" -o pulse-json-test
        ./pulse-json-test
        runHook postInstallCheck
      '';
    });
    developmentTools = native.symlinkJoin {
      name = "myx-elfuse-development-tools";
      paths = [
        native.cargo
        native.rustc
        native.rustfmt
        native.clippy
        (native.python3.withPackages (p: [ p.websockets ]))
      ];
    };

    # Historical names and override chain preserve the validated store identity.
    # These are implementation details, not selectable trial/A-B backends.
    runner = native.stdenv.mkDerivation {
      pname = "myx-elfuse-trial";
      version = "unstable-ba455f4";
      src = source.outPath;
      nativeBuildInputs = [
        native.gnumake
        native.binutils-unwrapped
        native.xxd
        native.python3
        native.darwin.sigtool
      ];
      buildInputs = [
        native.apple-sdk_15
        # ELFUSE uses macOS 15 Hypervisor APIs; the SDK alone does not raise the target.
        (native.darwinMinVersionHook "15.0")
      ];
      dontConfigure = true;
      dontStrip = true;
      buildPhase = ''
        runHook preBuild
        make elfuse GNU_OBJCOPY=${native.binutils-unwrapped}/bin/objcopy
        runHook postBuild
      '';
      doCheck = true;
      checkPhase = ''
        runHook preCheck
        make build/test-tlbi-encoder-host build/test-fork-ipc-protocol-host \
          GNU_OBJCOPY=${native.binutils-unwrapped}/bin/objcopy
        build/test-tlbi-encoder-host
        build/test-fork-ipc-protocol-host
        runHook postCheck
      '';
      installPhase = ''
        runHook preInstall
        mkdir -p "$out/bin" "$out/share/licenses/elfuse"
        cp build/elfuse "$out/bin/elfuse"
        cp LICENSE "$out/share/licenses/elfuse/LICENSE"
        if [ -f NOTICE ]; then cp NOTICE "$out/share/licenses/elfuse/NOTICE"; fi
        runHook postInstall
      '';
      postFixup = ''
        codesign --entitlements ${source.outPath}/entitlements.plist -f -s "-" "$out/bin/elfuse"
      '';
      meta = {
        description = "Pinned local ARM64 Linux ABI runner for the Soloist trial";
        platforms = [ "aarch64-darwin" ];
        license = native.lib.licenses.asl20;
      };
    };

    runnerFutexClock = runner.overrideAttrs (old: {
      pname = "myx-elfuse-futex-clock-trial";
      patches = (old.patches or [ ]) ++ [ ./compat/futex-clock-domain.patch ];
    });

    runnerTimerfd = runnerFutexClock.overrideAttrs (old: {
      pname = "myx-elfuse-timerfd-trial";
      patches = (old.patches or [ ]) ++ [ ./compat/timerfd-readiness.patch ];
    });

    runnerPackaging = runnerTimerfd.overrideAttrs (old: {
      pname = "myx-elfuse-packaging-trial";
      # The upstream atomic link uses a PID-bearing temporary filename, which
      # changes LC_UUID and its signature hash. No guest/runtime code is patched.
      env = (old.env or { }) // {
        NIX_LDFLAGS = (old.env.NIX_LDFLAGS or "") + " -no_uuid";
      };
      preFixup = (old.preFixup or "") + ''
        # Keep the normal audit registration; replace only its implementation.
        source ${./compat/audit-tmpdir.sh}
      '';
      checkPhase = (old.checkPhase or "") + ''
        AUDIT_HOOK_SOURCE=${./compat/audit-tmpdir.sh} \
        AUDIT_TEST_BASH=${native.bash}/bin/bash \
        AUDIT_TEST_TMPDIR="$TMPDIR" \
          python3 ${./tests/test_packaging.py}
      '';
      postInstall = (old.postInstall or "") + ''
        mkdir -p "$out/share/licenses/elfuse-packaging"
        cp ${./compat/nixpkgs-LICENSE} "$out/share/licenses/elfuse-packaging/NIXPKGS-LICENSE"
      '';
      meta = old.meta // {
        license = [ native.lib.licenses.asl20 native.lib.licenses.mit ];
      };
    });

    # Reference cached Linux outputs from a Darwin derivation.
    armLibraries = native.writeText "myx-soloist-arm64-libraries.json" (builtins.toJSON {
      loader = "${linux.glibc}/lib/ld-linux-aarch64.so.1";
      libraryPath = native.lib.makeLibraryPath [
        linux.glibc
        linux.stdenv.cc.cc.lib
        linux.libpulseaudio
      ];
      glibc = linux.glibc.version;
      pulse = linux.libpulseaudio.version;
      arch = "aarch64-linux";
      nixpkgs = locked.rev;
      elfuse = "ba455f45592cd12488519e207c44c32fa8e167df";
    });
  };
in
{
  elfuse = packages.runnerPackaging;
  audio = packages.audioTools;
  libraries = packages.armLibraries;
  development = packages.developmentTools;
  certificates = native.cacert;
  # Metadata/tools for Mac delivery only, not a Linux application package.
  releaseTools = native.symlinkJoin {
    name = "myx-macos-release-tools";
    paths = [ native.python3 native.patchelf ];
  };
}
