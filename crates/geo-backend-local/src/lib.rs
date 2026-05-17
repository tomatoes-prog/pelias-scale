//! Dependency-free local backend for the Rust geosearch refactor.
//!
//! The backend stores documents once in `docs.tsv` and keeps compact offset indexes in
//! `terms.tsv` and `cells.tsv`. Query execution reads only candidate documents by byte offset,
//! avoiding a full country dataset in RAM while staying fast enough for single-country use.

use geo_core::{
    AddressParts, AdminHierarchy, AutocompleteQuery, BackendError, ForwardQuery, GeoDocument,
    Point, ReverseQuery, SchemaDefinition, SearchBackend, SearchHit, bounded_limit,
    bounding_box_for_radius, haversine_meters, normalized_terms,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const DOCS_FILE: &str = "docs.tsv";
const TERMS_FILE: &str = "terms.tsv";
const CELLS_FILE: &str = "cells.tsv";
const SCHEMA_FILE: &str = "schema.txt";
const CELL_SIZE_DEGREES: f64 = 0.05;

#[derive(Clone, Debug)]
pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BackendError> {
        let root = path.as_ref().to_path_buf();
        Ok(Self { root })
    }

    pub fn count_documents(&self) -> Result<u64, BackendError> {
        let path = self.root.join(DOCS_FILE);
        if !path.exists() {
            return Ok(0);
        }
        let file = File::open(path).map_err(to_backend_error)?;
        Ok(BufReader::new(file).lines().count() as u64)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl SearchBackend for LocalBackend {
    fn name(&self) -> &'static str {
        "local-offset-index"
    }

    fn create_schema(&self, schema: &SchemaDefinition) -> Result<(), BackendError> {
        fs::create_dir_all(&self.root).map_err(to_backend_error)?;
        fs::write(
            self.root.join(SCHEMA_FILE),
            format!(
                "country_code={}\nlanguages={}\nlayers={}\n",
                schema.country_code,
                schema.languages.join(","),
                schema.layers.join(",")
            ),
        )
        .map_err(to_backend_error)
    }

    fn bulk_index(&self, docs: &[GeoDocument]) -> Result<(), BackendError> {
        fs::create_dir_all(&self.root).map_err(to_backend_error)?;
        let mut existing = load_all_documents(&self.root)?;
        for doc in docs {
            existing.insert(doc.id.clone(), doc.clone());
        }
        write_index(&self.root, existing.values())
    }

    fn forward(&self, query: &ForwardQuery) -> Result<Vec<SearchHit>, BackendError> {
        let terms = normalized_terms(&query.text);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let term_index = read_offset_index(&self.root.join(TERMS_FILE))?;
        let offsets = intersect_offsets(&terms, &term_index);
        let mut hits = score_offsets(
            &self.root,
            offsets,
            query.country_code.as_deref(),
            query.focus,
        )?;
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(bounded_limit(query.limit));
        Ok(hits)
    }

    fn autocomplete(&self, query: &AutocompleteQuery) -> Result<Vec<SearchHit>, BackendError> {
        let terms = normalized_terms(&query.text);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let term_index = read_offset_index(&self.root.join(TERMS_FILE))?;
        let mut candidate_sets = Vec::with_capacity(terms.len());
        for (index, term) in terms.iter().enumerate() {
            if index + 1 == terms.len() {
                let mut offsets = BTreeSet::new();
                for (indexed_term, indexed_offsets) in &term_index {
                    if indexed_term.starts_with(term) {
                        offsets.extend(indexed_offsets.iter().copied());
                    }
                }
                candidate_sets.push(offsets);
            } else {
                candidate_sets.push(term_index.get(term).cloned().unwrap_or_else(BTreeSet::new));
            }
        }
        let offsets = intersect_sets(candidate_sets);
        let mut hits = score_offsets(&self.root, offsets, query.country_code.as_deref(), None)?;
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(bounded_limit(query.limit));
        Ok(hits)
    }

    fn reverse(&self, query: &ReverseQuery) -> Result<Vec<SearchHit>, BackendError> {
        let cell_index = read_offset_index(&self.root.join(CELLS_FILE))?;
        let bbox = bounding_box_for_radius(query.point, query.radius_meters);
        let mut offsets = BTreeSet::new();
        for cell in cells_for_bounds(bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon) {
            if let Some(cell_offsets) = cell_index.get(&cell) {
                offsets.extend(cell_offsets.iter().copied());
            }
        }
        let mut hits = Vec::new();
        let mut reader = open_docs_reader(&self.root)?;
        for offset in offsets {
            let doc = read_doc_at(&mut reader, offset)?;
            let distance = haversine_meters(
                query.point,
                Point {
                    latitude: doc.latitude,
                    longitude: doc.longitude,
                },
            );
            if distance <= f64::from(query.radius_meters) {
                hits.push(SearchHit {
                    score: doc.popularity - (distance as f32 / 10_000.0),
                    document: doc,
                    distance_meters: Some(distance),
                });
            }
        }
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
        hits.truncate(bounded_limit(query.limit));
        Ok(hits)
    }
}

fn write_index<'a>(
    root: &Path,
    docs: impl Iterator<Item = &'a GeoDocument>,
) -> Result<(), BackendError> {
    let mut docs_file = File::create(root.join(DOCS_FILE)).map_err(to_backend_error)?;
    let mut term_offsets: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    let mut cell_offsets: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    let mut offset = 0_u64;

    for doc in docs {
        let line = encode_document(doc);
        let bytes = line.as_bytes();
        docs_file.write_all(bytes).map_err(to_backend_error)?;

        let unique_terms: HashSet<String> = normalized_terms(&doc.searchable_text())
            .into_iter()
            .collect();
        for term in unique_terms {
            term_offsets.entry(term).or_default().insert(offset);
        }
        cell_offsets
            .entry(cell_for_point(doc.latitude, doc.longitude))
            .or_default()
            .insert(offset);
        offset += bytes.len() as u64;
    }
    write_offset_index(&root.join(TERMS_FILE), &term_offsets)?;
    write_offset_index(&root.join(CELLS_FILE), &cell_offsets)
}

