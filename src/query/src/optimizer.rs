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

use std::str::FromStr;
use std::sync::Arc;

use common_time::timestamp::{TimeUnit, Timestamp};
use datafusion::optimizer::optimizer::OptimizerRule;
use datafusion::optimizer::OptimizerConfig;
use datafusion_common::{DFSchemaRef, DataFusionError, Result, ScalarValue};
use datafusion_expr::expr_rewriter::{ExprRewritable, ExprRewriter};
use datafusion_expr::{
    Between, BinaryExpr, Expr, ExprSchemable, Filter, LogicalPlan, Operator, TableScan,
};
use datatypes::arrow::compute;
use datatypes::arrow::datatypes::DataType;

/// TypeConversionRule converts some literal values in logical plan to other types according
/// to data type of corresponding columns.
/// Specifically:
/// - string literal of timestamp is converted to `Expr::Literal(ScalarValue::TimestampMillis)`
/// - string literal of boolean is converted to `Expr::Literal(ScalarValue::Boolean)`
pub struct TypeConversionRule;

impl OptimizerRule for TypeConversionRule {
    fn try_optimize(
        &self,
        plan: &LogicalPlan,
        _config: &dyn OptimizerConfig,
    ) -> Result<Option<LogicalPlan>> {
        let mut converter = TypeConverter {
            schemas: plan.all_schemas(),
        };

        match plan {
            LogicalPlan::Filter(filter) => {
                let rewritten = filter.predicate().clone().rewrite(&mut converter)?;
                let Some(plan) = self.try_optimize(filter.input(), _config)? else { return Ok(None) };
                Ok(Some(LogicalPlan::Filter(Filter::try_new(
                    rewritten,
                    Arc::new(plan),
                )?)))
            }
            LogicalPlan::TableScan(TableScan {
                table_name,
                source,
                projection,
                projected_schema,
                filters,
                fetch,
            }) => {
                let rewrite_filters = filters
                    .clone()
                    .into_iter()
                    .map(|e| e.rewrite(&mut converter))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Some(LogicalPlan::TableScan(TableScan {
                    table_name: table_name.clone(),
                    source: source.clone(),
                    projection: projection.clone(),
                    projected_schema: projected_schema.clone(),
                    filters: rewrite_filters,
                    fetch: *fetch,
                })))
            }
            LogicalPlan::Projection { .. }
            | LogicalPlan::Window { .. }
            | LogicalPlan::Aggregate { .. }
            | LogicalPlan::Repartition { .. }
            | LogicalPlan::CreateExternalTable { .. }
            | LogicalPlan::Extension { .. }
            | LogicalPlan::Sort { .. }
            | LogicalPlan::Explain { .. }
            | LogicalPlan::Limit { .. }
            | LogicalPlan::Union { .. }
            | LogicalPlan::Join { .. }
            | LogicalPlan::CrossJoin { .. }
            | LogicalPlan::CreateMemoryTable { .. }
            | LogicalPlan::DropTable { .. }
            | LogicalPlan::DropView { .. }
            | LogicalPlan::Distinct { .. }
            | LogicalPlan::Values { .. }
            | LogicalPlan::SetVariable { .. }
            | LogicalPlan::Analyze { .. } => {
                let inputs = plan.inputs();
                let mut new_inputs = Vec::with_capacity(inputs.len());
                for input in inputs {
                    let Some(plan) = self.try_optimize(input, _config)? else { return Ok(None) };
                    new_inputs.push(plan);
                }

                let expr = plan
                    .expressions()
                    .into_iter()
                    .map(|e| e.rewrite(&mut converter))
                    .collect::<Result<Vec<_>>>()?;

                datafusion_expr::utils::from_plan(plan, &expr, &new_inputs).map(Some)
            }

            LogicalPlan::Subquery { .. }
            | LogicalPlan::SubqueryAlias { .. }
            | LogicalPlan::CreateView { .. }
            | LogicalPlan::CreateCatalogSchema { .. }
            | LogicalPlan::CreateCatalog { .. }
            | LogicalPlan::EmptyRelation(_)
            | LogicalPlan::Prepare(_) => Ok(Some(plan.clone())),
        }
    }

    fn name(&self) -> &str {
        "TypeConversionRule"
    }
}

struct TypeConverter<'a> {
    schemas: Vec<&'a DFSchemaRef>,
}

impl<'a> TypeConverter<'a> {
    fn column_type(&self, expr: &Expr) -> Option<DataType> {
        if let Expr::Column(_) = expr {
            for schema in &self.schemas {
                if let Ok(v) = expr.get_type(schema) {
                    return Some(v);
                }
            }
        }
        None
    }

