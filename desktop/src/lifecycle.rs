#[derive(Debug, Clone, PartialEq)]
pub enum Lifecycle {
    Starting,
    Running(u16),
    Failed,
}
impl Lifecycle {
    pub fn ready(&mut self, host: &str, port: u64) {
        if matches!(self, Self::Starting) && host == "127.0.0.1" && port > 0 && port <= 65535 {
            *self = Self::Running(port as u16);
        }
    }
    pub fn fail(&mut self) {
        *self = Self::Failed;
    }
    pub fn endpoint(&self) -> Option<String> {
        match self {
            Self::Running(port) => Some(format!("http://127.0.0.1:{port}")),
            _ => None,
        }
    }
}
fn safe_segment(segment: &str) -> bool {
    if segment.is_empty() || segment.contains(['/', '\\', '?', '#']) {
        return false;
    }
    let mut decoded = Vec::new();
    let bytes = segment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return false;
            }
            let hex = |b: u8| (b as char).to_digit(16).map(|v| v as u8);
            let (Some(a), Some(b)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) else {
                return false;
            };
            decoded.push(a * 16 + b);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    decoded != b"."
        && decoded != b".."
        && !decoded
            .iter()
            .any(|b| b.is_ascii_control() || matches!(*b, b'\\'))
}
pub fn allowed(method: &str, path: &str) -> bool {
    match method {
        "GET" => matches!(
            path,
            "/healthz"
                | "/api/bridge/status"
                | "/api/bridge/models"
                | "/api/bridge/config"
                | "/api/bridge/providers"
                | "/api/bridge/discover"
                | "/api/bridge/accio"
        ),
        "POST" => matches!(path, "/api/bridge/models" | "/api/bridge/accio/launch"),
        "PUT" => path == "/api/bridge/config",
        "PATCH" => {
            (path
                .strip_prefix("/api/bridge/models/")
                .is_some_and(safe_segment))
                || path
                    .strip_prefix("/api/bridge/providers/p_")
                    .is_some_and(|id| id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        }
        "DELETE" => path
            .strip_prefix("/api/bridge/models?id=")
            .is_some_and(|id| !id.is_empty() && !id.contains(['&', '#', '/', '?'])),
        _ => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_path_normalization_and_escaped_controls() {
        for id in [
            ".",
            "..",
            "%2e%2e",
            ".%2e",
            "..\\config",
            "%5cconfig",
            "x%00y",
            "x%0ay",
            "x%",
            "x%zz",
        ] {
            assert!(
                !allowed("PATCH", &format!("/api/bridge/models/{id}")),
                "{id}"
            );
        }
        assert!(allowed("PATCH", "/api/bridge/models/a%20b"));
        assert!(allowed("PATCH", "/api/bridge/models/org%2Fmodel"));
        assert!(allowed(
            "PATCH",
            "/api/bridge/models/name%3Fpart%23fragment"
        ));
    }
    #[test]
    fn never_connected_before_readiness() {
        assert_eq!(Lifecycle::Starting.endpoint(), None);
    }
    #[test]
    fn trusted_readiness_only() {
        let mut s = Lifecycle::Starting;
        for (h, p) in [("evil.test", 8080), ("127.0.0.1", 0), ("127.0.0.1", 65536)] {
            s.ready(h, p);
            assert_eq!(s, Lifecycle::Starting);
        }
        s.ready("127.0.0.1", 1234);
        assert_eq!(s.endpoint().as_deref(), Some("http://127.0.0.1:1234"));
    }
    #[test]
    fn crash_removes_connection_and_cannot_resurrect() {
        let mut s = Lifecycle::Starting;
        s.ready("127.0.0.1", 8080);
        s.fail();
        assert_eq!(s.endpoint(), None);
        s.ready("127.0.0.1", 8080);
        assert_eq!(s, Lifecycle::Failed);
    }
    #[test]
    fn spawn_failure_never_connects() {
        let mut s = Lifecycle::Starting;
        s.fail();
        assert_eq!(s.endpoint(), None);
    }
    #[test]
    fn no_arbitrary_network_or_generation() {
        assert!(allowed("GET", "/api/bridge/status"));
        assert!(allowed("DELETE", "/api/bridge/models?id=my%2Fmodel"));
        for path in [
            "http://evil.test",
            "/api/bridge/models?id=a&url=x",
            "/api/llm/generate",
            "/api/bridge/models?id=x#y",
        ] {
            assert!(!allowed("DELETE", path));
        }
        assert!(!allowed("POST", "/healthz"));
    }
}
