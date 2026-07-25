use crate::serve::path::VisiblePath;
use anyhow::{bail, Context, Result};
use http::HeaderValue;
use mime::Mime;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    process_routes: Vec<RawRoute>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    path: String,
    executable: PathBuf,
    args: Vec<String>,
    request_content_type: String,
    response_content_type: String,
    max_request_bytes: u64,
    max_concurrency: usize,
    timeout_seconds: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureType {
    Sha256,
    U32,
}

impl CaptureType {
    fn parse_name(raw: &str) -> Result<Self> {
        match raw {
            "sha256" => Ok(Self::Sha256),
            "u32" => Ok(Self::U32),
            _ => bail!("unknown capture type {raw:?}; expected `sha256` or `u32`"),
        }
    }

    fn accepts(self, raw: &str) -> bool {
        match self {
            Self::Sha256 => {
                raw.len() == 64
                    && raw
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }
            Self::U32 => {
                if raw.is_empty()
                    || (raw.len() > 1 && raw.starts_with('0'))
                    || !raw.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return false;
                }
                raw.parse::<u32>().is_ok()
            }
        }
    }
}

#[derive(Debug)]
enum Segment {
    Literal(String),
    Capture {
        name: String,
        kind: CaptureType,
        index: usize,
    },
}

#[derive(Debug)]
struct RoutePattern {
    source: String,
    segments: Vec<Segment>,
    capture_count: usize,
}

impl RoutePattern {
    fn parse(raw: &str) -> Result<Self> {
        if raw == "/" {
            bail!("process route must not be the root path");
        }
        if !raw.starts_with('/') || raw.ends_with('/') || raw.contains('?') || raw.contains('#') {
            bail!("process route must be an absolute path without a query, fragment, or trailing slash: {raw:?}");
        }

        let mut segments = Vec::new();
        let mut names = HashSet::new();
        for raw_segment in raw[1..].split('/') {
            if raw_segment.is_empty() {
                bail!("process route must not contain empty path segments: {raw:?}");
            }
            if raw_segment.starts_with('{') || raw_segment.ends_with('}') {
                let body = raw_segment
                    .strip_prefix('{')
                    .and_then(|value| value.strip_suffix('}'))
                    .with_context(|| {
                        format!("capture must occupy one whole path segment: {raw_segment:?}")
                    })?;
                let (name, kind) = body.split_once(':').with_context(|| {
                    format!("capture must use `{{name:type}}`: {raw_segment:?}")
                })?;
                if name.is_empty()
                    || !name.bytes().enumerate().all(|(index, byte)| {
                        byte == b'_'
                            || byte.is_ascii_alphabetic()
                            || (index > 0 && byte.is_ascii_digit())
                    })
                {
                    bail!("invalid capture name {name:?}");
                }
                if !names.insert(name.to_string()) {
                    bail!("duplicate capture name {name:?} in {raw:?}");
                }
                let index = names.len() - 1;
                segments.push(Segment::Capture {
                    name: name.to_string(),
                    kind: CaptureType::parse_name(kind)?,
                    index,
                });
            } else {
                if raw_segment.contains('{')
                    || raw_segment.contains('}')
                    || !raw_segment.bytes().all(is_unreserved)
                {
                    bail!(
                        "literal route segment must contain only RFC 3986 unreserved bytes: {raw_segment:?}"
                    );
                }
                segments.push(Segment::Literal(raw_segment.to_string()));
            }
        }

        if matches!(
            segments.first(),
            Some(Segment::Literal(value)) if value == "pcas" || value == ".pcas"
        ) {
            bail!("process route collides with reserved top-level namespace: {raw:?}");
        }
        if matches!(
            segments.as_slice(),
            [Segment::Literal(value)] if value == "purecas.db"
        ) {
            bail!("process route collides with the reserved legacy database path");
        }

        Ok(Self {
            source: raw.to_string(),
            capture_count: names.len(),
            segments,
        })
    }