    fn cast_scalar_value(value: &ScalarValue, target_type: &DataType) -> Result<ScalarValue> {
        match (target_type, value) {
            (DataType::Timestamp(_, _), ScalarValue::Utf8(Some(v))) => string_to_timestamp_ms(v),
            (DataType::Boolean, ScalarValue::Utf8(Some(v))) => match v.to_lowercase().as_str() {
                "true" => Ok(ScalarValue::Boolean(Some(true))),
                "false" => Ok(ScalarValue::Boolean(Some(false))),
                _ => Ok(ScalarValue::Boolean(None)),
            },
            (target_type, value) => {
                let value_arr = value.to_array();
                let arr =
                    compute::cast(&value_arr, target_type).map_err(DataFusionError::ArrowError)?;

                ScalarValue::try_from_array(
                    &arr,
                    0, // index: Converts a value in `array` at `index` into a ScalarValue
                )
            }
        }
    }

    fn convert_type<'b>(&self, mut left: &'b Expr, mut right: &'b Expr) -> Result<(Expr, Expr)> {
        let left_type = self.column_type(left);
        let right_type = self.column_type(right);

        let mut reverse = false;
        let left_type = match (&left_type, &right_type) {
            (Some(v), None) => v,
            (None, Some(v)) => {
                reverse = true;
                std::mem::swap(&mut left, &mut right);
                v
            }
            _ => return Ok((left.clone(), right.clone())),
        };

        match (left, right) {
            (Expr::Column(col), Expr::Literal(value)) => {
                let casted_right = Self::cast_scalar_value(value, left_type)?;
                if casted_right.is_null() {
                    return Err(DataFusionError::Plan(format!(
                        "column:{col:?} value:{value:?} is invalid",
                    )));
                }
                if reverse {
                    Ok((Expr::Literal(casted_right), left.clone()))
                } else {
                    Ok((left.clone(), Expr::Literal(casted_right)))
                }
            }
            _ => Ok((left.clone(), right.clone())),
        }
    }
}

impl<'a> ExprRewriter for TypeConverter<'a> {
    fn mutate(&mut self, expr: Expr) -> Result<Expr> {
        let new_expr = match expr {
            Expr::BinaryExpr(BinaryExpr { left, op, right }) => match op {
                Operator::Eq
                | Operator::NotEq
                | Operator::Lt
                | Operator::LtEq
                | Operator::Gt
                | Operator::GtEq => {
                    let (left, right) = self.convert_type(&left, &right)?;
                    Expr::BinaryExpr(BinaryExpr {
                        left: Box::new(left),
                        op,
                        right: Box::new(right),
                    })
                }
                _ => Expr::BinaryExpr(BinaryExpr { left, op, right }),
            },
            Expr::Between(Between {
                expr,
                negated,
                low,
                high,
            }) => {
                let (expr, low) = self.convert_type(&expr, &low)?;
                let (expr, high) = self.convert_type(&expr, &high)?;
                Expr::Between(Between {
                    expr: Box::new(expr),
                    negated,
                    low: Box::new(low),
                    high: Box::new(high),
                })
            }
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let mut list_expr = Vec::with_capacity(list.len());
                for e in list {
                    let (_, expr_conversion) = self.convert_type(&expr, &e)?;
                    list_expr.push(expr_conversion);
                }
                Expr::InList {
                    expr,
                    list: list_expr,
                    negated,
                }
            }
            Expr::Literal(value) => match value {
                ScalarValue::TimestampSecond(Some(i), _) => {
                    timestamp_to_timestamp_ms_expr(i, TimeUnit::Second)
                }
                ScalarValue::TimestampMillisecond(Some(i), _) => {
                    timestamp_to_timestamp_ms_expr(i, TimeUnit::Millisecond)
                }

                ScalarValue::TimestampMicrosecond(Some(i), _) => {
                    timestamp_to_timestamp_ms_expr(i, TimeUnit::Microsecond)
                }
                ScalarValue::TimestampNanosecond(Some(i), _) => {
                    timestamp_to_timestamp_ms_expr(i, TimeUnit::Nanosecond)
                }
                _ => Expr::Literal(value),
            },
            expr => expr,
        };
        Ok(new_expr)
    }
}

