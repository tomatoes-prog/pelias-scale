//! Manticore Search backend for country-scale geosearch benchmarks.
//!
//! The adapter talks to Manticore's HTTP SQL endpoint (`/sql`) and keeps the
//! ingestion API batch-oriented so callers such as `geo-ingest` can stream CSV
//! rows into small `bulk_index` calls instead of materializing a whole country.

use geo_core::{
    AddressParts, AdminHierarchy, AutocompleteQuery, BackendError, ForwardQuery, GeoDocument,
    Point, ReverseQuery, SchemaDefinition, SearchBackend, SearchHit, bounded_limit,
    bounding_box_for_radius, haversine_meters,
};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

const DEFAULT_ENDPOINT: &str = "http://localhost:9308/sql";
const DEFAULT_TABLE_NAME: &str = "geo";
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_BATCH_SIZE: usize = 500;
const TEXT_FIELDS: [&str; 7] = [
    "name", "label", "aliases", "street", "locality", "region", "country",
];

/// Connection and indexing settings for the Manticore backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManticoreBackendConfig {
    pub endpoint: String,
    pub table_name: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub batch_size: usize,
}

impl Default for ManticoreBackendConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_string(),
            table_name: DEFAULT_TABLE_NAME.to_string(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

impl ManticoreBackendConfig {
    pub fn new(endpoint: impl Into<String>, table_name: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            table_name: table_name.into(),
            ..Self::default()
        }
    }
}

/// Manticore adapter backed by the HTTP SQL API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManticoreBackend {
    config: ManticoreBackendConfig,
}

impl ManticoreBackend {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self::with_config(ManticoreBackendConfig {
            endpoint: endpoint.into(),
            ..ManticoreBackendConfig::default()
        })
        .expect("default Manticore HTTP client configuration should be valid")
    }

    pub fn with_config(config: ManticoreBackendConfig) -> Result<Self, BackendError> {
        HttpEndpoint::parse(&config.endpoint)?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &ManticoreBackendConfig {
        &self.config
    }

    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }

    fn execute_sql(&self, sql: &str) -> Result<Value, BackendError> {
        let text = post_sql_form(
            &self.config.endpoint,
            sql,
            self.config.connect_timeout,
            self.config.request_timeout,
        )?;
        let value = serde_json::from_str::<Value>(&text).map_err(|error| {
            BackendError::new(format!(
                "invalid Manticore SQL JSON response: {error}; body={text}"
            ))
        })?;
        manticore_error(&value).map_or(Ok(value), Err)
    }

    fn table_for_country(&self, country_code: &str) -> Result<String, BackendError> {
        let country = country_code.trim().to_ascii_lowercase();
        if country.is_empty() {
            return Err(BackendError::new(
                "country code is required for a country RT table",
            ));
        }
        if self.config.table_name.contains("{country}") {
            sanitize_identifier(&self.config.table_name.replace("{country}", &country))
        } else if self
            .config
            .table_name
            .to_ascii_lowercase()
            .ends_with(&format!("_{country}"))
        {
            sanitize_identifier(&self.config.table_name)
        } else {
            sanitize_identifier(&format!("{}_{}", self.config.table_name, country))
        }
    }

    fn table_for_query(&self, country_code: Option<&str>) -> Result<String, BackendError> {
        country_code.map_or_else(
            || sanitize_identifier(&self.config.table_name),
            |country| self.table_for_country(country),
        )
    }

    fn select_hits(&self, sql: &str) -> Result<Vec<SearchHit>, BackendError> {
        let value = self.execute_sql(sql)?;
        let rows = extract_rows(&value)?;
        rows.iter().map(row_to_hit).collect()
    }
}

