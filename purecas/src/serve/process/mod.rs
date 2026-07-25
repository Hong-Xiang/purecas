mod config;
mod runtime;

pub use config::ProcessRoutes;

pub(crate) use config::{RouteLookup, RouteMatch};

use axum::extract::Request;
use axum::response::IntoResponse;
use axum::response::Response;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::{StatusCode, Version};

pub(crate) async fn dispatch(matched: RouteMatch, request: Request) -> Response {
    if request.version() != Version::HTTP_11 {
        return (
            StatusCode::HTTP_VERSION_NOT_SUPPORTED,
            "Process routes require HTTP/1.1",
        )
            .into_response();
    }
    let (route, argv) = matched.into_parts();
    let content_type_matches = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .is_some_and(|mime| &mime == route.request_content_type());
    if !content_type_matches {
        return runtime::unsupported_media_type();
    }
    if request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > route.max_request_bytes())
    {
        return runtime::payload_too_large();
    }
    let permit = match route.try_acquire() {
        Ok(permit) => permit,
        Err(_) => return runtime::saturated(),
    };
    runtime::execute(route, argv, request.into_body(), permit).await
}
