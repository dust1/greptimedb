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

use api::v1::{CreateDatabaseExpr, ObjectExpr, ObjectResult};
use arrow_flight::flight_service_server::FlightService;
use arrow_flight::Ticket;
use async_trait::async_trait;
use common_error::prelude::BoxedError;
use common_grpc::flight;
use common_query::Output;
use prost::Message;
use query::plan::LogicalPlan;
use servers::query_handler::GrpcQueryHandler;
use snafu::prelude::*;
use substrait::{DFLogicalSubstraitConvertor, SubstraitPlan};
use table::requests::CreateDatabaseRequest;
use tonic::Request;

use crate::error::{
    DecodeLogicalPlanSnafu, ExecuteSqlSnafu, FlightGetSnafu, InvalidFlightDataSnafu, Result,
};
use crate::instance::Instance;

impl Instance {
    async fn boarding(&self, ticket: Request<Ticket>) -> Result<ObjectResult> {
        let response = self.do_get(ticket).await.context(FlightGetSnafu)?;
        flight::flight_data_to_object_result(response)
            .await
            .context(InvalidFlightDataSnafu)
    }

    pub(crate) async fn handle_create_database(&self, expr: CreateDatabaseExpr) -> Result<Output> {
        let req = CreateDatabaseRequest {
            db_name: expr.database_name,
        };
        self.sql_handler().create_database(req).await
    }

    pub(crate) async fn execute_logical(&self, plan_bytes: Vec<u8>) -> Result<Output> {
        let logical_plan = DFLogicalSubstraitConvertor
            .decode(plan_bytes.as_slice(), self.catalog_manager.clone())
            .context(DecodeLogicalPlanSnafu)?;

        self.query_engine
            .execute(&LogicalPlan::DfPlan(logical_plan))
            .await
            .context(ExecuteSqlSnafu)
    }
}

#[async_trait]
impl GrpcQueryHandler for Instance {
    async fn do_query(&self, query: ObjectExpr) -> servers::error::Result<ObjectResult> {
        let ticket = Request::new(Ticket {
            ticket: query.encode_to_vec(),
        });
        // TODO(LFC): Temporarily use old GRPC interface here, will get rid of them near the end of Arrow Flight adoption.
        self.boarding(ticket)
            .await
            .map_err(BoxedError::new)
            .with_context(|_| servers::error::ExecuteQuerySnafu {
                query: format!("{query:?}"),
            })
    }
}
