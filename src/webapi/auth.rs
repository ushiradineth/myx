use std::ffi::CString;
use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::client::ApiError;

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const REDIRECT_URI: &str = "http://127.0.0.1:8989/login";
const MAX_BYTES: u64 = 1024 * 1024;
const SCOPES: &[&str] = &[
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-private",
    "playlist-modify-public",
    "user-library-read",
    "user-library-modify",
    "user-follow-read",
    "user-follow-modify",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-top-read",
    "user-read-recently-played",
];

// Intentionally no Debug: neither credentials nor PKCE material is diagnostic data.
#[derive(Serialize, Deserialize)]
struct Cached {
    client_id: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: u64,
    #[serde(default)]
    scopes: String,
}

pub struct Authorization {
    cached: Cached,
    cache_path: PathBuf,
}

impl Authorization {
    /// Loading never creates directories, keys, or an absent cache.
    pub fn load_cached(client_id: String, cache_path: PathBuf) -> Result<Self> {
        validate_client_id(&client_id)?;
        let (dir, name) = cache_directory(&cache_path, false)?;
        let file = open_at(&dir, &name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
        protected(&file.metadata()?, false)?;
        let bytes = bounded_read(file)?;
        let cached: Cached =
            serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid authorization cache"))?;
        ensure!(
            cached.client_id == client_id && cached.scopes == SCOPES.join(","),
            "authorization cache requires explicit setup"
        );
        validate_secret(&cached.access_token)?;
        if let Some(token) = &cached.refresh_token {
            validate_secret(token)?;
        }
        let mut auth = Self { cached, cache_path };
        auth.token()?;
        Ok(auth)
    }

    pub(crate) fn token(&mut self) -> Result<&str> {
        if now().saturating_add(60) >= self.cached.expires_at {
            self.refresh()?;
        }
        Ok(&self.cached.access_token)
    }

    fn refresh(&mut self) -> Result<()> {
        let refresh = self
            .cached
            .refresh_token
            .as_deref()
            .ok_or_else(|| anyhow!("authorization requires explicit setup"))?;
        let response = exchange(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", &self.cached.client_id),
        ])?;
        let replacement = token_cache(
            &self.cached.client_id,
            response,
            self.cached.refresh_token.clone(),
        )?;
        // A failed refresh/save never returns the stale access token or destroys the old cache.
        save_cache(&self.cache_path, &replacement)?;
        self.cached = replacement;
        Ok(())
    }
}

pub struct AuthorizationFlow {
    listener: TcpListener,
    client_id: String,
    cache_path: PathBuf,
    verifier: String,
    state: String,
    url: String,
}