fn load_all_documents(root: &Path) -> Result<BTreeMap<String, GeoDocument>, BackendError> {
    let path = root.join(DOCS_FILE);
    let mut docs = BTreeMap::new();
    if !path.exists() {
        return Ok(docs);
    }
    let file = File::open(path).map_err(to_backend_error)?;
    for line in BufReader::new(file).lines() {
        let doc = decode_document(&line.map_err(to_backend_error)?)?;
        docs.insert(doc.id.clone(), doc);
    }
    Ok(docs)
}

fn score_offsets(
    root: &Path,
    offsets: BTreeSet<u64>,
    country_code: Option<&str>,
    focus: Option<Point>,
) -> Result<Vec<SearchHit>, BackendError> {
    let mut reader = open_docs_reader(root)?;
    let mut hits = Vec::with_capacity(offsets.len().min(256));
    for offset in offsets {
        let doc = read_doc_at(&mut reader, offset)?;
        if country_code.is_some_and(|code| doc.country_code != code) {
            continue;
        }
        let distance_meters = focus.map(|point| {
            haversine_meters(
                point,
                Point {
                    latitude: doc.latitude,
                    longitude: doc.longitude,
                },
            )
        });
        let distance_penalty = distance_meters.unwrap_or_default() as f32 / 100_000.0;
        hits.push(SearchHit {
            score: doc.popularity - distance_penalty,
            document: doc,
            distance_meters,
        });
    }
    Ok(hits)
}

fn open_docs_reader(root: &Path) -> Result<BufReader<File>, BackendError> {
    File::open(root.join(DOCS_FILE))
        .map(BufReader::new)
        .map_err(to_backend_error)
}