    fn match_path(&self, path: &VisiblePath) -> PatternMatch {
        if path.trailing_slash {
            return PatternMatch::None;
        }
        if path.segments.len() != self.segments.len() {
            return PatternMatch::None;
        }

        let mut captures = vec![OsString::new(); self.capture_count];
        let mut invalid_capture = false;
        for (configured, requested) in self.segments.iter().zip(&path.segments) {
            match configured {
                Segment::Literal(literal) if literal.as_bytes() == requested => {}
                Segment::Capture { kind, index, .. } => {
                    let Ok(requested) = std::str::from_utf8(requested) else {
                        invalid_capture = true;
                        continue;
                    };
                    if kind.accepts(requested) {
                        captures[*index] = OsString::from(requested);
                    } else {
                        invalid_capture = true;
                    }
                }
                Segment::Literal(_) => return PatternMatch::None,
            }
        }
        if invalid_capture {
            PatternMatch::InvalidCapture
        } else {
            PatternMatch::Matched(captures)
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.segments.len() == other.segments.len()
            && self
                .segments
                .iter()
                .zip(&other.segments)
                .all(|(left, right)| segments_overlap(left, right))
    }
}

enum PatternMatch {
    None,
    InvalidCapture,
    Matched(Vec<OsString>),
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn segments_overlap(left: &Segment, right: &Segment) -> bool {
    match (left, right) {
        (Segment::Literal(left), Segment::Literal(right)) => left == right,
        (Segment::Literal(literal), Segment::Capture { kind, .. })
        | (Segment::Capture { kind, .. }, Segment::Literal(literal)) => kind.accepts(literal),
        (
            Segment::Capture {
                kind: CaptureType::Sha256,
                ..
            },
            Segment::Capture {
                kind: CaptureType::U32,
                ..
            },
        )
        | (
            Segment::Capture {
                kind: CaptureType::U32,
                ..
            },
            Segment::Capture {
                kind: CaptureType::Sha256,
                ..
            },
        ) => false,
        (Segment::Capture { .. }, Segment::Capture { .. }) => true,
    }
}

#[derive(Debug)]
enum Arg {
    Literal(OsString),
    Capture(usize),
}

#[derive(Debug)]
pub(crate) struct ProcessRoute {
    pattern: RoutePattern,
    executable: PathBuf,
    args: Vec<Arg>,
    request_content_type: Mime,
    response_content_type: HeaderValue,
    max_request_bytes: u64,
    timeout: Duration,
    semaphore: Arc<Semaphore>,
}

impl ProcessRoute {
    fn validate(raw: RawRoute) -> Result<Self> {
        let pattern = RoutePattern::parse(&raw.path)
            .with_context(|| format!("validating process route {}", raw.path))?;
        validate_executable(&raw.executable)?;

        let capture_indices = pattern
            .segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Capture { name, index, .. } => Some((name.as_str(), *index)),
                Segment::Literal(_) => None,
            })
            .collect::<HashMap<_, _>>();
        let mut referenced = HashSet::new();
        let args = raw
            .args
            .into_iter()
            .map(|raw_arg| {
                if let Some(name) = raw_arg
                    .strip_prefix('{')
                    .and_then(|value| value.strip_suffix('}'))
                {
                    let index = capture_indices.get(name).copied().with_context(|| {
                        format!(
                            "argument placeholder {raw_arg:?} is not declared by route {}",
                            pattern.source
                        )
                    })?;
                    referenced.insert(index);
                    Ok(Arg::Capture(index))
                } else if raw_arg.contains('{') || raw_arg.contains('}') {
                    bail!("capture placeholders must occupy one whole argv element: {raw_arg:?}")
                } else {
                    Ok(Arg::Literal(OsString::from(raw_arg)))
                }
            })
            .collect::<Result<Vec<_>>>()?;
        if referenced.len() != pattern.capture_count {
            let missing = pattern
                .segments
                .iter()
                .filter_map(|segment| match segment {
                    Segment::Capture { name, index, .. } if !referenced.contains(index) => {
                        Some(name.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            bail!(
                "route {} has captures not referenced by argv: {}",
                pattern.source,
                missing.join(", ")
            );
        }

        let request_content_type = raw.request_content_type.parse::<Mime>().with_context(|| {
            format!(
                "invalid request_content_type {:?}",
                raw.request_content_type
            )
        })?;
        let response_mime = raw.response_content_type.parse::<Mime>().with_context(|| {
            format!(
                "invalid response_content_type {:?}",
                raw.response_content_type
            )
        })?;
        let response_content_type = HeaderValue::from_str(response_mime.as_ref())
            .context("response content type is not a valid HTTP header")?;
        if raw.max_request_bytes == 0 {
            bail!("max_request_bytes must be positive");
        }
        if raw.max_concurrency == 0 || raw.max_concurrency > Semaphore::MAX_PERMITS {
            bail!(
                "max_concurrency must be between 1 and {}",
                Semaphore::MAX_PERMITS
            );
        }
        if raw.timeout_seconds == 0 || raw.timeout_seconds > u32::MAX as u64 {
            bail!("timeout_seconds must be between 1 and {}", u32::MAX);
        }

        Ok(Self {
            pattern,
            executable: raw.executable,
            args,
            request_content_type,
            response_content_type,
            max_request_bytes: raw.max_request_bytes,
            timeout: Duration::from_secs(raw.timeout_seconds),
            semaphore: Arc::new(Semaphore::new(raw.max_concurrency)),
        })
    }

    pub(crate) fn path(&self) -> &str {
        &self.pattern.source
    }

    pub(crate) fn executable(&self) -> &Path {
        &self.executable
    }

    pub(crate) fn request_content_type(&self) -> &Mime {
        &self.request_content_type
    }

    pub(crate) fn response_content_type(&self) -> HeaderValue {
        self.response_content_type.clone()
    }

    pub(crate) fn max_request_bytes(&self) -> u64 {
        self.max_request_bytes
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    fn argv(&self, captures: &[OsString]) -> Vec<OsString> {
        self.args
            .iter()
            .map(|arg| match arg {
                Arg::Literal(value) => value.clone(),
                Arg::Capture(index) => captures[*index].clone(),
            })
            .collect()
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        Arc::clone(&self.semaphore).try_acquire_owned()
    }
}

fn validate_executable(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!(
            "process route executable must be absolute: {}",
            path.display()
        );
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("statting process route executable {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "process route executable must be a non-symlink regular file: {}",
            path.display()
        );
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        bail!(
            "process route executable has no execute permission: {}",
            path.display()
        );
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ProcessRoutes {
    routes: Arc<[Arc<ProcessRoute>]>,
}

impl ProcessRoutes {
    pub fn load(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("reading process route config {}", path.display()))?;
        Self::parse(&source)
            .with_context(|| format!("validating process route config {}", path.display()))
    }

    pub fn parse(source: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(source).context("parsing process route TOML")?;
        if raw.process_routes.is_empty() {
            bail!("process route config must contain at least one [[process_routes]] entry");
        }
        let routes = raw
            .process_routes
            .into_iter()
            .map(ProcessRoute::validate)
            .map(|result| result.map(Arc::new))
            .collect::<Result<Vec<_>>>()?;
        for (index, route) in routes.iter().enumerate() {
            for other in &routes[index + 1..] {
                if route.pattern.overlaps(&other.pattern) {
                    bail!(
                        "process routes overlap ambiguously: {} and {}",
                        route.path(),
                        other.path()
                    );
                }
            }
        }
        Ok(Self {
            routes: routes.into(),
        })
    }

    pub(crate) fn lookup(&self, path: &VisiblePath) -> RouteLookup {
        let mut invalid_capture = false;
        for route in self.routes.iter() {
            match route.pattern.match_path(path) {
                PatternMatch::Matched(captures) => {
                    return RouteLookup::Matched(RouteMatch {
                        route: Arc::clone(route),
                        captures,
                    })
                }
                PatternMatch::InvalidCapture => invalid_capture = true,
                PatternMatch::None => {}
            }
        }
        if invalid_capture {
            RouteLookup::InvalidCapture
        } else {
            RouteLookup::None
        }
    }

    #[cfg(test)]
    pub(crate) fn match_path(&self, raw_path: &str) -> Option<RouteMatch> {
        let parsed = crate::serve::path::parse(raw_path).ok()?;
        match self.lookup(&parsed) {
            RouteLookup::Matched(matched) => Some(matched),
            RouteLookup::InvalidCapture | RouteLookup::None => None,
        }
    }
}

pub(crate) enum RouteLookup {
    None,
    InvalidCapture,
    Matched(RouteMatch),
}

pub(crate) struct RouteMatch {
    route: Arc<ProcessRoute>,
    captures: Vec<OsString>,
}

impl RouteMatch {
    #[cfg(test)]
    pub(crate) fn argv(&self) -> Vec<OsString> {
        self.route.argv(&self.captures)
    }

    pub(crate) fn into_parts(self) -> (Arc<ProcessRoute>, Vec<OsString>) {
        let argv = self.route.argv(&self.captures);
        (self.route, argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable() -> String {
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "\\\\")
    }

    fn route(path: &str, args: &[&str]) -> String {
        let args = args
            .iter()
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"
[[process_routes]]
path = {path:?}
executable = "{executable}"
args = [{args}]
request_content_type = "application/octet-stream"
response_content_type = "application/octet-stream"
max_request_bytes = 16
max_concurrency = 1
timeout_seconds = 5
"#,
            executable = executable()
        )
    }

    #[test]
    fn parses_and_matches_typed_captures() {
        let config = ProcessRoutes::parse(&route(
            "/decode/{digest:sha256}/{stream:u32}",
            &["--digest", "{digest}", "--stream", "{stream}"],
        ))
        .unwrap();
        let digest = "a".repeat(64);
        let matched = config.match_path(&format!("/decode/{digest}/17")).unwrap();
        assert_eq!(
            matched.argv(),
            ["--digest", &digest, "--stream", "17"].map(OsString::from)
        );
    }

    #[test]
    fn rejects_noncanonical_capture_values() {
        let config = ProcessRoutes::parse(&route(
            "/decode/{digest:sha256}/{stream:u32}",
            &["{digest}", "{stream}"],
        ))
        .unwrap();
        assert!(config
            .match_path(&format!("/decode/{}/1", "A".repeat(64)))
            .is_none());
        assert!(config
            .match_path(&format!("/decode/{}/01", "a".repeat(64)))
            .is_none());
        assert!(config
            .match_path(&format!("/decode/{}/4294967296", "a".repeat(64)))
            .is_none());
    }

    #[test]
    fn rejects_placeholder_mismatch_and_partial_substitution() {
        assert!(ProcessRoutes::parse(&route("/run/{stream:u32}", &["prefix-{stream}"])).is_err());
        assert!(ProcessRoutes::parse(&route("/run/{stream:u32}", &["{missing}"])).is_err());
        assert!(ProcessRoutes::parse(&route("/run/{stream:u32}", &["literal"])).is_err());
    }

    #[test]
    fn rejects_duplicate_overlap_and_builtin_collisions() {
        let duplicate = format!(
            "{}{}",
            route("/run/{stream:u32}", &["{stream}"]),
            route("/run/{stream:u32}", &["{stream}"])
        );
        assert!(ProcessRoutes::parse(&duplicate).is_err());

        let overlap = format!(
            "{}{}",
            route("/run/{stream:u32}", &["{stream}"]),
            route("/run/17", &[])
        );
        assert!(ProcessRoutes::parse(&overlap).is_err());
        assert!(ProcessRoutes::parse(&route("/pcas/{digest:sha256}", &["{digest}"])).is_err());
        assert!(ProcessRoutes::parse(&route("/.pcas/run", &[])).is_err());
        assert!(ProcessRoutes::parse(&route("/purecas.db", &[])).is_err());
    }

    #[test]
    fn rejects_bad_executable_mime_and_numeric_bounds() {
        let missing = route("/run/{stream:u32}", &["{stream}"])
            .replace(&executable(), "/definitely/missing/process-route");
        assert!(ProcessRoutes::parse(&missing).is_err());
        assert!(ProcessRoutes::parse(
            &route("/run/{stream:u32}", &["{stream}"])
                .replace("application/octet-stream", "not a mime")
        )
        .is_err());
        assert!(ProcessRoutes::parse(
            &route("/run/{stream:u32}", &["{stream}"])
                .replace("max_concurrency = 1", "max_concurrency = 0")
        )
        .is_err());
    }

    #[test]
    fn literal_metacharacters_remain_one_argv_value() {
        let config = ProcessRoutes::parse(&route(
            "/run/{stream:u32}",
            &["$(touch /tmp/never)", "{stream}"],
        ))
        .unwrap();
        let matched = config.match_path("/run/7").unwrap();
        assert_eq!(
            matched.argv(),
            ["$(touch /tmp/never)", "7"].map(OsString::from)
        );
    }
}
