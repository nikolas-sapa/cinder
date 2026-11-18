use serde_json::{json, Map, Value};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const NOT_CONNECTED: i64 = -32010;
pub const PEER_OFFLINE: i64 = -32011;
pub const CONTACT_BLOCKED: i64 = -32012;
pub const KEY_CHANGED_BLOCKED: i64 = -32013;
pub const MAILBOX_FULL: i64 = -32014;
pub const RATE_LIMITED: i64 = -32015;
pub const UNAUTHORIZED_SCOPE: i64 = -32016;
pub const NOT_FOUND: i64 = -32017;
pub const CONFLICT: i64 = -32018;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    ParseError,
    InvalidRequest,
    UnknownMethod,
    InvalidParams,
    Internal,
    NotConnected,
    PeerOffline,
    ContactBlocked,
    KeyChangedBlocked,
    MailboxFull,
    RateLimited,
    UnauthorizedScope,
    NotFound,
    Conflict,
    Unimplemented,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParseError => "parse_error",
            Self::InvalidRequest => "invalid_request",
            Self::UnknownMethod => "unknown_method",
            Self::InvalidParams => "invalid_params",
            Self::Internal => "internal",
            Self::NotConnected => "not_connected",
            Self::PeerOffline => "peer_offline",
            Self::ContactBlocked => "contact_blocked",
            Self::KeyChangedBlocked => "key_changed_blocked",
            Self::MailboxFull => "mailbox_full",
            Self::RateLimited => "rate_limited",
            Self::UnauthorizedScope => "unauthorized_scope",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Unimplemented => "unimplemented",
        }
    }

    pub fn code(self) -> i64 {
        match self {
            Self::ParseError => PARSE_ERROR,
            Self::InvalidRequest => INVALID_REQUEST,
            Self::UnknownMethod => METHOD_NOT_FOUND,
            Self::InvalidParams => INVALID_PARAMS,
            Self::Internal => INTERNAL_ERROR,
            Self::NotConnected => NOT_CONNECTED,
            Self::PeerOffline => PEER_OFFLINE,
            Self::ContactBlocked => CONTACT_BLOCKED,
            Self::KeyChangedBlocked => KEY_CHANGED_BLOCKED,
            Self::MailboxFull => MAILBOX_FULL,
            Self::RateLimited => RATE_LIMITED,
            Self::UnauthorizedScope => UNAUTHORIZED_SCOPE,
            Self::NotFound => NOT_FOUND,
            Self::Conflict => CONFLICT,
            Self::Unimplemented => METHOD_NOT_FOUND,
        }
    }

    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Internal
                | Self::NotConnected
                | Self::PeerOffline
                | Self::MailboxFull
                | Self::RateLimited
        )
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::ParseError => "line is not valid JSON",
            Self::InvalidRequest => "not a JSON-RPC 2.0 Request object",
            Self::UnknownMethod => "method not found",
            Self::InvalidParams => "missing/wrong-typed params, bad URI, or limit out of bounds",
            Self::Internal => "unexpected server error; retry, then report",
            Self::NotConnected => "relay/daemon link down; wait + retry",
            Self::PeerOffline => "peer unreachable and no mailbox path exists; wait + retry",
            Self::ContactBlocked => "contact is blocked or declined; do not retry",
            Self::KeyChangedBlocked => "contact key changed; re-run ident.verify with the new SAS",
            Self::MailboxFull => "peer relay mailbox at cap; back off and retry later",
            Self::RateLimited => "throttled; honor data.retry_after_ms if present",
            Self::UnauthorizedScope => "connection scope lacks this capability",
            Self::NotFound => "unknown id, or expired cursor/since; re-fetch",
            Self::Conflict => "state conflict; do not retry the identical call",
            Self::Unimplemented => "method reserved but not implemented in this build",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub kind: ErrorKind,
    pub message: String,
    pub retryable: bool,
    pub hint: Option<String>,
    pub extra: Option<Map<String, Value>>,
}

impl RpcError {
    pub fn new(kind: ErrorKind, hint: impl Into<String>) -> Self {
        Self {
            code: kind.code(),
            kind,
            message: kind.describe().to_string(),
            retryable: kind.retryable(),
            hint: Some(hint.into()),
            extra: None,
        }
    }

    pub fn with_code(mut self, code: i64) -> Self {
        self.code = code;
        self
    }

    /// v2 wire: merges extra fields (e.g. `retry_after_ms`) into `error.data`.
    #[allow(dead_code)]
    pub fn with_extra(mut self, key: &str, value: Value) -> Self {
        self.extra
            .get_or_insert_with(Map::new)
            .insert(key.to_string(), value);
        self
    }

    pub fn parse_error() -> Self {
        Self {
            code: PARSE_ERROR,
            kind: ErrorKind::ParseError,
            message: "Parse error".into(),
            retryable: false,
            hint: Some("line is not valid JSON; send one JSON-RPC message per line".into()),
            extra: None,
        }
    }

    pub fn invalid_request(hint: impl Into<String>) -> Self {
        let mut e = Self::new(ErrorKind::InvalidRequest, hint);
        e.message = "Invalid Request".into();
        e
    }

    pub fn unknown_method() -> Self {
        Self {
            code: METHOD_NOT_FOUND,
            kind: ErrorKind::UnknownMethod,
            message: "Method not found".into(),
            retryable: false,
            hint: Some("call rpc.describe for the supported method catalog".into()),
            extra: None,
        }
    }

    pub fn invalid_params(hint: impl Into<String>) -> Self {
        let mut e = Self::new(ErrorKind::InvalidParams, hint);
        e.message = "Invalid params".into();
        e
    }

    pub fn internal(hint: impl Into<String>) -> Self {
        let mut e = Self::new(ErrorKind::Internal, hint);
        e.message = "Internal error".into();
        e
    }

    pub fn pre_hello() -> Self {
        let mut e = Self::invalid_request("call session.hello first");
        e.kind = ErrorKind::UnauthorizedScope;
        e
    }

    /// Constructor for the `unimplemented` kind — a reserved entry in the
    /// describe() error table; no v1 method produces it.
    #[allow(dead_code)]
    pub fn unimplemented() -> Self {
        let mut e = Self::new(
            ErrorKind::Unimplemented,
            "method reserved but not implemented in this build",
        );
        e.message = "Method not implemented".into();
        e
    }

    pub fn error_object(&self) -> Value {
        let mut data = Map::new();
        data.insert("kind".into(), json!(self.kind.as_str()));
        data.insert("retryable".into(), json!(self.retryable));
        if let Some(h) = &self.hint {
            data.insert("hint".into(), json!(h));
        }
        if let Some(extra) = &self.extra {
            for (k, v) in extra {
                data.insert(k.clone(), v.clone());
            }
        }
        json!({"code": self.code, "message": self.message, "data": data})
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rpc error {} ({}): {}",
            self.code,
            self.kind.as_str(),
            self.message
        )
    }
}

impl std::error::Error for RpcError {}

pub fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error_response(id: Value, err: &RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": err.error_object()})
}

pub fn event_notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}
