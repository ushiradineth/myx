# Nix installation and age-backed configuration

The complete Mac release includes its native runtime. Installing only the raw
source-built executable is not equivalent. The release currently requires a
regular copied bundle outside `/nix/store`; its integrity reader rejects store
roots, symlinked runtime files and unsafe paths.

The Nix integration therefore fetches a hash-pinned public release, keeps those
public bytes in the store, and materializes a verified versioned copy in the
user's data directory. The `myx` command executes that copied `bin/myx`. It does
not copy or distribute the proprietary SDK or user credentials.

## Credential ownership

Myx uses the Spotify application **client ID**, not a Spotify client secret.
Web API authorization uses PKCE; OAuth access/refresh tokens remain runtime state.
Both the client ID and Soloist API key can come from agenix. Neither needs to be
an inline Nix string or part of a generated store file.

An example nix-darwin declaration, after creating the encrypted files:

```nix
age.secrets = {
  myx-spotify-client-id = {
    file = "${mysecrets}/${hostname}/myx-spotify-client-id.age";
    owner = myvars.username;
    mode = "0600";
  };
  myx-soloist-api-key = {
    file = "${mysecrets}/${hostname}/myx-soloist-api-key.age";
    owner = myvars.username;
    mode = "0600";
  };
};
```

Only encrypted `.age` files enter the secret input. Do not use
`builtins.readFile` on decrypted files, put values in `writeText`, or pass them
as command-line arguments. Encrypt with the intended host/user recipients and
keep private identities outside source control.

## Materialize private regular files

Myx rejects direct Home Manager config symlinks and agenix's published secret
symlink as its key file. The integration reads the decrypted age sources and
creates owner-only regular files instead:

- A public template contains root `client_id = "@MYX_SPOTIFY_CLIENT_ID@"` and
  `[soloist] api_key_file = "@MYX_SOLOIST_KEY_FILE@"`, never their values.
- Both sources must resolve to protected mode-0600 files outside the store.
- The client ID is rendered only into a private mode-0600 config.
- The Soloist key stays in a separate versioned mode-0600 file. The config is
  atomically published after that file, so an existing config keeps its old key
  if an update fails. Old manual configuration is not overwritten.

On Darwin, agenix decrypts asynchronously in its existing launch daemon. The
integration materializes configuration on normal Myx launch, after decrypted
files are available. It does not attach another decryptor, service or global
agenix hook. Help/version remain credential-free. An explicit `--config` selects
the caller's own profile without reading the managed credentials.

The local nix-config implementation lives in `hosts/shu/myx.nix`,
`lib/myx-package.nix` and `lib/myx-materialize.py`. SDK/data/cache paths remain
external. The SDK for this Mac runtime is the Linux AArch64 Soloist executable,
not the native macOS or Windows SDK.

## Select the managed profile

After the package and age declarations are applied, run the Nix-managed command:

```sh
myx config check
myx daemon restart
myx
```

The restart explicitly selects the new protected profile. Do not attach an old
application-folder executable to a different running profile and expect it to
change configuration. Use the Nix command or the Nix-provided launcher.

A generic Soloist-unavailable message is not proof of a missing key. Inspect
`myx daemon status --json` for the current safe diagnostic. An owned WebSocket
transport failure needs a runtime retry/investigation, not automatic replacement
of already provisioned developer credentials.
