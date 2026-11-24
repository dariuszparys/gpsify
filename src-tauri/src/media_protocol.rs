//! Custom `gpsify://` protocol serving lazily-generated JPEG thumbs and previews
//! strictly by active `job_id` + `photo_id` UUID (no arbitrary filesystem paths, K16).

use tauri::http::{header, Request, Response, StatusCode};
use tauri::{Manager, Runtime};

use gpsify::review::routes::media_jpeg_bytes;

use crate::state::DesktopState;

pub fn handle_media_request<R: Runtime>(
    ctx: tauri::UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
    responder: tauri::UriSchemeResponder,
) {
    let app = ctx.app_handle().clone();
    let path = request.uri().path().to_string();

    tauri::async_runtime::spawn(async move {
        let resp = build_media_response(&app, &path).await;
        responder.respond(resp);
    });
}

fn plain_error(status: StatusCode, msg: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(msg.as_bytes().to_vec())
        .unwrap_or_else(|_| Response::new(Vec::new()))
}

async fn build_media_response<R: Runtime>(
    app: &tauri::AppHandle<R>,
    raw_path: &str,
) -> Response<Vec<u8>> {
    let parts: Vec<&str> = raw_path.trim_start_matches('/').split('/').collect();
    if parts.len() != 3 {
        return plain_error(StatusCode::BAD_REQUEST, "invalid media URL");
    }
    let is_preview = match parts[0] {
        "thumb" => false,
        "preview" => true,
        _ => return plain_error(StatusCode::NOT_FOUND, "unknown media kind"),
    };
    let job_id = parts[1];
    let photo_id = parts[2];
    if job_id.is_empty() || photo_id.is_empty() {
        return plain_error(StatusCode::BAD_REQUEST, "missing job_id or photo_id");
    }

    let state = app.state::<DesktopState>();
    let Ok(review_state) = state.active_state().await else {
        return plain_error(StatusCode::NOT_FOUND, "no active job");
    };
    if review_state.job_id != job_id {
        return plain_error(StatusCode::NOT_FOUND, "stale or mismatched job_id");
    }

    match media_jpeg_bytes(&review_state, photo_id, is_preview).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "no-store")
            .body(bytes)
            .unwrap_or_else(|_| Response::new(Vec::new())),
        Err((code, msg)) => {
            let status = StatusCode::from_u16(code.as_u16()).unwrap_or(StatusCode::NOT_FOUND);
            plain_error(status, &msg)
        }
    }
}
