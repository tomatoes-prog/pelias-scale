use geo_backend_local::LocalBackend;
use geo_core::{
    AutocompleteQuery, Bounds, CountryConfig, ForwardQuery, Point, ReverseQuery, SearchBackend,
    SearchConfig, SearchHit,
};
use std::collections::HashMap;
use std::env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;

#[derive(Clone)]
struct AppState {
    config: CountryConfig,
    index: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("geo-api: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let flags = parse_flags(&env::args().skip(1).collect::<Vec<_>>())?;
    let config_path = flags
        .get("config")
        .cloned()
        .or_else(|| env::var("GEO_CONFIG").ok())
        .ok_or("missing --config or GEO_CONFIG")?;
    let index = flags
        .get("index")
        .cloned()
        .or_else(|| env::var("GEO_INDEX").ok())
        .ok_or("missing --index or GEO_INDEX")?;
    let bind = flags
        .get("bind")
        .cloned()
        .or_else(|| env::var("GEO_BIND").ok())
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let state = Arc::new(AppState {
        config: read_country_config(Path::new(&config_path))?,
        index,
    });
    let listener = TcpListener::bind(&bind).map_err(|error| format!("binding {bind}: {error}"))?;
    println!("geo-api listening on {bind}");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle_connection(stream, state));
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, state: Arc<AppState>) {
    let mut buffer = [0_u8; 8192];
    let Ok(read) = stream.read(&mut buffer) else {
        return;
    };
    let request = String::from_utf8_lossy(&buffer[..read]);
    let Some(first_line) = request.lines().next() else {
        return;
    };
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" {
        write_response(&mut stream, 405, "{\"error\":\"method not allowed\"}");
        return;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let params = parse_query(query);
    let result = match path {
        "/healthz" => Ok("{\"ok\":true}".to_string()),
        "/v1/search" => search(&state, &params),
        "/v1/autocomplete" => autocomplete(&state, &params),
        "/v1/reverse" => reverse(&state, &params),
        _ => Err("not found".to_string()),
    };
    match result {
        Ok(body) => write_response(&mut stream, 200, &body),
        Err(error) if error == "not found" => {
            write_response(&mut stream, 404, "{\"error\":\"not found\"}")
        }
        Err(error) => write_response(
            &mut stream,
            500,
            &format!("{{\"error\":\"{}\"}}", json_escape(&error)),
        ),
    }
}

fn search(state: &AppState, params: &HashMap<String, String>) -> Result<String, String> {
    let text = params
        .get("query")
        .or_else(|| params.get("text"))
        .cloned()
        .unwrap_or_default();
    let limit = params
        .get("limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(state.config.search.default_limit);
    let focus = match (params.get("focus_lat"), params.get("focus_lon")) {
        (Some(lat), Some(lon)) => Some(Point {
            latitude: lat.parse::<f64>().map_err(|error| error.to_string())?,
            longitude: lon.parse::<f64>().map_err(|error| error.to_string())?,
        }),
        _ => None,
    };
    let backend = LocalBackend::open(&state.index).map_err(|error| error.to_string())?;
    let hits = backend
        .forward(&ForwardQuery {
            text,
            country_code: Some(state.config.country_code.clone()),
            focus,
            limit,
        })
        .map_err(|error| error.to_string())?;
    Ok(response_json(&hits))
}

fn autocomplete(state: &AppState, params: &HashMap<String, String>) -> Result<String, String> {
    let text = params.get("text").cloned().unwrap_or_default();
    let limit = params
        .get("limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(state.config.search.autocomplete_limit);
    let backend = LocalBackend::open(&state.index).map_err(|error| error.to_string())?;
    let hits = backend
        .autocomplete(&AutocompleteQuery {
            text,
            country_code: Some(state.config.country_code.clone()),
            limit,
        })
        .map_err(|error| error.to_string())?;
    Ok(response_json(&hits))
}

fn reverse(state: &AppState, params: &HashMap<String, String>) -> Result<String, String> {
    let lat = params
        .get("lat")
        .ok_or("missing lat")?
        .parse::<f64>()
        .map_err(|error| error.to_string())?;
    let lon = params
        .get("lon")
        .ok_or("missing lon")?
        .parse::<f64>()
        .map_err(|error| error.to_string())?;
    let limit = params
        .get("limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(state.config.search.default_limit);
    let radius_meters = params
        .get("radius_meters")
        .and_then(|value| value.parse().ok())
        .unwrap_or(state.config.search.reverse_radius_meters);
    let backend = LocalBackend::open(&state.index).map_err(|error| error.to_string())?;
    let hits = backend
        .reverse(&ReverseQuery {
            point: Point {
                latitude: lat,
                longitude: lon,
            },
            radius_meters,
            limit,
        })
        .map_err(|error| error.to_string())?;
    Ok(response_json(&hits))
}

fn response_json(hits: &[SearchHit]) -> String {
    let features = hits.iter().map(feature_json).collect::<Vec<_>>().join(",");
    format!("{{\"type\":\"FeatureCollection\",\"features\":[{features}]}}")
}

fn feature_json(hit: &SearchHit) -> String {
    let distance = hit
        .distance_meters
        .map(|value| format!(",\"distance_meters\":{value:.2}"))
        .unwrap_or_default();
    format!(
        "{{\"type\":\"Feature\",\"geometry\":{{\"type\":\"Point\",\"coordinates\":[{},{}]}},\"properties\":{{\"id\":\"{}\",\"source\":\"{}\",\"layer\":\"{}\",\"name\":\"{}\",\"label\":\"{}\",\"country_code\":\"{}\",\"score\":{}{}}}}}",
        hit.document.longitude,
        hit.document.latitude,
        json_escape(&hit.document.id),
        json_escape(&hit.document.source),
        json_escape(&hit.document.layer),
        json_escape(&hit.document.name),
        json_escape(&hit.document.label),
        json_escape(&hit.document.country_code),
        hit.score,
        distance
    )
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (url_decode(key), url_decode(value)))
        .collect()
}

fn parse_flags(args: &[String]) -> Result<HashMap<String, String>, String> {
    let mut flags = HashMap::new();
    let mut index = 0;
    while index < args.len() {
        let key = args[index]
            .strip_prefix("--")
            .ok_or_else(|| format!("expected --flag, got {}", args[index]))?;
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("missing value for --{key}"))?;
        flags.insert(key.to_string(), value.to_string());
        index += 2;
    }
    Ok(flags)
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn url_decode(value: &str) -> String {
    let mut output = String::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => output.push(' '),
            b'%' if index + 2 < bytes.len() => {
                if let Ok(hex) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                    output.push(hex as char);
                    index += 2;
                } else {
                    output.push('%');
                }
            }
            byte => output.push(byte as char),
        }
        index += 1;
    }
    output
}

