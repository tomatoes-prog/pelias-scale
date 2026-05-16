use geo_backend_local::LocalBackend;
use geo_core::{
    AddressParts, AdminHierarchy, Bounds, CountryConfig, GeoDocument, Point, SchemaDefinition,
    SearchBackend, SearchConfig,
};
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("geo-ingest: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    match args.get(1).map(String::as_str) {
        Some("prepare") => prepare(&parse_flags(&args[2..])?),
        Some("stats") => stats(&parse_flags(&args[2..])?),
        _ => {
            print_usage();
            Ok(())
        }
    }
}

fn prepare(flags: &HashMap<String, String>) -> Result<(), String> {
    let config_path = required(flags, "config")?;
    let input_path = required(flags, "input")?;
    let output_path = required(flags, "output")?;
    let batch_size = flags
        .get("batch-size")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(5_000)
        .max(1);
    let config = read_country_config(Path::new(config_path))?;
    let backend = LocalBackend::open(output_path).map_err(|error| error.to_string())?;
    backend
        .create_schema(&SchemaDefinition {
            country_code: config.country_code.clone(),
            languages: config.languages.clone(),
            layers: config.search.preferred_layers.clone(),
        })
        .map_err(|error| error.to_string())?;

    let mut imported = 0_u64;
    let mut skipped = 0_u64;
    let mut batch = Vec::with_capacity(batch_size);
    for document in read_csv_documents(Path::new(input_path), &config)? {
        let document = document?;
        if !config.bounds.contains(Point {
            latitude: document.latitude,
            longitude: document.longitude,
        }) {
            skipped += 1;
            continue;
        }
        batch.push(document);
        if batch.len() >= batch_size {
            backend
                .bulk_index(&batch)
                .map_err(|error| error.to_string())?;
            imported += batch.len() as u64;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        backend
            .bulk_index(&batch)
            .map_err(|error| error.to_string())?;
        imported += batch.len() as u64;
    }
    println!("prepared {imported} documents into {output_path} (skipped {skipped} outside bounds)");
    Ok(())
}

fn stats(flags: &HashMap<String, String>) -> Result<(), String> {
    let index_path = required(flags, "index")?;
    let backend = LocalBackend::open(index_path).map_err(|error| error.to_string())?;
    println!(
        "documents={}",
        backend
            .count_documents()
            .map_err(|error| error.to_string())?
    );
    Ok(())
}

fn read_csv_documents(
    path: &Path,
    config: &CountryConfig,
) -> Result<Vec<Result<GeoDocument, String>>, String> {
    let file = File::open(path).map_err(|error| format!("opening {}: {error}", path.display()))?;
    let mut lines = BufReader::new(file).lines();
    let header = lines
        .next()
        .ok_or_else(|| format!("{} is empty", path.display()))?
        .map_err(|error| error.to_string())?;
    let headers = parse_csv_line(&header);
    let mut docs = Vec::new();
    for (line_number, line) in lines.enumerate() {
        let line = line.map_err(|error| error.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let values = parse_csv_line(&line);
        let row = headers
            .iter()
            .zip(values.iter())
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<HashMap<_, _>>();
        docs.push(
            csv_row_to_document(row, config)
                .map_err(|error| format!("{} line {}: {error}", path.display(), line_number + 2)),
        );
    }
    Ok(docs)
}

fn csv_row_to_document(
    row: HashMap<&str, &str>,
    config: &CountryConfig,
) -> Result<GeoDocument, String> {
    let id = required_row(&row, "id")?.to_string();
    let name = required_row(&row, "name")?.to_string();
    let lat = required_row(&row, "lat")?
        .parse::<f64>()
        .map_err(|error| error.to_string())?;
    let lon = required_row(&row, "lon")?
        .parse::<f64>()
        .map_err(|error| error.to_string())?;
    let mut document = GeoDocument {
        id,
        source: optional_row(&row, "source").unwrap_or("csv").to_string(),
        layer: optional_row(&row, "layer").unwrap_or("venue").to_string(),
        name,
        label: optional_row(&row, "label").unwrap_or_default().to_string(),
        country_code: config.country_code.clone(),
        latitude: lat,
        longitude: lon,
        admin_hierarchy: AdminHierarchy {
            country: Some(
                optional_row(&row, "country")
                    .unwrap_or(&config.country_name)
                    .to_string(),
            ),
            region: optional_row(&row, "region").map(ToOwned::to_owned),
            county: optional_row(&row, "county").map(ToOwned::to_owned),
            locality: optional_row(&row, "locality").map(ToOwned::to_owned),
            neighbourhood: optional_row(&row, "neighbourhood").map(ToOwned::to_owned),
        },
        address: AddressParts {
            house_number: optional_row(&row, "house_number").map(ToOwned::to_owned),
            street: optional_row(&row, "street").map(ToOwned::to_owned),
            postal_code: optional_row(&row, "postal_code").map(ToOwned::to_owned),
        },
        aliases: optional_row(&row, "aliases")
            .unwrap_or_default()
            .split('|')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        popularity: optional_row(&row, "popularity")
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or_default(),
    };
    document.ensure_label();
    Ok(document)
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

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                values.push(current.trim().to_string());
                current.clear();
            }
            other => current.push(other),
        }
    }
    values.push(current.trim().to_string());
    values
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

fn required<'a>(flags: &'a HashMap<String, String>, key: &str) -> Result<&'a str, String> {
    flags
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("missing --{key}"))
}

fn required_row<'a>(row: &'a HashMap<&str, &str>, key: &str) -> Result<&'a str, String> {
    optional_row(row, key).ok_or_else(|| format!("missing required CSV column {key}"))
}

fn optional_row<'a>(row: &'a HashMap<&str, &str>, key: &str) -> Option<&'a str> {
    row.get(key)
        .copied()
        .filter(|value| !value.trim().is_empty())
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

fn print_usage() {
    eprintln!(
        "usage:\n  geo-ingest prepare --config countries/colombia.toml --input data.csv --output index-dir [--batch-size 5000]\n  geo-ingest stats --index index-dir"
    );
}