fn read_doc_at(reader: &mut BufReader<File>, offset: u64) -> Result<GeoDocument, BackendError> {
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(to_backend_error)?;
    let mut line = String::new();
    reader.read_line(&mut line).map_err(to_backend_error)?;
    decode_document(line.trim_end_matches('\n'))
}

fn intersect_offsets(terms: &[String], index: &BTreeMap<String, BTreeSet<u64>>) -> BTreeSet<u64> {
    let sets = terms
        .iter()
        .map(|term| index.get(term).cloned().unwrap_or_else(BTreeSet::new))
        .collect();
    intersect_sets(sets)
}

fn intersect_sets(mut sets: Vec<BTreeSet<u64>>) -> BTreeSet<u64> {
    if sets.is_empty() {
        return BTreeSet::new();
    }
    sets.sort_by_key(BTreeSet::len);
    let mut result = sets.remove(0);
    for set in sets {
        result = result.intersection(&set).copied().collect();
        if result.is_empty() {
            break;
        }
    }
    result
}

fn write_offset_index(
    path: &Path,
    index: &BTreeMap<String, BTreeSet<u64>>,
) -> Result<(), BackendError> {
    let mut file = File::create(path).map_err(to_backend_error)?;
    for (key, offsets) in index {
        let joined = offsets
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        writeln!(file, "{}\t{}", escape(key), joined).map_err(to_backend_error)?;
    }
    Ok(())
}

fn read_offset_index(path: &Path) -> Result<BTreeMap<String, BTreeSet<u64>>, BackendError> {
    let mut index = BTreeMap::new();
    if !path.exists() {
        return Ok(index);
    }
    let file = File::open(path).map_err(to_backend_error)?;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(to_backend_error)?;
        let Some((key, values)) = line.split_once('\t') else {
            continue;
        };
        let offsets = values
            .split(',')
            .filter(|value| !value.is_empty())
            .filter_map(|value| value.parse::<u64>().ok())
            .collect::<BTreeSet<_>>();
        index.insert(unescape(key), offsets);
    }
    Ok(index)
}

fn encode_document(doc: &GeoDocument) -> String {
    [
        escape(&doc.id),
        escape(&doc.source),
        escape(&doc.layer),
        escape(&doc.name),
        escape(&doc.label),
        escape(&doc.country_code),
        doc.latitude.to_string(),
        doc.longitude.to_string(),
        doc.popularity.to_string(),
        escape_opt(&doc.admin_hierarchy.country),
        escape_opt(&doc.admin_hierarchy.region),
        escape_opt(&doc.admin_hierarchy.county),
        escape_opt(&doc.admin_hierarchy.locality),
        escape_opt(&doc.admin_hierarchy.neighbourhood),
        escape_opt(&doc.address.house_number),
        escape_opt(&doc.address.street),
        escape_opt(&doc.address.postal_code),
        escape(&doc.aliases.join("|")),
    ]
    .join("\t")
        + "\n"
}