impl AuthorizationFlow {
    /// The app registration must allow http://127.0.0.1:8989/login.
    pub fn begin(client_id: String, cache_path: PathBuf) -> Result<Self> {
        validate_client_id(&client_id)?;
        let listener = TcpListener::bind("127.0.0.1:8989")
            .map_err(|_| anyhow!("OAuth callback port 8989 is unavailable"))?;
        listener.set_nonblocking(true)?;
        let verifier = random(32);
        let state = random(32);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        let mut url = reqwest::Url::parse("https://accounts.spotify.com/authorize")?;
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT_URI),
            ("scope", &SCOPES.join(" ")),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
            ("state", &state),
        ]);
        Ok(Self {
            listener,
            client_id,
            cache_path,
            verifier,
            state,
            url: url.into(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn wait(self, cancel: &AtomicBool) -> Result<Authorization> {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            ensure!(!cancel.load(Ordering::Relaxed), "authorization cancelled");
            ensure!(
                Instant::now() < deadline,
                "authorization callback timed out"
            );
            match self.listener.accept() {
                Ok((mut stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    let result = read_callback(&mut stream, &self.state, cancel, deadline);
                    let body = if result.is_ok() {
                        "Authorization received. You may close this tab."
                    } else {
                        "Invalid authorization callback."
                    };
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
                    let _ = write!(stream, "HTTP/1.1 {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        if result.is_ok() { "200 OK" } else { "400 Bad Request" }, body.len(), body);
                    if result
                        .as_ref()
                        .is_err_and(|error| error.is::<ConsentDenied>())
                    {
                        bail!("authorization was denied");
                    }
                    let Ok(code) = result else { continue };
                    ensure!(!cancel.load(Ordering::Relaxed), "authorization cancelled");
                    let response = exchange(&[
                        ("grant_type", "authorization_code"),
                        ("code", &code),
                        ("redirect_uri", REDIRECT_URI),
                        ("client_id", &self.client_id),
                        ("code_verifier", &self.verifier),
                    ])?;
                    ensure!(!cancel.load(Ordering::Relaxed), "authorization cancelled");
                    let cached = token_cache(&self.client_id, response, None)?;
                    save_cache(&self.cache_path, &cached)?;
                    return Ok(Authorization {
                        cached,
                        cache_path: self.cache_path,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(_) => bail!("authorization callback listener failed"),
            }
        }
    }
}

fn read_callback(
    stream: &mut TcpStream,
    state: &str,
    cancel: &AtomicBool,
    outer: Instant,
) -> Result<String> {
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    let deadline = outer.min(Instant::now() + Duration::from_secs(3));
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        ensure!(!cancel.load(Ordering::Relaxed), "authorization cancelled");
        ensure!(
            Instant::now() < deadline && header.len() < 8192,
            "invalid callback headers"
        );
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(1) => header.push(byte[0]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            _ => bail!("incomplete callback headers"),
        }
    }
    let text = std::str::from_utf8(&header).map_err(|_| anyhow!("invalid callback headers"))?;
    let mut request = text.lines().next().unwrap_or_default().split_whitespace();
    ensure!(request.next() == Some("GET"), "invalid callback method");
    let target = request
        .next()
        .ok_or_else(|| anyhow!("invalid callback target"))?;
    ensure!(
        matches!(request.next(), Some("HTTP/1.1" | "HTTP/1.0")) && request.next().is_none(),
        "invalid callback request"
    );
    callback_code(target, state)
}

fn callback_code(target: &str, state: &str) -> Result<String> {
    ensure!(
        target.starts_with("/login?") && !target.contains('#') && !target.contains('\\'),
        "invalid callback target"
    );
    let url = reqwest::Url::parse(&format!("http://127.0.0.1:8989{target}"))
        .map_err(|_| anyhow!("invalid callback target"))?;
    let mut actual_state = None;
    let mut code = None;
    let mut denied = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" => {
                ensure!(actual_state.is_none(), "duplicate callback state");
                actual_state = Some(value.into_owned());
            }
            "code" => {
                ensure!(code.is_none(), "duplicate callback code");
                code = Some(value.into_owned());
            }
            "error" => denied = true,
            _ => {}
        }
    }
    ensure!(
        actual_state.as_deref() == Some(state),
        "invalid callback state"
    );
    if denied {
        return Err(ConsentDenied.into());
    }
    let code = code.ok_or_else(|| anyhow!("missing callback code"))?;
    validate_secret(&code)?;
    Ok(code)
}

#[derive(Debug)]
struct ConsentDenied;

impl std::fmt::Display for ConsentDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("authorization was denied")
    }
}

impl std::error::Error for ConsentDenied {}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    token_type: String,
    scope: Option<String>,
}

fn exchange(params: &[(&str, &str)]) -> Result<TokenResponse> {
    let body = reqwest::Url::parse_with_params("https://unused.invalid", params)?
        .query()
        .unwrap_or_default()
        .to_owned();
    let client = reqwest::blocking::Client::builder()
        .tls_backend_native()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| ApiError::Offline)?;
    let response = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .map_err(|_| ApiError::Offline)?;
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            429 => ApiError::RateLimited {
                retry_after_secs: None,
            },
            500..=599 => ApiError::Server,
            _ => ApiError::Unauthorized,
        }
        .into());
    }
    let bytes = bounded_read(response).map_err(|_| anyhow!("invalid authorization response"))?;
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid authorization response"))
}

