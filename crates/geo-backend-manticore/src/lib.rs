//! Manticore Search adapter placeholder for country-scale geosearch benchmarks.

use geo_core::{
    AutocompleteQuery, BackendError, ForwardQuery, GeoDocument, ReverseQuery, SchemaDefinition,
    SearchBackend, SearchHit,
};

/// Prototype adapter that records the target endpoint but does not issue
/// network requests yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManticoreBackend {
    endpoint: String,
}

impl ManticoreBackend {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl SearchBackend for ManticoreBackend {
    fn name(&self) -> &'static str {
        "manticore"
    }

    fn create_schema(&self, _schema: &SchemaDefinition) -> Result<(), BackendError> {
        Err(BackendError::new(
            "Manticore schema creation is not implemented yet",
        ))
    }

    fn bulk_index(&self, _docs: &[GeoDocument]) -> Result<(), BackendError> {
        Err(BackendError::new(
            "Manticore bulk indexing is not implemented yet",
        ))
    }

    fn forward(&self, _query: &ForwardQuery) -> Result<Vec<SearchHit>, BackendError> {
        Err(BackendError::new(
            "Manticore forward search is not implemented yet",
        ))
    }

    fn autocomplete(&self, _query: &AutocompleteQuery) -> Result<Vec<SearchHit>, BackendError> {
        Err(BackendError::new(
            "Manticore autocomplete is not implemented yet",
        ))
    }

    fn reverse(&self, _query: &ReverseQuery) -> Result<Vec<SearchHit>, BackendError> {
        Err(BackendError::new(
            "Manticore reverse search is not implemented yet",
        ))
    }
}
