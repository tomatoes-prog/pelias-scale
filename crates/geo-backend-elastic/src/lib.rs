//! Elasticsearch adapter placeholder for parity benchmarks.

use geo_core::{
    AutocompleteQuery, BackendError, ForwardQuery, GeoDocument, ReverseQuery, SchemaDefinition,
    SearchBackend, SearchHit,
};

/// Prototype adapter that records the target endpoint but does not issue
/// network requests yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElasticsearchBackend {
    endpoint: String,
}

impl ElasticsearchBackend {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl SearchBackend for ElasticsearchBackend {
    fn name(&self) -> &'static str {
        "elasticsearch"
    }

    fn create_schema(&self, _schema: &SchemaDefinition) -> Result<(), BackendError> {
        Err(BackendError::new(
            "Elasticsearch schema creation is not implemented yet",
        ))
    }

    fn bulk_index(&self, _docs: &[GeoDocument]) -> Result<(), BackendError> {
        Err(BackendError::new(
            "Elasticsearch bulk indexing is not implemented yet",
        ))
    }

    fn forward(&self, _query: &ForwardQuery) -> Result<Vec<SearchHit>, BackendError> {
        Err(BackendError::new(
            "Elasticsearch forward search is not implemented yet",
        ))
    }

    fn autocomplete(&self, _query: &AutocompleteQuery) -> Result<Vec<SearchHit>, BackendError> {
        Err(BackendError::new(
            "Elasticsearch autocomplete is not implemented yet",
        ))
    }

    fn reverse(&self, _query: &ReverseQuery) -> Result<Vec<SearchHit>, BackendError> {
        Err(BackendError::new(
            "Elasticsearch reverse search is not implemented yet",
        ))
    }
}