fn token_cache(
    client_id: &str,
    response: TokenResponse,
    previous_refresh: Option<String>,
) -> Result<Cached> {
    validate_secret(&response.access_token)?;
    ensure!(
        response.token_type.eq_ignore_ascii_case("bearer")
            && response.expires_in > 60
            && response.expires_in <= 86400,
        "invalid authorization response"
    );
    if let Some(scope) = response.scope {
        ensure!(
            SCOPES
                .iter()
                .all(|s| scope.split_whitespace().any(|granted| granted == *s)),
            "authorization is missing required scopes"
        );
    }
    let refresh_token = response.refresh_token.or(previous_refresh);
    if let Some(token) = &refresh_token {
        validate_secret(token)?;
    }
    Ok(Cached {
        client_id: client_id.to_owned(),
        access_token: response.access_token,
        refresh_token,
        expires_at: now().saturating_add(response.expires_in),
        scopes: SCOPES.join(","),
    })
}

fn validate_client_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid Spotify client id"
    );
    Ok(())
}

fn validate_secret(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 16384 && value.bytes().all(|b| b.is_ascii_graphic()),
        "invalid authorization credential"
    );
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn random(size: usize) -> String {
    let mut bytes = vec![0; size];
    rand::fill(bytes.as_mut_slice());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn bounded_read(reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "authorization data exceeds size limit"
    );
    Ok(bytes)
}

fn protected(meta: &Metadata, directory: bool) -> Result<()> {
    // SAFETY: geteuid has no arguments or memory preconditions.
    let uid = unsafe { libc::geteuid() };
    let expected_mode = if directory { 0o700 } else { 0o600 };
    let valid_type = if directory {
        meta.is_dir()
    } else {
        meta.is_file() && meta.nlink() == 1 && meta.len() <= MAX_BYTES
    };
    ensure!(
        meta.uid() == uid && meta.mode() & 0o7777 == expected_mode && valid_type,
        "authorization cache must be private and owned by this user"
    );
    Ok(())
}

fn open_at(dir: &File, name: &CString, flags: i32, mode: libc::mode_t) -> Result<File> {
    // SAFETY: dir and name remain valid throughout openat; ownership of a successful fd is transferred once.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(anyhow!("cannot open protected authorization cache"));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn cache_directory(path: &Path, create: bool) -> Result<(File, CString)> {
    let parent = path.parent().ok_or_else(|| anyhow!("invalid cache path"))?;
    let name = CString::new(
        path.file_name()
            .ok_or_else(|| anyhow!("invalid cache path"))?
            .as_bytes(),
    )?;
    let mut dir = File::open(if path.is_absolute() { "/" } else { "." })?;
    let components: Vec<_> = parent
        .components()
        .filter(|c| !matches!(c, Component::RootDir | Component::CurDir))
        .collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            bail!("invalid cache directory");
        };
        let part = CString::new(part.as_bytes())?;
        if create && index + 1 == components.len() {
            // SAFETY: directory fd and NUL-terminated name are valid. Existing directories are checked below.
            let result = unsafe { libc::mkdirat(dir.as_raw_fd(), part.as_ptr(), 0o700) };
            if result != 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                bail!("cannot create private authorization directory");
            }
        }
        dir = open_at(&dir, &part, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    }
    protected(&dir.metadata()?, true)?;
    Ok((dir, name))
}