fn timestamp_to_timestamp_ms_expr(val: i64, unit: TimeUnit) -> Expr {
    let timestamp = match unit {
        TimeUnit::Second => val * 1_000,
        TimeUnit::Millisecond => val,
        TimeUnit::Microsecond => val / 1_000,
        TimeUnit::Nanosecond => val / 1_000 / 1_000,
    };

    Expr::Literal(ScalarValue::TimestampMillisecond(Some(timestamp), None))
}

fn string_to_timestamp_ms(string: &str) -> Result<ScalarValue> {
    Ok(ScalarValue::TimestampMillisecond(
        Some(
            Timestamp::from_str(string)
                .map(|t| t.value() / 1_000_000)
                .map_err(|e| DataFusionError::External(Box::new(e)))?,
        ),
        None,
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use datafusion_common::{Column, DFField, DFSchema};

    use super::*;

    #[test]
    fn test_string_to_timestamp_ms() {
        assert!(matches!(
            string_to_timestamp_ms("2022-02-02 19:00:00+08:00").unwrap(),
            ScalarValue::TimestampMillisecond(Some(1643799600000), None)
        ));
        assert!(matches!(
            string_to_timestamp_ms("2009-02-13 23:31:30Z").unwrap(),
            ScalarValue::TimestampMillisecond(Some(1234567890000), None)
        ));
    }

    #[test]
    fn test_timestamp_to_timestamp_ms_expr() {
        assert!(matches!(
            timestamp_to_timestamp_ms_expr(123, TimeUnit::Second),
            Expr::Literal(ScalarValue::TimestampMillisecond(Some(123000), None))
        ));

        assert!(matches!(
            timestamp_to_timestamp_ms_expr(123, TimeUnit::Millisecond),
            Expr::Literal(ScalarValue::TimestampMillisecond(Some(123), None))
        ));

        assert!(matches!(
            timestamp_to_timestamp_ms_expr(123, TimeUnit::Microsecond),
            Expr::Literal(ScalarValue::TimestampMillisecond(Some(0), None))
        ));

        assert!(matches!(
            timestamp_to_timestamp_ms_expr(1230, TimeUnit::Microsecond),
            Expr::Literal(ScalarValue::TimestampMillisecond(Some(1), None))
        ));

        assert!(matches!(
            timestamp_to_timestamp_ms_expr(123000, TimeUnit::Microsecond),
            Expr::Literal(ScalarValue::TimestampMillisecond(Some(123), None))
        ));

        assert!(matches!(
            timestamp_to_timestamp_ms_expr(1230, TimeUnit::Nanosecond),
            Expr::Literal(ScalarValue::TimestampMillisecond(Some(0), None))
        ));
        assert!(matches!(
            timestamp_to_timestamp_ms_expr(123_000_000, TimeUnit::Nanosecond),
            Expr::Literal(ScalarValue::TimestampMillisecond(Some(123), None))
        ));
    }

    #[test]
    fn test_convert_timestamp_str() {
        use datatypes::arrow::datatypes::TimeUnit as ArrowTimeUnit;

        let schema_ref = Arc::new(
            DFSchema::new_with_metadata(
                vec![DFField::new(
                    None,
                    "ts",
                    DataType::Timestamp(ArrowTimeUnit::Millisecond, None),
                    true,
                )],
                HashMap::new(),
            )
            .unwrap(),
        );
        let mut converter = TypeConverter {
            schemas: vec![&schema_ref],
        };

        assert_eq!(
            Expr::Column(Column::from_name("ts")).gt(Expr::Literal(
                ScalarValue::TimestampMillisecond(Some(1599514949000), None)
            )),
            converter
                .mutate(
                    Expr::Column(Column::from_name("ts")).gt(Expr::Literal(ScalarValue::Utf8(
                        Some("2020-09-08T05:42:29+08:00".to_string()),
                    )))
                )
                .unwrap()
        );
    }

    #[test]
    fn test_convert_bool() {
        let col_name = "is_valid";
        let schema_ref = Arc::new(
            DFSchema::new_with_metadata(
                vec![DFField::new(None, col_name, DataType::Boolean, false)],
                HashMap::new(),
            )
            .unwrap(),
        );
        let mut converter = TypeConverter {
            schemas: vec![&schema_ref],
        };

        assert_eq!(
            Expr::Column(Column::from_name(col_name))
                .eq(Expr::Literal(ScalarValue::Boolean(Some(true)))),
            converter
                .mutate(
                    Expr::Column(Column::from_name(col_name))
                        .eq(Expr::Literal(ScalarValue::Utf8(Some("true".to_string()))))
                )
                .unwrap()
        );
    }
}
