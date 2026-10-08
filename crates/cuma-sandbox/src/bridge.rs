//! CUMA as the bridge to sandboxes that only speak HTTP.
//!
//! Their prefix is `cuma sandbox exec --session <file> --`: CUMA itself runs
//! as the agent's process and relays its own stdin and stdout to the process
//! it starts inside the sandbox. The session file is written mode 0600 when
//! the launch opens and holds what the bridge needs — the sandbox's address
//! and access token — so no secret appears in an argument list. The values of
//! forwarded variables are read from the bridge's own environment, which is
//! CUMA's.
//!
//! When the ACP connection ends, the bridge sees its stdin close, closes the
//! remote process's stdin, and exits when that process does — or is killed a
//! moment later, after which the launch's session destroys the sandbox.

use crate::{e2b, failure, opensandbox};
use cuma_core::error::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// What a bridge needs to reach one launch's remote process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeSession {
    /// An E2B sandbox: CubeSandbox, or E2B itself.
    E2b(e2b::Session),
    /// An OpenSandbox sandbox.
    Opensandbox(opensandbox::Session),
}

/// Write `session` into the launch's private directory.
pub(crate) fn write_session(dir: &Path, session: &BridgeSession) -> Result<PathBuf> {
    let path = dir.join("session.json");
    let text = serde_json::to_string(session).map_err(|e| failure("bridge", e))?;
    crate::arcbox::write_private(&path, &text)
        .map_err(|e| failure("bridge", format!("writing {}: {e}", path.display())))?;
    Ok(path)
}

/// The prefix that runs an agent through the bridge.
pub(crate) fn prefix(cuma: &Path, session: &Path) -> Vec<String> {
    vec![
        cuma.display().to_string(),
        "sandbox".to_owned(),
        "exec".to_owned(),
        "--session".to_owned(),
        session.display().to_string(),
        "--".to_owned(),
    ]
}

/// Start `argv` in the sandbox `session_file` describes, relay this process's
/// stdio to it, and return its exit code. What `cuma sandbox exec` runs.
pub async fn exec(session_file: &Path, argv: Vec<String>) -> Result<i32> {
    if argv.is_empty() {
        return Err(failure("bridge", "no command to run"));
    }
    let text = std::fs::read_to_string(session_file)
        .map_err(|e| failure("bridge", format!("reading {}: {e}", session_file.display())))?;
    let session: BridgeSession = serde_json::from_str(&text)
        .map_err(|e| failure("bridge", format!("{}: {e}", session_file.display())))?;
    let io = Stdio {
        input: tokio::io::stdin(),
        output: tokio::io::stdout(),
        errors: tokio::io::stderr(),
    };
    match session {
        BridgeSession::E2b(session) => e2b::relay(&session, &argv, io).await,
        BridgeSession::Opensandbox(session) => opensandbox::relay(&session, &argv, io).await,
    }
}

/// The three streams a relay works with — the process's own, or test
/// doubles.
pub struct Stdio<I, O, E> {
    /// What the agent reads.
    pub input: I,
    /// What the agent writes.
    pub output: O,
    /// What the agent logs.
    pub errors: E,
}

/// Feed `input` to `send` in chunks, in order, and call `close` at its end.
pub(crate) async fn pump<I, S, SF, C, CF>(mut input: I, mut send: S, close: C) -> Result<()>
where
    I: AsyncRead + Unpin,
    S: FnMut(Vec<u8>) -> SF,
    SF: std::future::Future<Output = Result<()>>,
    C: FnOnce() -> CF,
    CF: std::future::Future<Output = Result<()>>,
{
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .await
            .map_err(|e| failure("bridge", format!("reading stdin: {e}")))?;
        if read == 0 {
            return close().await;
        }
        send(buffer[..read].to_vec()).await?;
    }
}

/// Write `bytes` and flush, so a JSON-RPC line is not held back.
pub(crate) async fn deliver<W: AsyncWrite + Unpin>(out: &mut W, bytes: &[u8]) -> Result<()> {
    out.write_all(bytes)
        .await
        .map_err(|e| failure("bridge", format!("writing output: {e}")))?;
    out.flush()
        .await
        .map_err(|e| failure("bridge", format!("writing output: {e}")))
}

/// Encode `bytes` the way JSON carries protobuf `bytes`.
pub(crate) fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode what [`b64`] encodes.
pub(crate) fn unb64(text: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|e| failure("bridge", format!("bad base64 from the sandbox: {e}")))
}

/// `url` with `pairs` appended to its query, percent-encoded.
pub(crate) fn with_query(url: &str, pairs: &[(&str, &str)]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(url).map_err(|e| failure("bridge", format!("{url}: {e}")))?;
    url.query_pairs_mut().extend_pairs(pairs);
    Ok(url)
}

/// An HTTP client for sandbox APIs: no overall timeout, since output streams
/// stay open as long as the agent runs.
pub(crate) fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| failure("bridge", format!("building an HTTP client: {e}")))
}

/// A failed HTTP exchange, with the start of the body for context.
pub(crate) async fn http_error(
    sandbox: &str,
    what: &str,
    response: reqwest::Response,
) -> cuma_core::MetaAgentError {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let body: String = body.chars().take(500).collect();
    failure(sandbox, format!("{what}: HTTP {status}: {}", body.trim()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn a_session_file_is_private_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let session = BridgeSession::E2b(e2b::Session {
            envd: "https://49983-sbx.e2b.app".into(),
            token: Some("secret-token".into()),
            sandbox_id: "sbx".into(),
            port: 49983,
            user: "user".into(),
            cwd: Some("/work".into()),
            env: vec!["ANTHROPIC_API_KEY".into()],
        });
        let path = write_session(dir.path(), &session).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let back: BridgeSession =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let BridgeSession::E2b(back) = back else {
            panic!("e2b")
        };
        assert_eq!(back.token.as_deref(), Some("secret-token"));

        let prefix = prefix(Path::new("/usr/local/bin/cuma"), &path);
        assert_eq!(&prefix[1..4], ["sandbox", "exec", "--session"]);
        assert!(
            prefix.iter().all(|w| !w.contains("secret-token")),
            "never on the command line"
        );
    }

    #[tokio::test]
    async fn the_pump_sends_everything_in_order_then_closes() {
        let input: &[u8] = b"line one\nline two\n";
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s, c) = (sent.clone(), closed.clone());
        pump(
            input,
            move |chunk| {
                let s = s.clone();
                async move {
                    s.lock().unwrap().extend(chunk);
                    Ok(())
                }
            },
            move || async move {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(sent.lock().unwrap().as_slice(), input);
        assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
    }
}
