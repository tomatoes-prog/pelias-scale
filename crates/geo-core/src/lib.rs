//! Core types and dependency-free utilities for the single-country geosearch engine.

/// A normalized document ready to be indexed in any supported search backend.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoDocument {
    pub id: String,
    pub source: String,
    pub layer: String,
    pub name: String,
    pub label: String,
    pub country_code: String,
    pub latitude: f64,
    pub longitude: f64,
    pub admin_hierarchy: AdminHierarchy,
    pub address: AddressParts,
    pub aliases: Vec<String>,
    pub popularity: f32,
}

impl GeoDocument {
    pub fn searchable_text(&self) -> String {
        let mut parts = Vec::with_capacity(12 + self.aliases.len());
        push_non_empty(&mut parts, &self.name);
        push_non_empty(&mut parts, &self.label);
        for alias in &self.aliases {
            push_non_empty(&mut parts, alias);
        }
        push_optional(&mut parts, self.address.house_number.as_deref());
        push_optional(&mut parts, self.address.street.as_deref());
        push_optional(&mut parts, self.admin_hierarchy.neighbourhood.as_deref());
        push_optional(&mut parts, self.admin_hierarchy.locality.as_deref());
        push_optional(&mut parts, self.admin_hierarchy.county.as_deref());
        push_optional(&mut parts, self.admin_hierarchy.region.as_deref());
        push_optional(&mut parts, self.admin_hierarchy.country.as_deref());
        parts.join(" ")
    }

    pub fn ensure_label(&mut self) {
        if !self.label.trim().is_empty() {
            return;
        }
        let mut parts = Vec::with_capacity(5);
        push_non_empty(&mut parts, &self.name);
        push_optional(&mut parts, self.admin_hierarchy.locality.as_deref());
        push_optional(&mut parts, self.admin_hierarchy.region.as_deref());
        push_optional(&mut parts, self.admin_hierarchy.country.as_deref());
        self.label = parts.join(", ");
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AdminHierarchy {
    pub country: Option<String>,
    pub region: Option<String>,
    pub county: Option<String>,
    pub locality: Option<String>,
    pub neighbourhood: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AddressParts {
    pub house_number: Option<String>,
    pub street: Option<String>,
    pub postal_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForwardQuery {
    pub text: String,
    pub country_code: Option<String>,
    pub focus: Option<Point>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutocompleteQuery {
    pub text: String,
    pub country_code: Option<String>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReverseQuery {
    pub point: Point,
    pub radius_meters: u32,
    pub limit: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub document: GeoDocument,
    pub score: f32,
    pub distance_meters: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SchemaDefinition {
    pub country_code: String,
    pub languages: Vec<String>,
    pub layers: Vec<String>,
}

pub trait SearchBackend {
    fn name(&self) -> &'static str;
    fn create_schema(&self, schema: &SchemaDefinition) -> Result<(), BackendError>;
    fn bulk_index(&self, docs: &[GeoDocument]) -> Result<(), BackendError>;
    fn forward(&self, query: &ForwardQuery) -> Result<Vec<SearchHit>, BackendError>;
    fn autocomplete(&self, query: &AutocompleteQuery) -> Result<Vec<SearchHit>, BackendError>;
    fn reverse(&self, query: &ReverseQuery) -> Result<Vec<SearchHit>, BackendError>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct CountryConfig {
    pub country_code: String,
    pub country_name: String,
    pub languages: Vec<String>,
    pub timezone: String,
    pub bounds: Bounds,
    pub search: SearchConfig,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

impl Bounds {
    pub fn contains(&self, point: Point) -> bool {
        point.longitude >= self.min_lon
            && point.longitude <= self.max_lon
            && point.latitude >= self.min_lat
            && point.latitude <= self.max_lat
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchConfig {
    pub default_limit: usize,
    pub autocomplete_limit: usize,
    pub reverse_radius_meters: u32,
    pub preferred_layers: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendError {
    message: String,
}

impl BackendError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BackendError {}

pub fn haversine_meters(a: Point, b: Point) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_371_000.0;
    let d_lat = (b.latitude - a.latitude).to_radians();
    let d_lon = (b.longitude - a.longitude).to_radians();
    let lat1 = a.latitude.to_radians();
    let lat2 = b.latitude.to_radians();
    let sin_lat = (d_lat / 2.0).sin();
    let sin_lon = (d_lon / 2.0).sin();
    let h = sin_lat * sin_lat + lat1.cos() * lat2.cos() * sin_lon * sin_lon;
    2.0 * EARTH_RADIUS_M * h.sqrt().asin()
}

pub fn bounding_box_for_radius(point: Point, radius_meters: u32) -> Bounds {
    let radius_km = f64::from(radius_meters) / 1000.0;
    let lat_delta = radius_km / 110.574;
    let lon_delta = radius_km / (111.320 * point.latitude.to_radians().cos().abs().max(0.01));
    Bounds {
        min_lon: point.longitude - lon_delta,
        min_lat: point.latitude - lat_delta,
        max_lon: point.longitude + lon_delta,
        max_lat: point.latitude + lat_delta,
    }
}

/// Tokenize text into normalized ASCII-ish terms. Spanish accents are folded so `Bogota` and
/// `Bogotá` match without keeping duplicate index entries.
pub fn normalized_terms(text: &str) -> Vec<String> {
    let mut normalized = String::with_capacity(text.len());
    for character in text.chars() {
        let folded = match character {
            'á' | 'à' | 'ä' | 'â' | 'Á' | 'À' | 'Ä' | 'Â' => 'a',
            'é' | 'è' | 'ë' | 'ê' | 'É' | 'È' | 'Ë' | 'Ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' | 'Í' | 'Ì' | 'Ï' | 'Î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' | 'Ó' | 'Ò' | 'Ö' | 'Ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' | 'Ú' | 'Ù' | 'Ü' | 'Û' => 'u',
            'ñ' | 'Ñ' => 'n',
            other => other.to_ascii_lowercase(),
        };
        if folded.is_ascii_alphanumeric() {
            normalized.push(folded);
        } else {
            normalized.push(' ');
        }
    }
    normalized
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub fn bounded_limit(limit: usize) -> usize {
    limit.clamp(1, 100)
}

fn push_non_empty<'a>(parts: &mut Vec<&'a str>, value: &'a str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        parts.push(trimmed);
    }
}

fn push_optional<'a>(parts: &mut Vec<&'a str>, value: Option<&'a str>) {
    if let Some(value) = value {
        push_non_empty(parts, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_accents_for_spanish_search() {
        assert_eq!(
            normalized_terms("Bogotá Medellín"),
            vec!["bogota", "medellin"]
        );
    }

    #[test]
    fn haversine_distance_is_reasonable_for_bogota_medellin() {
        let km = haversine_meters(
            Point {
                latitude: 4.711,
                longitude: -74.0721,
            },
            Point {
                latitude: 6.2442,
                longitude: -75.5812,
            },
        ) / 1000.0;
        assert!((km - 245.0).abs() < 15.0, "distance was {km}");
    }
}
