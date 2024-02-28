mod memorystreams;

use std::{collections::BTreeSet, sync::{Arc, Mutex}};
use std::ops::Bound::{Unbounded, Excluded, Included, self};

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, response},
    response::{IntoResponse, Response,},
    routing::any,
    Router,
};

use crate::memorystreams::DbItemStream;

const MAX_PATH_LEN: usize = 32 * 1024; // maximum accepted path length for a file
const INIT_LIST_LEN: usize = 8 * 1024 * 1024; // pre allocated string length for list of file using LIST
const LISTEN_ADRESS: &str = "0.0.0.0:3000"; // address and port the server will listen on

#[derive(Clone, Default)]
struct AppState {
    file_list: Arc<Mutex<BTreeSet<String>>>,
}

#[tokio::main]
async fn main() {
    memorystreams::init_db();
    let router = Router::new()
        .route("/:path", any(list_handler).get(get_handler).put(put_handler).delete(delete_handler))
        .route("/", any(root_folder_handle))
        .with_state(AppState::default());
    let listener = tokio::net::TcpListener::bind(LISTEN_ADRESS).await.unwrap();
    axum::serve(listener, router).await.unwrap();
}

// parses Range header for GET request, accepts "Range: bytes=X-Y"
fn get_request_range(headers: HeaderMap) -> (usize, usize) {
    let (mut start, mut end) = (0, usize::MAX);
    match headers.get(header::RANGE) {
        None => {}
        Some(value) => { // some clumsy parsing of a Range header
            let str = value.to_str().unwrap_or("");
            let mut range = str.split('=').last().unwrap_or("").split('-');
            start = range.next().unwrap_or("").parse::<usize>().unwrap_or(0);
            end = range.next().unwrap_or("").parse::<usize>().unwrap_or(usize::MAX);
        }
    }
    (start, end)
}

// handles GET requests
async fn get_handler(
    headers: HeaderMap,
    Path(path): Path<String>,
    _request: Request,
) -> Response {
    if path.len() > MAX_PATH_LEN {
        return Ok::<StatusCode, StatusCode>(StatusCode::URI_TOO_LONG).into_response();
    }
    let (item, id, _is_chunked, content_type) = memorystreams::get_item(path).await;
    if id == usize::MAX { // means file was not found
        return (StatusCode::NOT_FOUND).into_response();
    }
    let (start, end) = get_request_range(headers);
    let stream = DbItemStream::new(item, id, start, end);
    let header = response::Builder::new()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::TRANSFER_ENCODING, "chunked")
        .header(header::ACCEPT_RANGES, "bytes");
    let response = header
        .body(Body::from_stream(stream)).unwrap();
    return response.into_response();
}

// handles PUT requests
async fn put_handler(
    mut headers: HeaderMap,
    State(state): State<AppState>,
    Path(path): Path<String>,
    request: Request,
) -> Response {
    if path.len() > MAX_PATH_LEN {
        return Ok::<StatusCode, StatusCode>(StatusCode::URI_TOO_LONG).into_response();
    }
    let content_type = headers.entry(header::CONTENT_TYPE)
        .or_insert(HeaderValue::from_str("application/octet-stream").unwrap());
    let path_copy = path.clone() + "\n";
    tokio::spawn(async move {
        state.file_list.lock().unwrap().insert(path_copy);
    }); 
    unsafe {
        let status = memorystreams::from_stream_to_item(request.into_body().into_data_stream(), path, content_type.to_owned()).await; 
        Ok::<StatusCode, StatusCode>(status).into_response()
    }
}

// handles DELETE requests
async fn delete_handler(
    _headers: HeaderMap,
    State(state): State<AppState>,
    Path(path): Path<String>,
    _request: Request,
) -> Response {
    if path.len() > MAX_PATH_LEN {
        return Ok::<StatusCode, StatusCode>(StatusCode::URI_TOO_LONG).into_response();
    }
    let path_copy = path.clone() + "\n";
    tokio::spawn(async move {
        state.file_list.lock().unwrap().remove(&path_copy);
    }); 
    let code = memorystreams::delete_item(path).await;
    Ok::<StatusCode, StatusCode>(code).into_response()
}

// creates lexicographically next string to use as an upper bound for search in LIST requests
fn get_range_bounds(mut next_path: String) -> (Bound<String>, Bound<String>) {
    let begin = Included(next_path.clone());
    let mut end = Unbounded;
    while next_path.len() > 0 {
        let x = next_path.pop().unwrap();
        if x as u8 != 255 {
            next_path.push((x as u8 + 1) as char);
            break;
        }
    }
    if next_path.len() != 0 {
        end = Excluded(next_path);
    }
    (begin, end)
}

// gets file list for LIST requests
fn get_file_list(state: AppState, range: (Bound<String>, Bound<String>)) -> String {
    let set = state.file_list.lock().unwrap();
    let mut list = String::new();

    let range = set.range(range);
    list.reserve(INIT_LIST_LEN);
    for name in range {
        list += name;
    }
    list
}

// handles LIST requests
async fn list_handler(
    method: Method,
    _headers: HeaderMap,
    State(state): State<AppState>,
    Path(path): Path<String>,
    _request: Request,
) -> Response {
    if method.as_str() != "LIST" {
        return Ok::<StatusCode, StatusCode>(StatusCode::NOT_IMPLEMENTED).into_response();
    }
    if path.len() > MAX_PATH_LEN {
        return Ok::<StatusCode, StatusCode>(StatusCode::URI_TOO_LONG).into_response();
    }
    let (begin, end) = get_range_bounds(path);
    let list = get_file_list(state, (begin, end));
    
    let header = response::Builder::new()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8");
    let response = header
        .body(Body::from(list)).unwrap();
    response
}

// forbids access to "/" and returns all files for LIST "/"
async fn root_folder_handle(
    method: Method,
    State(state): State<AppState>
) -> Response {
    if method.as_str() == "LIST" {
        let list = get_file_list(state, (Unbounded, Unbounded));
        let header = response::Builder::new()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8");
        let response = header
            .body(Body::from(list)).unwrap();
        return response;
    }
    Ok::<StatusCode, StatusCode>(StatusCode::FORBIDDEN).into_response()
} 