impl SearchBackend for ManticoreBackend {
    fn name(&self) -> &'static str {
        "manticore"
    }

    fn create_schema(&self, schema: &SchemaDefinition) -> Result<(), BackendError> {
        let table = self.table_for_country(&schema.country_code)?;
        let text_columns = TEXT_FIELDS
            .iter()
            .map(|field| format!("{field} text"))
            .collect::<Vec<_>>()
            .join(", ");
        self.execute_sql(&format!(
            "CREATE TABLE IF NOT EXISTS {table} (\
                {text_columns}, \
                external_id string attribute indexed, \
                source string attribute indexed, \
                layer string attribute indexed, \
                country_code string attribute indexed, \
                lat float, \
                lon float, \
                popularity float, \
                house_number string attribute, \
                postal_code string attribute, \
                county string attribute, \
                neighbourhood string attribute\
            ) min_infix_len='2' charset_table='non_cjk'"
        ))?;
        Ok(())
    }

    fn bulk_index(&self, docs: &[GeoDocument]) -> Result<(), BackendError> {
        for batch in docs.chunks(self.config.batch_size.max(1)) {
            let Some(first) = batch.first() else {
                continue;
            };
            let table = self.table_for_country(&first.country_code)?;
            let mut sql = String::from("REPLACE INTO ");
            sql.push_str(&table);
            sql.push_str(" (id, name, label, aliases, street, locality, region, country, external_id, source, layer, country_code, lat, lon, popularity, house_number, postal_code, county, neighbourhood) VALUES ");
            for (index, doc) in batch.iter().enumerate() {
                if doc.country_code != first.country_code {
                    return Err(BackendError::new(
                        "bulk_index batches must not mix country codes for per-country tables",
                    ));
                }
                if index > 0 {
                    sql.push_str(", ");
                }
                push_document_values(&mut sql, doc);
            }
            self.execute_sql(&sql)?;
        }
        Ok(())
    }

    fn forward(&self, query: &ForwardQuery) -> Result<Vec<SearchHit>, BackendError> {
        let text = query.text.trim();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let table = self.table_for_query(query.country_code.as_deref())?;
        let limit = bounded_limit(query.limit);
        let country_filter = country_filter(query.country_code.as_deref());
        let match_expr = sql_string(&match_query(text, false));
        let order = if let Some(focus) = query.focus {
            format!(
                "(WEIGHT() + popularity * 10 - ((lat - {lat}) * (lat - {lat}) + (lon - {lon}) * (lon - {lon})) * 1000) DESC",
                lat = focus.latitude,
                lon = focus.longitude
            )
        } else {
            "(WEIGHT() + popularity * 10) DESC".to_string()
        };
        let sql = format!(
            "SELECT {select_fields}, WEIGHT() text_score FROM {table} WHERE MATCH({match_expr}){country_filter} ORDER BY {order} LIMIT {limit}",
            select_fields = select_fields()
        );
        let mut hits = self.select_hits(&sql)?;
        if let Some(focus) = query.focus {
            for hit in &mut hits {
                let distance = haversine_meters(
                    focus,
                    Point {
                        latitude: hit.document.latitude,
                        longitude: hit.document.longitude,
                    },
                );
                hit.distance_meters = Some(distance);
                hit.score -= distance as f32 / 100_000.0;
            }
        }
        Ok(hits)
    }

    fn autocomplete(&self, query: &AutocompleteQuery) -> Result<Vec<SearchHit>, BackendError> {
        let text = query.text.trim();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let table = self.table_for_query(query.country_code.as_deref())?;
        let limit = bounded_limit(query.limit);
        let country_filter = country_filter(query.country_code.as_deref());
        let match_expr = sql_string(&match_query(text, true));
        let sql = format!(
            "SELECT {select_fields}, WEIGHT() text_score FROM {table} WHERE MATCH({match_expr}){country_filter} ORDER BY (WEIGHT() + popularity * 10) DESC LIMIT {limit}",
            select_fields = select_fields()
        );
        self.select_hits(&sql)
    }

    fn reverse(&self, query: &ReverseQuery) -> Result<Vec<SearchHit>, BackendError> {
        let table = self.table_for_query(None)?;
        let bbox = bounding_box_for_radius(query.point, query.radius_meters);
        let limit = bounded_limit(query.limit);
        let candidate_limit = (limit * 10).max(50);
        let sql = format!(
            "SELECT {select_fields}, (((lat - {lat}) * (lat - {lat})) + ((lon - {lon}) * (lon - {lon}))) distance_degrees \
             FROM {table} WHERE lat BETWEEN {min_lat} AND {max_lat} AND lon BETWEEN {min_lon} AND {max_lon} \
             ORDER BY distance_degrees ASC, popularity DESC LIMIT {candidate_limit}",
            select_fields = select_fields(),
            lat = query.point.latitude,
            lon = query.point.longitude,
            min_lat = bbox.min_lat,
            max_lat = bbox.max_lat,
            min_lon = bbox.min_lon,
            max_lon = bbox.max_lon,
        );
        let mut hits = self.select_hits(&sql)?;
        for hit in &mut hits {
            let distance = haversine_meters(
                query.point,
                Point {
                    latitude: hit.document.latitude,
                    longitude: hit.document.longitude,
                },
            );
            hit.distance_meters = Some(distance);
            hit.score = hit.document.popularity - distance as f32 / 10_000.0;
        }
        hits.retain(|hit| {
            hit.distance_meters
                .is_some_and(|distance| distance <= f64::from(query.radius_meters))
        });
        hits.sort_by(|a, b| {
            a.distance_meters
                .partial_cmp(&b.distance_meters)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        hits.truncate(limit);
        Ok(hits)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HttpEndpoint {
    host: String,
    port: u16,
    path: String,
}

impl HttpEndpoint {
    fn parse(endpoint: &str) -> Result<Self, BackendError> {
        let without_scheme = endpoint
            .strip_prefix("http://")
            .ok_or_else(|| BackendError::new("Manticore endpoint must use http://"))?;
        let (authority, path) = without_scheme
            .split_once('/')
            .map_or((without_scheme, "/"), |(authority, path)| (authority, path));
        let (host, port) = authority
            .rsplit_once(':')
            .and_then(|(host, port)| Some((host, port.parse::<u16>().ok()?)))
            .unwrap_or((authority, 80));
        if host.is_empty() {
            return Err(BackendError::new("Manticore endpoint host is required"));
        }
        Ok(Self {
            host: host.to_string(),
            port,
            path: if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            },
        })
    }

    fn socket_addr(&self) -> Result<SocketAddr, BackendError> {
        (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(to_backend_error)?
            .next()
            .ok_or_else(|| BackendError::new("Manticore endpoint did not resolve to an address"))
    }
}

fn post_sql_form(
    endpoint: &str,
    sql: &str,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<String, BackendError> {
    let endpoint = HttpEndpoint::parse(endpoint)?;
    let mut stream = TcpStream::connect_timeout(&endpoint.socket_addr()?, connect_timeout)
        .map_err(to_backend_error)?;
    stream
        .set_read_timeout(Some(request_timeout))
        .map_err(to_backend_error)?;
    stream
        .set_write_timeout(Some(request_timeout))
        .map_err(to_backend_error)?;

    let body = format!("mode=raw&query={}", form_encode(sql));
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}",
        path = endpoint.path,
        host = endpoint.host,
        port = endpoint.port,
        length = body.len(),
    );
    stream
        .write_all(request.as_bytes())
        .map_err(to_backend_error)?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(to_backend_error)?;
    let response = String::from_utf8(response).map_err(to_backend_error)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| BackendError::new("invalid HTTP response from Manticore"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| BackendError::new("invalid HTTP status from Manticore"))?;
    if !(200..300).contains(&status) {
        return Err(BackendError::new(format!(
            "Manticore SQL request failed with HTTP {status}: {body}"
        )));
    }
    Ok(body.to_string())
}

fn form_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char);
            }
            b' ' => output.push('+'),
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}