fn read_country_config(path: &Path) -> Result<CountryConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    let values = parse_simple_toml(&text);
    Ok(CountryConfig {
        country_code: required_value(&values, "country_code")?.to_string(),
        country_name: required_value(&values, "country_name")?.to_string(),
        languages: parse_array(required_value(&values, "languages")?),
        timezone: required_value(&values, "timezone")?.to_string(),
        bounds: Bounds {
            min_lon: parse_f64(&values, "bounds.min_lon")?,
            min_lat: parse_f64(&values, "bounds.min_lat")?,
            max_lon: parse_f64(&values, "bounds.max_lon")?,
            max_lat: parse_f64(&values, "bounds.max_lat")?,
        },
        search: SearchConfig {
            default_limit: parse_usize(&values, "search.default_limit")?,
            autocomplete_limit: parse_usize(&values, "search.autocomplete_limit")?,
            reverse_radius_meters: parse_u32(&values, "search.reverse_radius_meters")?,
            preferred_layers: parse_array(required_value(&values, "search.preferred_layers")?),
        },
    })
}

fn parse_simple_toml(text: &str) -> HashMap<String, String> {
    let mut section = String::new();
    let mut values = HashMap::new();
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(&['[', ']'][..]).to_string();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let full_key = if section.is_empty() {
                key.trim().to_string()
            } else {
                format!("{}.{}", section, key.trim())
            };
            values.insert(full_key, value.trim().trim_matches('"').to_string());
        }
    }
    values
}

fn required_value<'a>(values: &'a HashMap<String, String>, key: &str) -> Result<&'a str, String> {
    values
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("missing config key {key}"))
}
fn parse_array(value: &str) -> Vec<String> {
    value
        .trim_matches(&['[', ']'][..])
        .split(',')
        .map(|item| item.trim().trim_matches('"'))
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}
fn parse_f64(values: &HashMap<String, String>, key: &str) -> Result<f64, String> {
    required_value(values, key)?
        .parse::<f64>()
        .map_err(|error| error.to_string())
}
fn parse_usize(values: &HashMap<String, String>, key: &str) -> Result<usize, String> {
    required_value(values, key)?
        .parse::<usize>()
        .map_err(|error| error.to_string())
}
fn parse_u32(values: &HashMap<String, String>, key: &str) -> Result<u32, String> {
    required_value(values, key)?
        .parse::<u32>()
        .map_err(|error| error.to_string())
}
