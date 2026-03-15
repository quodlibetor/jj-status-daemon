use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Request {
    Query { repo_path: String },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Response {
    Status { formatted: String },
    Error { message: String },
    Ok,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_roundtrip() {
        let query = Request::Query {
            repo_path: "/tmp/repo".to_string(),
        };
        let json = serde_json::to_string(&query).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, query);

        let shutdown = Request::Shutdown;
        let json = serde_json::to_string(&shutdown).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, shutdown);
    }

    #[test]
    fn test_response_roundtrip() {
        let status = Response::Status {
            formatted: "abc123 main [1 +5-2] ".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, status);

        let error = Response::Error {
            message: "not found".to_string(),
        };
        let json = serde_json::to_string(&error).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, error);

        let ok = Response::Ok;
        let json = serde_json::to_string(&ok).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ok);
    }
}