fn select_fields() -> &'static str {
    "external_id, source, layer, country_code, name, label, aliases, street, locality, region, country, lat, lon, popularity, house_number, postal_code, county, neighbourhood"
}

fn push_document_values(sql: &mut String, doc: &GeoDocument) {
    sql.push('(');
    sql.push_str(&stable_doc_id(&doc.id).to_string());
    for value in [
        doc.name.as_str(),
        doc.label.as_str(),
        &doc.aliases.join(" | "),
        doc.address.street.as_deref().unwrap_or_default(),
        doc.admin_hierarchy.locality.as_deref().unwrap_or_default(),
        doc.admin_hierarchy.region.as_deref().unwrap_or_default(),
        doc.admin_hierarchy.country.as_deref().unwrap_or_default(),
        doc.id.as_str(),
        doc.source.as_str(),
        doc.layer.as_str(),
        doc.country_code.as_str(),
    ] {
        sql.push_str(", ");
        sql.push_str(&sql_string(value));
    }
    for value in [doc.latitude, doc.longitude, f64::from(doc.popularity)] {
        sql.push_str(", ");
        sql.push_str(&value.to_string());
    }
    for value in [
        doc.address.house_number.as_deref().unwrap_or_default(),
        doc.address.postal_code.as_deref().unwrap_or_default(),
        doc.admin_hierarchy.county.as_deref().unwrap_or_default(),
        doc.admin_hierarchy
            .neighbourhood
            .as_deref()
            .unwrap_or_default(),
    ] {
        sql.push_str(", ");
        sql.push_str(&sql_string(value));
    }
    sql.push(')');
}