fn save_cache(path: &Path, cached: &Cached) -> Result<()> {
    let (dir, name) = cache_directory(path, true)?;
    // Refuse insecure existing destinations instead of silently repairing or following them.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let exists = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if exists == 0 {
        protected(
            &open_at(&dir, &name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?.metadata()?,
            false,
        )?;
    } else {
        ensure!(
            std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound,
            "cannot inspect authorization destination"
        );
    }
    let temporary = CString::new(format!(".oauth-{}.tmp", random(16)))?;
    let mut file = open_at(
        &dir,
        &temporary,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    let result = (|| -> Result<()> {
        protected(&file.metadata()?, false)?;
        let bytes =
            serde_json::to_vec(cached).map_err(|_| anyhow!("cannot encode authorization cache"))?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "authorization cache exceeds size limit"
        );
        file.write_all(&bytes)?;
        file.sync_all()?;
        // SAFETY: both names and the pinned parent fd remain valid; rename is within the private directory.
        ensure!(
            unsafe {
                libc::renameat(
                    dir.as_raw_fd(),
                    temporary.as_ptr(),
                    dir.as_raw_fd(),
                    name.as_ptr(),
                )
            } == 0,
            "cannot save authorization cache"
        );
        dir.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: removal is limited to this operation's unpredictable temporary name in the pinned directory.
        unsafe {
            libc::unlinkat(dir.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result.map_err(|_| anyhow!("cannot persist protected authorization cache"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn callback_validates_state_path_duplicates_and_decoding() {
        assert_eq!(
            callback_code("/login?state=expected&code=a%2Bb", "expected").unwrap(),
            "a+b"
        );
        for target in [
            "/login?state=wrong&code=secret",
            "/other?state=expected&code=x",
            "/login?state=expected&state=expected&code=x",
            "/login?state=expected&code=x&code=y",
            "/login?state=expected&error=access_denied",
            "/login?state=expected&code=x#fragment",
        ] {
            assert!(callback_code(target, "expected").is_err());
        }
    }

    #[test]
    fn load_missing_cache_does_not_create_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("absent/webapi.json");
        assert!(Authorization::load_cached("a".repeat(32), path.clone()).is_err());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn token_validation_retains_refresh_material_and_redacts_rejections() {
        let response = TokenResponse {
            access_token: "new-fixture-access".into(),
            refresh_token: None,
            expires_in: 3600,
            token_type: "Bearer".into(),
            scope: None,
        };
        let cached = token_cache(
            &"a".repeat(32),
            response,
            Some("old-fixture-refresh".into()),
        )
        .unwrap();
        assert_eq!(cached.refresh_token.as_deref(), Some("old-fixture-refresh"));
        let response = TokenResponse {
            access_token: "secret-invalid\ncredential".into(),
            refresh_token: None,
            expires_in: 3600,
            token_type: "Bearer".into(),
            scope: None,
        };
        let error = token_cache(&"a".repeat(32), response, None).err().unwrap();
        assert_eq!(error.to_string(), "invalid authorization credential");
        let response = TokenResponse {
            access_token: "fixture-access".into(),
            refresh_token: None,
            expires_in: 3600,
            token_type: "Bearer".into(),
            scope: Some("user-library-read".into()),
        };
        assert!(token_cache(&"a".repeat(32), response, None).is_err());
    }

    #[test]
    fn protected_cache_rejects_permissions_symlinks_size_and_wrong_client() {
        let root = tempfile::tempdir().unwrap();
        // macOS /var is a symlink: use the canonical fixture root, never a user's cache.
        let root_path = root.path().canonicalize().unwrap();
        std::fs::set_permissions(&root_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root_path.join("webapi.json");
        let cached = Cached {
            client_id: "a".repeat(32),
            access_token: "fixture-access".into(),
            refresh_token: Some("fixture-refresh".into()),
            expires_at: now() + 3600,
            scopes: SCOPES.join(","),
        };
        std::fs::write(&path, serde_json::to_vec(&cached).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Authorization::load_cached("a".repeat(32), path.clone()).is_ok());
        assert!(Authorization::load_cached("b".repeat(32), path.clone()).is_err());
        let hardlink = root_path.join("hardlink");
        std::fs::hard_link(&path, &hardlink).unwrap();
        assert!(Authorization::load_cached("a".repeat(32), path.clone()).is_err());
        std::fs::remove_file(hardlink).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Authorization::load_cached("a".repeat(32), path.clone()).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = root_path.join("link");
        symlink(&path, &link).unwrap();
        assert!(Authorization::load_cached("a".repeat(32), link).is_err());
        let directory_link = root_path.join("directory-link");
        symlink(&root_path, &directory_link).unwrap();
        assert!(
            Authorization::load_cached("a".repeat(32), directory_link.join("webapi.json")).is_err()
        );
        let mut expired = Cached {
            expires_at: 0,
            refresh_token: None,
            ..cached
        };
        std::fs::write(&path, serde_json::to_vec(&expired).unwrap()).unwrap();
        let error = Authorization::load_cached("a".repeat(32), path.clone())
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "authorization requires explicit setup");
        expired.scopes = "different".into();
        std::fs::write(&path, serde_json::to_vec(&expired).unwrap()).unwrap();
        assert!(Authorization::load_cached("a".repeat(32), path.clone()).is_err());
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(Authorization::load_cached("a".repeat(32), path.clone()).is_err());
        std::fs::set_permissions(&root_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Authorization::load_cached("a".repeat(32), path).is_err());
    }
}
