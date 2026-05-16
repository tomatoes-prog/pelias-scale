# Rust + Manticore geosearch modernization plan

## Purpose

This document captures a pragmatic modernization path for turning the current
Pelias-oriented documentation/codebase into a low-resource, single-country
geosearch platform. The target use case is not planet-scale search: it is a
country-sized deployment, for example Colombia, that imports open geographic
data, answers fast forward/reverse/autocomplete queries, and scales predictably
without requiring an Elasticsearch-sized operational footprint.

## What exists today in this repository

This repository is a top-level Pelias project/documentation repository rather
than the full implementation of the API, importers, schema, and datastore code.
The README describes Pelias as a modular open-source geocoder built on
Elasticsearch, with the heavy implementation split across separate repositories
under the Pelias organization.

The current architecture documented here has three important properties:

1. **Modular importers** normalize open datasets before ingestion. Supported
   import sources include OpenStreetMap, OpenAddresses, Who's On First,
   Geonames, polylines, and CSV.
2. **Elasticsearch is the primary datastore and query engine**. The documented
   `pelias-schema` component prepares Elasticsearch indices, and the API relies
   on Elasticsearch full-text and geospatial capabilities for ranking and
   filtering.
3. **The service layer is Node.js-first**. Pelias API/importer packages are
   Node.js packages, while selected CPU-heavy utilities have historically been
   implemented in lower-level languages when Node.js could not meet performance
   requirements.

Because the runnable implementation lives in the component repositories, the
first engineering milestone should not be an immediate rewrite. It should be a
compatibility-oriented prototype that proves the data model, ranking behavior,
and operational profile against one country extract.

## Modernization goals

The modernization branch should optimize for these outcomes:

- **Single-country simplicity**: first-class support for one bounded country
  extract instead of global deployment assumptions.
- **Low memory and CPU use**: reduce resident memory, JVM dependence, and
  ingestion overhead where Rust or a lighter search backend provides measurable
  wins.
- **Fast geosearch**: keep autocomplete, forward geocoding, reverse geocoding,
  administrative hierarchy, deduplication, and language-aware labels.
- **Operational clarity**: make the stack easy to run with Docker Compose first,
  then Kubernetes only when scale requires it.
- **Migration safety**: keep an Elasticsearch-compatible path until Manticore is
  proven with real queries, indexes, and data volumes.

## Recommended target architecture

```text
                 ┌──────────────────────────────┐
                 │ Country configuration         │
                 │ ISO code, bounds, languages,  │
                 │ data URLs, ranking knobs      │
                 └──────────────┬───────────────┘
                                │
┌──────────────┐     ┌──────────▼──────────┐     ┌────────────────┐
│ OSM / OA /   │     │ Rust ingest engine   │     │ Search backend │
│ WOF / CSV    ├────►│ normalize, dedupe,   ├────►│ Elasticsearch  │
│ extracts     │     │ hierarchy, batches   │     │ or Manticore   │
└──────────────┘     └──────────┬──────────┘     └───────┬────────┘
                                │                        │
                         ┌──────▼──────┐          ┌──────▼──────┐
                         │ SQLite/Parq. │          │ Rust API    │
                         │ checkpoints  │          │ geocode/rev │
                         └─────────────┘          └─────────────┘
```

### Rust processing engine

Rust should own the CPU- and memory-sensitive parts of the platform:

- streaming PBF/CSV/GeoJSON readers;
- country-boundary clipping and validation;
- canonical document construction;
- address/admin hierarchy enrichment;
- deterministic ID generation;
- duplicate detection and confidence features;
- bulk indexing adapters for each search backend;
- reverse-geocoding precomputation such as H3/S2/geohash buckets when useful.

A Rust engine can be introduced without replacing everything at once by exposing
stable command-line boundaries first:

```bash
geo-ingest prepare --country CO --config countries/colombia.toml
geo-ingest index --backend elasticsearch --endpoint http://localhost:9200
geo-ingest index --backend manticore --endpoint http://localhost:9308
geo-api serve --backend manticore --config countries/colombia.toml
```

### Backend abstraction

Create a small search-backend interface before committing to Manticore:

```rust
trait SearchBackend {
    async fn create_schema(&self, schema: SchemaDefinition) -> Result<()>;
    async fn bulk_index(&self, docs: impl Stream<Item = GeoDocument>) -> Result<()>;
    async fn forward(&self, query: ForwardQuery) -> Result<Vec<SearchHit>>;
    async fn autocomplete(&self, query: AutocompleteQuery) -> Result<Vec<SearchHit>>;
    async fn reverse(&self, query: ReverseQuery) -> Result<Vec<SearchHit>>;
}
```

This isolates the expensive Pelias-specific work from the datastore decision.
Elasticsearch and Manticore can then be benchmarked with the same normalized
country documents and query corpus.

## Manticore Search feasibility

Manticore is worth prototyping because it is implemented in C++, supports SQL
and JSON-over-HTTP APIs, provides real-time tables, supports Elasticsearch-like
write requests for some ingestion workflows, and offers columnar storage and
secondary indexes that can reduce memory pressure for filter-heavy workloads.
Those traits are attractive for a country-sized geocoder.