fn row_to_hit(row: &Value) -> Result<SearchHit, BackendError> {
    let document = GeoDocument {
        id: string_value(row, "external_id"),
        source: string_value(row, "source"),
        layer: string_value(row, "layer"),
        name: string_value(row, "name"),
        label: string_value(row, "label"),
        country_code: string_value(row, "country_code"),
        latitude: f64_value(row, "lat"),
        longitude: f64_value(row, "lon"),
        popularity: f64_value(row, "popularity") as f32,
        admin_hierarchy: AdminHierarchy {
            country: option_string_value(row, "country"),
            region: option_string_value(row, "region"),
            county: option_string_value(row, "county"),
            locality: option_string_value(row, "locality"),
            neighbourhood: option_string_value(row, "neighbourhood"),
        },
        address: AddressParts {
            house_number: option_string_value(row, "house_number"),
            street: option_string_value(row, "street"),
            postal_code: option_string_value(row, "postal_code"),
        },
        aliases: string_value(row, "aliases")
            .split('|')
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    };
    let score = f64_value(row, "text_score") as f32 + document.popularity;
    Ok(SearchHit {
        document,
        score,
        distance_meters: None,
    })
}

fn extract_rows(value: &Value) -> Result<&[Value], BackendError> {
    if let Some(rows) = value.get("data").and_then(Value::as_array) {
        return Ok(rows);
    }
    if let Some(rows) = value
        .as_array()
        .and_then(|items| items.iter().find_map(|item| item.get("data")?.as_array()))
    {
        return Ok(rows);
    }
    Err(BackendError::new(format!(
        "Manticore response does not contain a data array: {value}"
    )))
}

fn manticore_error(value: &Value) -> Option<BackendError> {
    if let Some(error) = value
        .get("error")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
    {
        return Some(BackendError::new(error.to_string()));
    }
    value.as_array().and_then(|items| {
        items.iter().find_map(|item| {
            item.get("error")
                .and_then(Value::as_str)
                .filter(|error| !error.is_empty())
                .map(|error| BackendError::new(error.to_string()))
        })
    })
}

