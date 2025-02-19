// Copyright 2022 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Planner, QueryEngine implementations based on DataFusion.

mod catalog_adapter;
mod error;
mod planner;

use std::sync::Arc;

use catalog::CatalogListRef;
use common_function::scalars::aggregate::AggregateFunctionMetaRef;
use common_function::scalars::udf::create_udf;
use common_function::scalars::FunctionRef;
use common_query::physical_plan::{DfPhysicalPlanAdapter, PhysicalPlan, PhysicalPlanAdapter};
use common_query::prelude::ScalarUdf;
use common_query::Output;
use common_recordbatch::adapter::RecordBatchStreamAdapter;
use common_recordbatch::{EmptyRecordBatchStream, SendableRecordBatchStream};
use common_telemetry::timer;
use datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec;
use datafusion::physical_plan::ExecutionPlan;
use session::context::QueryContextRef;
use snafu::{ensure, OptionExt, ResultExt};
use sql::dialect::GenericDialect;
use sql::parser::ParserContext;
use sql::statements::statement::Statement;

pub use crate::datafusion::catalog_adapter::DfCatalogListAdapter;
use crate::datafusion::planner::{DfContextProviderAdapter, DfPlanner};
use crate::error::Result;
use crate::executor::QueryExecutor;
use crate::logical_optimizer::LogicalOptimizer;
use crate::physical_optimizer::PhysicalOptimizer;
use crate::physical_planner::PhysicalPlanner;
use crate::plan::LogicalPlan;
use crate::planner::Planner;
use crate::query_engine::{QueryEngineContext, QueryEngineState};
use crate::{metric, QueryEngine};

pub(crate) struct DatafusionQueryEngine {
    state: QueryEngineState,
}

impl DatafusionQueryEngine {
    pub fn new(catalog_list: CatalogListRef) -> Self {
        Self {
            state: QueryEngineState::new(catalog_list.clone()),
        }
    }
}

// TODO(LFC): Refactor consideration: extract a "Planner" that stores query context and execute queries inside.
#[async_trait::async_trait]
impl QueryEngine for DatafusionQueryEngine {
    fn name(&self) -> &str {
        "datafusion"
    }

    fn sql_to_statement(&self, sql: &str) -> Result<Statement> {
        let mut statement = ParserContext::create_with_dialect(sql, &GenericDialect {})
            .context(error::ParseSqlSnafu)?;
        ensure!(1 == statement.len(), error::MultipleStatementsSnafu { sql });
        Ok(statement.remove(0))
    }

    fn statement_to_plan(
        &self,
        stmt: Statement,
        query_ctx: QueryContextRef,
    ) -> Result<LogicalPlan> {
        let context_provider = DfContextProviderAdapter::new(self.state.clone(), query_ctx);
        let planner = DfPlanner::new(&context_provider);

        planner.statement_to_plan(stmt)
    }

    fn sql_to_plan(&self, sql: &str, query_ctx: QueryContextRef) -> Result<LogicalPlan> {
        let _timer = timer!(metric::METRIC_PARSE_SQL_ELAPSED);
        let stmt = self.sql_to_statement(sql)?;
        self.statement_to_plan(stmt, query_ctx)
    }

    async fn execute(&self, plan: &LogicalPlan) -> Result<Output> {
        let mut ctx = QueryEngineContext::new(self.state.clone());
        let logical_plan = self.optimize_logical_plan(&mut ctx, plan)?;
        let physical_plan = self.create_physical_plan(&mut ctx, &logical_plan).await?;
        let physical_plan = self.optimize_physical_plan(&mut ctx, physical_plan)?;

        Ok(Output::Stream(
            self.execute_stream(&ctx, &physical_plan).await?,
        ))
    }

    async fn execute_physical(&self, plan: &Arc<dyn PhysicalPlan>) -> Result<Output> {
        let ctx = QueryEngineContext::new(self.state.clone());
        Ok(Output::Stream(self.execute_stream(&ctx, plan).await?))
    }

    fn register_udf(&self, udf: ScalarUdf) {
        self.state.register_udf(udf);
    }

    /// Note in SQL queries, aggregate names are looked up using
    /// lowercase unless the query uses quotes. For example,
    ///
    /// `SELECT MY_UDAF(x)...` will look for an aggregate named `"my_udaf"`
    /// `SELECT "my_UDAF"(x)` will look for an aggregate named `"my_UDAF"`
    ///
    /// So it's better to make UDAF name lowercase when creating one.
    fn register_aggregate_function(&self, func: AggregateFunctionMetaRef) {
        self.state.register_aggregate_function(func);
    }

    fn register_function(&self, func: FunctionRef) {
        self.state.register_udf(create_udf(func));
    }
}

impl LogicalOptimizer for DatafusionQueryEngine {
    fn optimize_logical_plan(
        &self,
        _: &mut QueryEngineContext,
        plan: &LogicalPlan,
    ) -> Result<LogicalPlan> {
        let _timer = timer!(metric::METRIC_OPTIMIZE_LOGICAL_ELAPSED);
        match plan {
            LogicalPlan::DfPlan(df_plan) => {
                let optimized_plan =
                    self.state
                        .optimize(df_plan)
                        .context(error::DatafusionSnafu {
                            msg: "Fail to optimize logical plan",
                        })?;

                Ok(LogicalPlan::DfPlan(optimized_plan))
            }
        }
    }
}

