//! Implementation of the schema gRPC service

#![deny(rustdoc::broken_intra_doc_links, rust_2018_idioms)]
#![warn(
    clippy::clone_on_ref_ptr,
    clippy::dbg_macro,
    clippy::explicit_iter_loop,
    // See https://github.com/influxdata/influxdb_iox/pull/1671
    clippy::future_not_send,
    clippy::todo,
    clippy::use_self,
    missing_debug_implementations,
    unused_crate_dependencies
)]

// Workaround for "unused crate" lint false positives.
use workspace_hack as _;

use std::{ops::DerefMut, sync::Arc};

use generated_types::influxdata::iox::schema::v1::*;
use iox_catalog::interface::{
    get_schema_by_name, get_schema_by_namespace_and_table, Catalog, SoftDeletedRows,
};
use observability_deps::tracing::warn;
use tonic::{Request, Response, Status};

/// Implementation of the gRPC schema service
#[derive(Debug)]
pub struct SchemaService {
    /// Catalog.
    catalog: Arc<dyn Catalog>,
}

impl SchemaService {
    pub fn new(catalog: Arc<dyn Catalog>) -> Self {
        Self { catalog }
    }
}

#[tonic::async_trait]
impl schema_service_server::SchemaService for SchemaService {
    async fn get_schema(
        &self,
        request: Request<GetSchemaRequest>,
    ) -> Result<Response<GetSchemaResponse>, Status> {
        let mut repos = self.catalog.repositories().await;

        let req = request.into_inner();

        let schema = match req.table {
            Some(table_name) => {
                get_schema_by_namespace_and_table(
                    &req.namespace,
                    &table_name,
                    repos.deref_mut(),
                    SoftDeletedRows::ExcludeDeleted,
                )
                .await
            }
            None => {
                get_schema_by_name(
                    &req.namespace,
                    repos.deref_mut(),
                    SoftDeletedRows::ExcludeDeleted,
                )
                .await
            }
        }
        .map_err(|e| {
            warn!(error=%e, %req.namespace, "failed to retrieve namespace schema");
            Status::not_found(e.to_string())
        })
        .map(Arc::new)?;

        Ok(Response::new(GetSchemaResponse {
            schema: Some(schema_to_proto(&schema)),
        }))
    }
}

fn schema_to_proto(schema: &data_types::NamespaceSchema) -> NamespaceSchema {
    NamespaceSchema {
        id: schema.id.get(),
        tables: schema
            .tables
            .iter()
            .map(|(name, t)| {
                (
                    name.clone(),
                    TableSchema {
                        id: t.id.get(),
                        columns: t
                            .columns
                            .iter()
                            .map(|(name, c)| {
                                (
                                    name.clone(),
                                    ColumnSchema {
                                        id: c.id.get(),
                                        column_type: c.column_type as i32,
                                    },
                                )
                            })
                            .collect(),
                    },
                )
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_types::ColumnType;
    use generated_types::influxdata::iox::schema::v1::schema_service_server::SchemaService;
    use iox_catalog::{
        mem::MemCatalog,
        test_helpers::{arbitrary_namespace, arbitrary_table},
    };
    use std::sync::Arc;
    use tonic::Code;

    #[tokio::test]
    async fn get_schema() {
        let namespace = "namespace_schema_test";
        let table = "schema_test_table";
        let column = "schema_test_column";
        let another_table = "another_schema_test_table";
        let another_column = "another_schema_test_column";

        // create a catalog and populate it with some test data, then drop the write lock
        let catalog = {
            let metrics = Arc::new(metric::Registry::default());
            let catalog = Arc::new(MemCatalog::new(metrics));
            let mut repos = catalog.repositories().await;
            let namespace = arbitrary_namespace(&mut *repos, namespace).await;

            let table = arbitrary_table(&mut *repos, table, &namespace).await;
            repos
                .columns()
                .create_or_get(column, table.id, ColumnType::Tag)
                .await
                .unwrap();

            let another_table = arbitrary_table(&mut *repos, another_table, &namespace).await;
            repos
                .columns()
                .create_or_get(another_column, another_table.id, ColumnType::Tag)
                .await
                .unwrap();
            catalog
        };

        // create grpc schema service
        let grpc = super::SchemaService::new(catalog);

        // request all tables for a namespace
        let request = GetSchemaRequest {
            namespace: namespace.to_string(),
            table: None,
        };
        let tonic_response = grpc.get_schema(Request::new(request)).await.unwrap();
        let response = tonic_response.into_inner();
        let schema = response.schema.unwrap();
        let mut table_names: Vec<_> = schema.tables.keys().collect();
        table_names.sort();
        assert_eq!(table_names, [another_table, table]);
        assert_eq!(
            schema
                .tables
                .get(table)
                .unwrap()
                .columns
                .keys()
                .collect::<Vec<_>>(),
            [column]
        );

        // request one table for a namespace
        let request = GetSchemaRequest {
            namespace: namespace.to_string(),
            table: Some(table.to_string()),
        };
        let tonic_response = grpc.get_schema(Request::new(request)).await.unwrap();
        let response = tonic_response.into_inner();
        let schema = response.schema.unwrap();
        let mut table_names: Vec<_> = schema.tables.keys().collect();
        table_names.sort();
        assert_eq!(table_names, [table]);
        assert_eq!(
            schema
                .tables
                .get("schema_test_table")
                .unwrap()
                .columns
                .keys()
                .collect::<Vec<_>>(),
            [column]
        );

        // request a nonexistent table for a namespace, which fails
        let request = GetSchemaRequest {
            namespace: namespace.to_string(),
            table: Some("does_not_exist".to_string()),
        };
        let tonic_status = grpc.get_schema(Request::new(request)).await.unwrap_err();
        assert_eq!(tonic_status.code(), Code::NotFound);
        assert_eq!(tonic_status.message(), "table does_not_exist not found");
    }
}