fn match_query(text: &str, autocomplete: bool) -> String {
    let terms = text
        .split_whitespace()
        .map(escape_match_term)
        .collect::<Vec<_>>();
    if !autocomplete {
        return terms.join(" ");
    }
    terms
        .iter()
        .enumerate()
        .map(|(index, term)| {
            if index + 1 == terms.len() {
                if term.chars().count() >= 2 {
                    format!("*{term}*")
                } else {
                    format!("{term}*")
                }
            } else {
                term.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn escape_match_term(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            !matches!(
                character,
                '\'' | '"'
                    | '\\'
                    | '/'
                    | '('
                    | ')'
                    | '|'
                    | '-'
                    | '!'
                    | '@'
                    | '~'
                    | '&'
                    | '^'
                    | '$'
                    | '='
                    | '<'
            )
        })
        .collect::<String>()
}

fn country_filter(country_code: Option<&str>) -> String {
    country_code.map_or_else(String::new, |country| {
        format!(" AND country_code = {}", sql_string(country))
    })
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn sanitize_identifier(value: &str) -> Result<String, BackendError> {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !value.is_empty()
    {
        Ok(value.to_string())
    } else {
        Err(BackendError::new(format!(
            "invalid Manticore identifier: {value}"
        )))
    }
}

fn stable_doc_id(value: &str) -> u64 {
    // FNV-1a gives Manticore a deterministic numeric document id while the
    // original geocoder id is preserved in the `external_id` string attribute.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash.max(1)
}

fn string_value(row: &Value, key: &str) -> String {
    row.get(key).and_then(Value::as_str).map_or_else(
        || row.get(key).map_or_else(String::new, ToString::to_string),
        ToOwned::to_owned,
    )
}

fn option_string_value(row: &Value, key: &str) -> Option<String> {
    let value = string_value(row, key);
    if value.is_empty() { None } else { Some(value) }
}

fn f64_value(row: &Value, key: &str) -> f64 {
    row.get(key)
        .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
        .unwrap_or_default()
}

fn to_backend_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_names_are_country_scoped_and_sanitized() {
        let backend = ManticoreBackend::new("http://localhost:9308/sql");
        assert_eq!(backend.table_for_country("CO").unwrap(), "geo_co");

        let backend = ManticoreBackend::with_config(ManticoreBackendConfig::new(
            "http://localhost:9308/sql",
            "places_{country}",
        ))
        .unwrap();
        assert_eq!(backend.table_for_country("MX").unwrap(), "places_mx");

        let backend = ManticoreBackend::with_config(ManticoreBackendConfig::new(
            "http://localhost:9308/sql",
            "geo_co",
        ))
        .unwrap();
        assert_eq!(backend.table_for_country("CO").unwrap(), "geo_co");
        assert!(backend.table_for_country("C-O").is_err());
    }

    #[test]
    fn sql_strings_escape_quotes_and_backslashes() {
        assert_eq!(sql_string("O'Hare \\ test"), "'O\\'Hare \\\\ test'");
    }

    #[test]
    fn autocomplete_uses_infix_for_last_token() {
        assert_eq!(match_query("bog san", true), "bog *san*");
        assert_eq!(match_query("x", true), "x*");
    }

    fn sample_document() -> GeoDocument {
        GeoDocument {
            id: "wof:locality:bogota".to_string(),
            source: "whosonfirst".to_string(),
            layer: "locality".to_string(),
            name: "Bogotá".to_string(),
            label: "Bogotá, Colombia".to_string(),
            country_code: "CO".to_string(),
            latitude: 4.711,
            longitude: -74.0721,
            admin_hierarchy: AdminHierarchy {
                country: Some("Colombia".to_string()),
                region: Some("Bogotá D.C.".to_string()),
                ..AdminHierarchy::default()
            },
            address: AddressParts::default(),
            aliases: vec!["Bogota".to_string()],
            popularity: 10.0,
        }
    }

    #[test]
    fn document_values_include_reconstructable_fields() {
        let doc = sample_document();
        let mut sql = String::new();
        push_document_values(&mut sql, &doc);
        assert!(sql.contains("'wof:locality:bogota'"));
        assert!(sql.contains("'Bogotá'"));
        assert!(sql.contains("'Bogota'"));
        assert!(sql.contains("4.711"));
    }

    #[cfg(feature = "manticore-integration")]
    #[test]
    fn indexes_and_queries_running_manticore() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let table_name = format!("geo_manticore_test_{suffix}_co");
        let backend = ManticoreBackend::with_config(ManticoreBackendConfig::new(
            std::env::var("MANTICORE_SQL_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:9308/sql".to_string()),
            table_name.clone(),
        ))
        .unwrap();
        backend
            .create_schema(&SchemaDefinition {
                country_code: "CO".to_string(),
                languages: vec!["es".to_string()],
                layers: vec!["locality".to_string()],
            })
            .unwrap();
        backend.bulk_index(&[sample_document()]).unwrap();

        assert_eq!(
            backend
                .forward(&ForwardQuery {
                    text: "Bogota".to_string(),
                    country_code: Some("CO".to_string()),
                    focus: None,
                    limit: 5,
                })
                .unwrap()[0]
                .document
                .id,
            "wof:locality:bogota"
        );
        assert!(
            backend
                .reverse(&ReverseQuery {
                    point: Point {
                        latitude: 4.711,
                        longitude: -74.0721,
                    },
                    radius_meters: 1_000,
                    limit: 5,
                })
                .unwrap()
                .iter()
                .any(|hit| hit.document.id == "wof:locality:bogota")
        );
        backend
            .execute_sql(&format!("DROP TABLE IF EXISTS {table_name}"))
            .unwrap();
    }
}
