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

use api::v1::ddl_request::Expr as DdlExpr;
use api::v1::{
    object_expr, query_request, AlterExpr, CreateTableExpr, DatabaseRequest, DdlRequest,
    DropTableExpr, InsertRequest, ObjectExpr, ObjectResult as GrpcObjectResult, QueryRequest,
};
use common_error::status_code::StatusCode;
use common_grpc::flight::{
    flight_messages_to_recordbatches, raw_flight_data_to_message, FlightMessage,
};
use common_query::Output;
use common_recordbatch::RecordBatches;
use snafu::{ensure, OptionExt, ResultExt};

use crate::error::{ConvertFlightDataSnafu, DatanodeSnafu, IllegalFlightMessagesSnafu};
use crate::{error, Client, Result};

#[derive(Clone, Debug)]
pub struct Database {
    name: String,
    client: Client,
}

impl Database {
    pub fn new(name: impl Into<String>, client: Client) -> Self {
        Self {
            name: name.into(),
            client,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn insert(&self, request: InsertRequest) -> Result<RpcOutput> {
        let expr = ObjectExpr {
            request: Some(object_expr::Request::Insert(request)),
        };
        self.object(expr).await?.try_into()
    }

    pub async fn sql(&self, sql: &str) -> Result<RpcOutput> {
        let query = QueryRequest {
            query: Some(query_request::Query::Sql(sql.to_string())),
        };
        self.do_query(query).await
    }

    pub async fn logical_plan(&self, logical_plan: Vec<u8>) -> Result<RpcOutput> {
        let query = QueryRequest {
            query: Some(query_request::Query::LogicalPlan(logical_plan)),
        };
        self.do_query(query).await
    }

    async fn do_query(&self, request: QueryRequest) -> Result<RpcOutput> {
        let expr = ObjectExpr {
            request: Some(object_expr::Request::Query(request)),
        };

        let obj_result = self.object(expr).await?;
        obj_result.try_into()
    }

    pub async fn create(&self, expr: CreateTableExpr) -> Result<RpcOutput> {
        let expr = ObjectExpr {
            request: Some(object_expr::Request::Ddl(DdlRequest {
                expr: Some(DdlExpr::CreateTable(expr)),
            })),
        };
        self.object(expr).await?.try_into()
    }

    pub async fn alter(&self, expr: AlterExpr) -> Result<RpcOutput> {
        let expr = ObjectExpr {
            request: Some(object_expr::Request::Ddl(DdlRequest {
                expr: Some(DdlExpr::Alter(expr)),
            })),
        };
        self.object(expr).await?.try_into()
    }

    pub async fn drop_table(&self, expr: DropTableExpr) -> Result<RpcOutput> {
        let expr = ObjectExpr {
            request: Some(object_expr::Request::Ddl(DdlRequest {
                expr: Some(DdlExpr::DropTable(expr)),
            })),
        };
        self.object(expr).await?.try_into()
    }

    pub async fn object(&self, expr: ObjectExpr) -> Result<GrpcObjectResult> {
        let res = self.objects(vec![expr]).await?.pop().unwrap();
        Ok(res)
    }

    async fn objects(&self, exprs: Vec<ObjectExpr>) -> Result<Vec<GrpcObjectResult>> {
        let expr_count = exprs.len();
        let req = DatabaseRequest {
            name: self.name.clone(),
            exprs,
        };

        let res = self.client.database(req).await?;
        let res = res.results;

        ensure!(
            res.len() == expr_count,
            error::MissingResultSnafu {
                name: "object_results",
                expected: expr_count,
                actual: res.len(),
            }
        );

        Ok(res)
    }
}

#[derive(Debug)]
pub enum RpcOutput {
    RecordBatches(RecordBatches),
    AffectedRows(usize),
}

impl TryFrom<api::v1::ObjectResult> for RpcOutput {
    type Error = error::Error;

    fn try_from(object_result: api::v1::ObjectResult) -> std::result::Result<Self, Self::Error> {
        let header = object_result.header.context(error::MissingHeaderSnafu)?;
        if !StatusCode::is_success(header.code) {
            return DatanodeSnafu {
                code: header.code,
                msg: header.err_msg,
            }
            .fail();
        }

        let flight_messages = raw_flight_data_to_message(object_result.flight_data)
            .context(ConvertFlightDataSnafu)?;

        let output = if let Some(FlightMessage::AffectedRows(rows)) = flight_messages.get(0) {
            ensure!(
                flight_messages.len() == 1,
                IllegalFlightMessagesSnafu {
                    reason: "Expect 'AffectedRows' Flight messages to be one and only!"
                }
            );
            RpcOutput::AffectedRows(*rows)
        } else {
            let recordbatches = flight_messages_to_recordbatches(flight_messages)
                .context(ConvertFlightDataSnafu)?;
            RpcOutput::RecordBatches(recordbatches)
        };
        Ok(output)
    }
}

impl From<RpcOutput> for Output {
    fn from(value: RpcOutput) -> Self {
        match value {
            RpcOutput::AffectedRows(x) => Output::AffectedRows(x),
            RpcOutput::RecordBatches(x) => Output::RecordBatches(x),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use api::helper::ColumnDataTypeWrapper;
    use api::v1::Column;
    use common_grpc::select::{null_mask, values};
    use common_grpc_expr::column_to_vector;
    use datatypes::prelude::{Vector, VectorRef};
    use datatypes::vectors::{
        BinaryVector, BooleanVector, DateTimeVector, DateVector, Float32Vector, Float64Vector,
        Int16Vector, Int32Vector, Int64Vector, Int8Vector, StringVector, UInt16Vector,
        UInt32Vector, UInt64Vector, UInt8Vector,
    };

    #[test]
    fn test_column_to_vector() {
        let mut column = create_test_column(Arc::new(BooleanVector::from(vec![true])));
        column.datatype = -100;
        let result = column_to_vector(&column, 1);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "Column datatype error, source: Unknown proto column datatype: -100"
        );

        macro_rules! test_with_vector {
            ($vector: expr) => {
                let vector = Arc::new($vector);
                let column = create_test_column(vector.clone());
                let result = column_to_vector(&column, vector.len() as u32).unwrap();
                assert_eq!(result, vector as VectorRef);
            };
        }

        test_with_vector!(BooleanVector::from(vec![Some(true), None, Some(false)]));
        test_with_vector!(Int8Vector::from(vec![Some(i8::MIN), None, Some(i8::MAX)]));
        test_with_vector!(Int16Vector::from(vec![
            Some(i16::MIN),
            None,
            Some(i16::MAX)
        ]));
        test_with_vector!(Int32Vector::from(vec![
            Some(i32::MIN),
            None,
            Some(i32::MAX)
        ]));
        test_with_vector!(Int64Vector::from(vec![
            Some(i64::MIN),
            None,
            Some(i64::MAX)
        ]));
        test_with_vector!(UInt8Vector::from(vec![Some(u8::MIN), None, Some(u8::MAX)]));
        test_with_vector!(UInt16Vector::from(vec![
            Some(u16::MIN),
            None,
            Some(u16::MAX)
        ]));
        test_with_vector!(UInt32Vector::from(vec![
            Some(u32::MIN),
            None,
            Some(u32::MAX)
        ]));
        test_with_vector!(UInt64Vector::from(vec![
            Some(u64::MIN),
            None,
            Some(u64::MAX)
        ]));
        test_with_vector!(Float32Vector::from(vec![
            Some(f32::MIN),
            None,
            Some(f32::MAX)
        ]));
        test_with_vector!(Float64Vector::from(vec![
            Some(f64::MIN),
            None,
            Some(f64::MAX)
        ]));
        test_with_vector!(BinaryVector::from(vec![
            Some(b"".to_vec()),
            None,
            Some(b"hello".to_vec())
        ]));
        test_with_vector!(StringVector::from(vec![Some(""), None, Some("foo"),]));
        test_with_vector!(DateVector::from(vec![Some(1), None, Some(3)]));
        test_with_vector!(DateTimeVector::from(vec![Some(4), None, Some(6)]));
    }

    fn create_test_column(vector: VectorRef) -> Column {
        let wrapper: ColumnDataTypeWrapper = vector.data_type().try_into().unwrap();
        Column {
            column_name: "test".to_string(),
            semantic_type: 1,
            values: Some(values(&[vector.clone()]).unwrap()),
            null_mask: null_mask(&[vector.clone()], vector.len()),
            datatype: wrapper.datatype() as i32,
        }
    }
}