However, it should not be treated as a drop-in Pelias replacement yet. Pelias
uses Elasticsearch query DSL, analyzers, token filters, scoring behavior,
geospatial filters, and operational assumptions. A Manticore migration is only
safe if these behaviors are explicitly tested:

- language analyzers and tokenization for Spanish, abbreviations, accents, and
  alternate names;
- prefix/infix autocomplete behavior;
- phrase, fallback, and fuzzy matching semantics;
- geo-distance filtering and sorting;
- admin hierarchy filters such as country, region, locality, and neighborhood;
- batch ingest throughput and segment/compaction behavior;
- relevance parity against representative Colombian queries.

### Decision rule

Use Manticore only if the benchmark shows it is better for the target country
profile. A good first threshold is:

- at least **30% lower steady-state RAM** than Elasticsearch/OpenSearch;
- equal or better **p95 latency** for forward, reverse, and autocomplete;
- no critical loss in top-1/top-5 relevance for the query corpus;
- simpler operations for the expected deployment size;
- acceptable feature coverage for Spanish and local address formats.

If Manticore does not pass this gate, keep Elasticsearch/OpenSearch as the
backend while still moving ingestion, normalization, and API hot paths to Rust.
That still reduces resource use without sacrificing proven search behavior.

## Colombia-first implementation plan

### Phase 0: repository inventory

- Map the Pelias component repositories that must be touched: API, schema,
  OpenStreetMap importer, OpenAddresses importer, Who's On First importer,
  interpolation, and Docker orchestration.
- Capture current Elasticsearch mappings, analyzers, and query templates.
- Build a representative Colombian query corpus covering Bogotá, Medellín,
  Cali, Barranquilla, addresses, POIs, neighborhoods, municipalities,
  departments, aliases, abbreviations, and accent/no-accent variants.

### Phase 1: Rust document pipeline

- Define `GeoDocument` as the canonical internal schema.
- Stream one Colombia OSM PBF extract and selected OpenAddresses/Who's On First
  data into normalized documents.
- Persist checkpoints in SQLite or Parquet so failed imports resume cheaply.
- Emit bulk requests for the existing Elasticsearch schema first, proving that
  the Rust pipeline can feed the current stack.

### Phase 2: benchmark harness

- Add Docker Compose profiles for Elasticsearch/OpenSearch and Manticore.
- Load identical Colombia documents into both backends.
- Run the same query corpus against both backends.
- Record ingest time, index size, RAM, CPU, p50/p95/p99 latency, and relevance
  judgments.

### Phase 3: Manticore prototype

- Design a Manticore schema for names, alternate names, address parts,
  hierarchy IDs, popularity, source/layer, centroid, bounding box metadata, and
  optional H3/S2 cells.
- Implement SQL and JSON search templates for forward, autocomplete, and
  reverse endpoints.
- Tune Spanish tokenization, morphology, min-prefix/min-infix settings, and
  ranking expressions.
- Compare against Elasticsearch with the benchmark gate above.

### Phase 4: Rust API

- Implement read-only HTTP endpoints compatible with the Pelias response shape
  where practical.
- Keep the API stateless; all state should live in the search backend or compact
  local lookup tables.
- Add response caching for hot autocomplete and reverse-geocode cells.
- Provide a minimal country configuration file that can be copied for another
  country later.

### Phase 5: production hardening

- Add repeatable data-download manifests with checksums.
- Add zero-downtime reindexing through versioned indexes/tables and aliases.
- Add Prometheus metrics for latency, backend timings, result counts, and cache
  hits.
- Add disaster recovery documentation for rebuilding a country index from raw
  extracts.

## Minimal deliverable for the new branch

The first useful branch should contain:

1. this plan;
2. a country configuration example for Colombia;
3. a Rust workspace skeleton with crates for `geo-core`, `geo-ingest`,
   `geo-backend-elastic`, `geo-backend-manticore`, and `geo-api`;
4. Docker Compose profiles for both backends;
5. a benchmark query corpus and repeatable benchmark command.

That deliverable is intentionally small enough to review but strong enough to
answer the main strategic question: **does Manticore beat Elasticsearch for a
single-country geocoder without hurting quality?**

## Immediate next actions

- Create the Rust workspace skeleton behind feature flags rather than replacing
  existing Pelias documentation.
- Write the Colombia config and query corpus.
- Prototype the backend trait and two no-op adapters so the architecture is
  testable before ingest code exists.
- Run a small benchmark with a Bogotá-only extract, then expand to all Colombia
  once the schema and query templates stabilize.

## Implemented baseline in this branch

The branch now includes a runnable Rust baseline backend named `geo-backend-local`. It is a
low-resource offset index that stores full documents once and uses token/cell posting lists to
read only candidates at query time. This gives the project a working country-scale execution path
while Elasticsearch and Manticore adapters remain behind the same `SearchBackend` trait for later
benchmarks.
