# Rust geosearch operations guide

## What is implemented

This branch now contains a runnable Rust path for country-scale geosearch:

1. `geo-ingest` reads a country TOML file and a normalized CSV extract.
2. It filters records outside the configured country bounds.
3. It writes a dependency-free local offset index directory with:
   - `docs.tsv`: one escaped document row per feature;
   - `terms.tsv`: normalized token to document byte-offset postings;
   - `cells.tsv`: geographic grid cell to document byte-offset postings;
   - `schema.txt`: country/language/layer metadata.
4. `geo-api` serves `/healthz`, `/v1/search`, `/v1/autocomplete`, and `/v1/reverse` from that index.

The local backend is intentionally not the final word on search relevance. It is the working,
low-resource baseline that makes the refactor executable before deciding whether Elasticsearch,
Manticore, or the embedded local offset index is the best production backend for a country.

## Why the storage is low-resource

The local backend does not load every full document into memory for each query. It loads compact
posting lists from `terms.tsv` or `cells.tsv`, intersects candidate byte offsets, and then reads
only candidate document rows from `docs.tsv`. That design keeps RAM tied to the search indexes and
candidate set instead of the full country document corpus.

For larger countries, the next optimization should be to shard `terms.tsv` by token prefix and
`cells.tsv` by region so each API worker maps or reads only the shard it needs. The backend trait
already allows replacing this local storage with Manticore or Elasticsearch without changing the
normalization model.

## Local runbook

Build and test everything:

```bash
cargo fmt --all --check
cargo test --workspace
```

Create the sample Colombia index:

```bash
cargo run -p geo-ingest -- prepare \
  --config countries/colombia.toml \
  --input data/sample/colombia.csv \
  --output work/colombia-index
```

Inspect index stats:

```bash
cargo run -p geo-ingest -- stats --index work/colombia-index
```

Run the API:

```bash
cargo run -p geo-api -- \
  --config countries/colombia.toml \
  --index work/colombia-index \
  --bind 127.0.0.1:8080
```

Example queries:

```bash
curl 'http://127.0.0.1:8080/v1/search?query=Bogota'
curl 'http://127.0.0.1:8080/v1/autocomplete?text=Call'
curl 'http://127.0.0.1:8080/v1/reverse?lat=4.6581&lon=-74.0591&radius_meters=250'
```

## Country generalization checklist

To add another country:

1. Copy `countries/colombia.toml` to `countries/<country>.toml`.
2. Change `country_code`, `country_name`, `languages`, `timezone`, and `[bounds]`.
3. Provide a normalized CSV with at least `id,name,lat,lon`; optional columns include `source`,
   `layer`, `label`, `country`, `region`, `county`, `locality`, `neighbourhood`, `street`,
   `house_number`, `postal_code`, `aliases`, and `popularity`.
4. Run `geo-ingest prepare` with the new config and CSV.
5. Run the benchmark corpus for that country before considering a backend switch.

## DevOps expectations

- CI runs formatting, tests, sample ingestion, and index stats through
  `.github/workflows/rust-geosearch.yml`.
- `Dockerfile.geosearch` builds only Rust binaries and runs them as an unprivileged user.
- `docker-compose.geosearch.yml` separates index creation from the API container and persists the
  index in a named volume.
- Runtime configuration is environment-variable friendly: `GEO_CONFIG`, `GEO_INDEX`, and
  `GEO_BIND`.