fn decode_document(line: &str) -> Result<GeoDocument, BackendError> {
    let fields = line.split('\t').map(unescape).collect::<Vec<_>>();
    if fields.len() != 18 {
        return Err(BackendError::new(format!(
            "invalid document row: expected 18 fields, got {}",
            fields.len()
        )));
    }
    Ok(GeoDocument {
        id: fields[0].clone(),
        source: fields[1].clone(),
        layer: fields[2].clone(),
        name: fields[3].clone(),
        label: fields[4].clone(),
        country_code: fields[5].clone(),
        latitude: fields[6].parse().map_err(to_backend_error)?,
        longitude: fields[7].parse().map_err(to_backend_error)?,
        popularity: fields[8].parse().map_err(to_backend_error)?,
        admin_hierarchy: AdminHierarchy {
            country: none_if_empty(fields[9].clone()),
            region: none_if_empty(fields[10].clone()),
            county: none_if_empty(fields[11].clone()),
            locality: none_if_empty(fields[12].clone()),
            neighbourhood: none_if_empty(fields[13].clone()),
        },
        address: AddressParts {
            house_number: none_if_empty(fields[14].clone()),
            street: none_if_empty(fields[15].clone()),
            postal_code: none_if_empty(fields[16].clone()),
        },
        aliases: fields[17]
            .split('|')
            .filter(|alias| !alias.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    })
}

fn escape_opt(value: &Option<String>) -> String {
    value
        .as_ref()
        .map_or_else(String::new, |value| escape(value))
}

fn none_if_empty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn unescape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            match chars.next() {
                Some('t') => output.push('\t'),
                Some('n') => output.push('\n'),
                Some('\\') => output.push('\\'),
                Some(other) => {
                    output.push('\\');
                    output.push(other);
                }
                None => output.push('\\'),
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn cell_for_point(lat: f64, lon: f64) -> String {
    let lat_cell = (lat / CELL_SIZE_DEGREES).floor() as i32;
    let lon_cell = (lon / CELL_SIZE_DEGREES).floor() as i32;
    format!("{lat_cell}:{lon_cell}")
}

fn cells_for_bounds(min_lat: f64, min_lon: f64, max_lat: f64, max_lon: f64) -> Vec<String> {
    let min_lat_cell = (min_lat / CELL_SIZE_DEGREES).floor() as i32;
    let max_lat_cell = (max_lat / CELL_SIZE_DEGREES).floor() as i32;
    let min_lon_cell = (min_lon / CELL_SIZE_DEGREES).floor() as i32;
    let max_lon_cell = (max_lon / CELL_SIZE_DEGREES).floor() as i32;
    let mut cells = Vec::new();
    for lat in min_lat_cell..=max_lat_cell {
        for lon in min_lon_cell..=max_lon_cell {
            cells.push(format!("{lat}:{lon}"));
        }
    }
    cells
}

fn to_backend_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_index() -> PathBuf {
        std::env::temp_dir().join(format!(
            "geo-backend-local-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn sample_docs() -> Vec<GeoDocument> {
        vec![
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
            },
            GeoDocument {
                id: "oa:address:calle-72".to_string(),
                source: "openaddresses".to_string(),
                layer: "address".to_string(),
                name: "Calle 72".to_string(),
                label: "Calle 72, Bogotá, Colombia".to_string(),
                country_code: "CO".to_string(),
                latitude: 4.658,
                longitude: -74.059,
                admin_hierarchy: AdminHierarchy {
                    country: Some("Colombia".to_string()),
                    locality: Some("Bogotá".to_string()),
                    ..AdminHierarchy::default()
                },
                address: AddressParts {
                    street: Some("Calle 72".to_string()),
                    ..AddressParts::default()
                },
                aliases: vec![],
                popularity: 5.0,
            },
        ]
    }

    #[test]
    fn indexes_and_searches_forward_autocomplete_and_reverse() {
        let path = temp_index();
        let backend = LocalBackend::open(&path).unwrap();
        backend
            .create_schema(&SchemaDefinition {
                country_code: "CO".to_string(),
                languages: vec!["es".to_string()],
                layers: vec![],
            })
            .unwrap();
        backend.bulk_index(&sample_docs()).unwrap();

        let forward_hits = backend
            .forward(&ForwardQuery {
                text: "Bogota".to_string(),
                country_code: Some("CO".to_string()),
                focus: None,
                limit: 5,
            })
            .unwrap();
        assert_eq!(forward_hits[0].document.name, "Bogotá");

        let autocomplete_hits = backend
            .autocomplete(&AutocompleteQuery {
                text: "Call".to_string(),
                country_code: Some("CO".to_string()),
                limit: 5,
            })
            .unwrap();
        assert_eq!(autocomplete_hits[0].document.layer, "address");

        let reverse_hits = backend
            .reverse(&ReverseQuery {
                point: Point {
                    latitude: 4.6581,
                    longitude: -74.0591,
                },
                radius_meters: 250,
                limit: 5,
            })
            .unwrap();
        assert_eq!(reverse_hits[0].document.id, "oa:address:calle-72");
        fs::remove_dir_all(path).ok();
    }
}