#[async_trait::async_trait]
impl PhysicalPlanner for DatafusionQueryEngine {
    async fn create_physical_plan(
        &self,
        _: &mut QueryEngineContext,
        logical_plan: &LogicalPlan,
    ) -> Result<Arc<dyn PhysicalPlan>> {
        let _timer = timer!(metric::METRIC_CREATE_PHYSICAL_ELAPSED);
        match logical_plan {
            LogicalPlan::DfPlan(df_plan) => {
                let physical_plan = self.state.create_physical_plan(df_plan).await.context(
                    error::DatafusionSnafu {
                        msg: "Fail to create physical plan",
                    },
                )?;

                Ok(Arc::new(PhysicalPlanAdapter::new(
                    Arc::new(
                        physical_plan
                            .schema()
                            .try_into()
                            .context(error::ConvertSchemaSnafu)?,
                    ),
                    physical_plan,
                )))
            }
        }
    }
}

impl PhysicalOptimizer for DatafusionQueryEngine {
    fn optimize_physical_plan(
        &self,
        _: &mut QueryEngineContext,
        plan: Arc<dyn PhysicalPlan>,
    ) -> Result<Arc<dyn PhysicalPlan>> {
        let _timer = timer!(metric::METRIC_OPTIMIZE_PHYSICAL_ELAPSED);

        let new_plan = plan
            .as_any()
            .downcast_ref::<PhysicalPlanAdapter>()
            .context(error::PhysicalPlanDowncastSnafu)?
            .df_plan();

        let new_plan =
            self.state
                .optimize_physical_plan(new_plan)
                .context(error::DatafusionSnafu {
                    msg: "Fail to optimize physical plan",
                })?;
        Ok(Arc::new(PhysicalPlanAdapter::new(plan.schema(), new_plan)))
    }
}

#[async_trait::async_trait]
impl QueryExecutor for DatafusionQueryEngine {
    async fn execute_stream(
        &self,
        ctx: &QueryEngineContext,
        plan: &Arc<dyn PhysicalPlan>,
    ) -> Result<SendableRecordBatchStream> {
        let _timer = timer!(metric::METRIC_EXEC_PLAN_ELAPSED);
        match plan.output_partitioning().partition_count() {
            0 => Ok(Box::pin(EmptyRecordBatchStream::new(plan.schema()))),
            1 => Ok(plan
                .execute(0, ctx.state().task_ctx())
                .context(error::ExecutePhysicalPlanSnafu)?),
            _ => {
                // merge into a single partition
                let plan =
                    CoalescePartitionsExec::new(Arc::new(DfPhysicalPlanAdapter(plan.clone())));
                // CoalescePartitionsExec must produce a single partition
                assert_eq!(1, plan.output_partitioning().partition_count());
                let df_stream =
                    plan.execute(0, ctx.state().task_ctx())
                        .context(error::DatafusionSnafu {
                            msg: "Failed to execute DataFusion merge exec",
                        })?;
                let stream = RecordBatchStreamAdapter::try_new(df_stream)
                    .context(error::ConvertDfRecordBatchStreamSnafu)?;
                Ok(Box::pin(stream))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use catalog::local::{MemoryCatalogProvider, MemorySchemaProvider};
    use catalog::{CatalogList, CatalogProvider, SchemaProvider};
    use common_catalog::consts::{DEFAULT_CATALOG_NAME, DEFAULT_SCHEMA_NAME};
    use common_query::Output;
    use common_recordbatch::util;
    use datatypes::vectors::{UInt64Vector, VectorRef};
    use session::context::QueryContext;
    use table::table::numbers::NumbersTable;

    use crate::query_engine::{QueryEngineFactory, QueryEngineRef};

    fn create_test_engine() -> QueryEngineRef {
        let catalog_list = catalog::local::new_memory_catalog_list().unwrap();

        let default_schema = Arc::new(MemorySchemaProvider::new());
        default_schema
            .register_table("numbers".to_string(), Arc::new(NumbersTable::default()))
            .unwrap();
        let default_catalog = Arc::new(MemoryCatalogProvider::new());
        default_catalog
            .register_schema(DEFAULT_SCHEMA_NAME.to_string(), default_schema)
            .unwrap();
        catalog_list
            .register_catalog(DEFAULT_CATALOG_NAME.to_string(), default_catalog)
            .unwrap();

        QueryEngineFactory::new(catalog_list).query_engine()
    }

    #[test]
    fn test_sql_to_plan() {
        let engine = create_test_engine();
        let sql = "select sum(number) from numbers limit 20";

        let plan = engine
            .sql_to_plan(sql, Arc::new(QueryContext::new()))
            .unwrap();

        // TODO(sunng87): do not rely on to_string for compare
        assert_eq!(
            format!("{plan:?}"),
            r#"DfPlan(Limit: skip=0, fetch=20
  Projection: SUM(numbers.number)
    Aggregate: groupBy=[[]], aggr=[[SUM(numbers.number)]]
      TableScan: numbers)"#
        );
    }

    #[tokio::test]
    async fn test_execute() {
        let engine = create_test_engine();
        let sql = "select sum(number) from numbers limit 20";

        let plan = engine
            .sql_to_plan(sql, Arc::new(QueryContext::new()))
            .unwrap();

        let output = engine.execute(&plan).await.unwrap();

        match output {
            Output::Stream(recordbatch) => {
                let numbers = util::collect(recordbatch).await.unwrap();
                assert_eq!(1, numbers.len());
                assert_eq!(numbers[0].num_columns(), 1);
                assert_eq!(1, numbers[0].schema.num_columns());
                assert_eq!(
                    "SUM(numbers.number)",
                    numbers[0].schema.column_schemas()[0].name
                );

                let batch = &numbers[0];
                assert_eq!(1, batch.num_columns());
                assert_eq!(batch.column(0).len(), 1);

                assert_eq!(
                    *batch.column(0),
                    Arc::new(UInt64Vector::from_slice(&[4950])) as VectorRef
                );
            }
            _ => unreachable!(),
        }
    }
}
