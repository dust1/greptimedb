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

pub mod alter;
pub mod create;
pub mod describe;
pub mod drop;
pub mod explain;
pub mod insert;
pub mod query;
pub mod show;
pub mod statement;
use std::str::FromStr;

use api::helper::ColumnDataTypeWrapper;
use common_base::bytes::Bytes;
use common_catalog::consts::{DEFAULT_CATALOG_NAME, DEFAULT_SCHEMA_NAME};
use common_time::Timestamp;
use datatypes::data_type::DataType;
use datatypes::prelude::ConcreteDataType;
use datatypes::schema::{ColumnDefaultConstraint, ColumnSchema};
use datatypes::types::DateTimeType;
use datatypes::value::Value;
use snafu::{ensure, ResultExt};

use crate::ast::{
    ColumnDef, ColumnOption, ColumnOptionDef, DataType as SqlDataType, Expr, ObjectName,
    Value as SqlValue,
};
use crate::error::{
    self, ColumnTypeMismatchSnafu, ConvertToGrpcDataTypeSnafu, ParseSqlValueSnafu, Result,
    SerializeColumnDefaultConstraintSnafu, UnsupportedDefaultValueSnafu,
};

// TODO(LFC): Get rid of this function, use session context aware version of "table_idents_to_full_name" instead.
// Current obstacles remain in some usage in Frontend, and other SQLs like "describe", "drop" etc.
/// Converts maybe fully-qualified table name (`<catalog>.<schema>.<table>` or `<table>` when
/// catalog and schema are default) to tuple.
pub fn table_idents_to_full_name(obj_name: &ObjectName) -> Result<(String, String, String)> {
    match &obj_name.0[..] {
        [table] => Ok((
            DEFAULT_CATALOG_NAME.to_string(),
            DEFAULT_SCHEMA_NAME.to_string(),
            table.value.clone(),
        )),
        [schema, table] => Ok((
            DEFAULT_CATALOG_NAME.to_string(),
            schema.value.clone(),
            table.value.clone(),
        )),
        [catalog, schema, table] => Ok((
            catalog.value.clone(),
            schema.value.clone(),
            table.value.clone(),
        )),
        _ => error::InvalidSqlSnafu {
            msg: format!(
                "expect table name to be <catalog>.<schema>.<table>, <schema>.<table> or <table>, actual: {obj_name}",
            ),
        }
        .fail(),
    }
}

fn parse_string_to_value(
    column_name: &str,
    s: String,
    data_type: &ConcreteDataType,
) -> Result<Value> {
    ensure!(
        data_type.is_stringifiable(),
        ColumnTypeMismatchSnafu {
            column_name,
            expect: data_type.clone(),
            actual: ConcreteDataType::string_datatype(),
        }
    );

    match data_type {
        ConcreteDataType::String(_) => Ok(Value::String(s.into())),
        ConcreteDataType::Date(_) => {
            if let Ok(date) = common_time::date::Date::from_str(&s) {
                Ok(Value::Date(date))
            } else {
                ParseSqlValueSnafu {
                    msg: format!("Failed to parse {s} to Date value"),
                }
                .fail()
            }
        }
        ConcreteDataType::DateTime(_) => {
            if let Ok(datetime) = common_time::datetime::DateTime::from_str(&s) {
                Ok(Value::DateTime(datetime))
            } else {
                ParseSqlValueSnafu {
                    msg: format!("Failed to parse {s} to DateTime value"),
                }
                .fail()
            }
        }
        ConcreteDataType::Timestamp(t) => {
            if let Ok(ts) = Timestamp::from_str(&s) {
                Ok(Value::Timestamp(Timestamp::new(
                    ts.convert_to(t.unit()),
                    t.unit(),
                )))
            } else {
                ParseSqlValueSnafu {
                    msg: format!("Failed to parse {s} to Timestamp value"),
                }
                .fail()
            }
        }
        _ => {
            unreachable!()
        }
    }
}

fn parse_hex_string(s: &str) -> Result<Value> {
    match hex::decode(s) {
        Ok(b) => Ok(Value::Binary(Bytes::from(b))),
        Err(hex::FromHexError::InvalidHexCharacter { c, index }) => ParseSqlValueSnafu {
            msg: format!(
                "Fail to parse hex string to Byte: invalid character {c:?} at position {index}"
            ),
        }
        .fail(),
        Err(hex::FromHexError::OddLength) => ParseSqlValueSnafu {
            msg: "Fail to parse hex string to Byte: odd number of digits".to_string(),
        }
        .fail(),
        Err(e) => ParseSqlValueSnafu {
            msg: format!("Fail to parse hex string to Byte {s}, {e:?}"),
        }
        .fail(),
    }
}

macro_rules! parse_number_to_value {
    ($data_type: expr, $n: ident,  $(($Type: ident, $PrimitiveType: ident)), +) => {
        match $data_type {
            $(
                ConcreteDataType::$Type(_) => {
                    let n  = parse_sql_number::<$PrimitiveType>($n)?;
                    Ok(Value::from(n))
                },
            )+
                _ => ParseSqlValueSnafu {
                    msg: format!("Fail to parse number {}, invalid column type: {:?}",
                                 $n, $data_type
                    )}.fail(),
        }
    }
}

/// Convert a sql value into datatype's value
pub fn sql_number_to_value(data_type: &ConcreteDataType, n: &str) -> Result<Value> {
    parse_number_to_value!(
        data_type,
        n,
        (UInt8, u8),
        (UInt16, u16),
        (UInt32, u32),
        (UInt64, u64),
        (Int8, i8),
        (Int16, i16),
        (Int32, i32),
        (Int64, i64),
        (Float64, f64),
        (Float32, f32),
        (Timestamp, i64)
    )
    // TODO(hl): also Date/DateTime
}

fn parse_sql_number<R: FromStr + std::fmt::Debug>(n: &str) -> Result<R>
where
    <R as FromStr>::Err: std::fmt::Debug,
{
    match n.parse::<R>() {
        Ok(n) => Ok(n),
        Err(e) => ParseSqlValueSnafu {
            msg: format!("Fail to parse number {n}, {e:?}"),
        }
        .fail(),
    }
}

pub fn sql_value_to_value(
    column_name: &str,
    data_type: &ConcreteDataType,
    sql_val: &SqlValue,
) -> Result<Value> {
    Ok(match sql_val {
        SqlValue::Number(n, _) => sql_number_to_value(data_type, n)?,
        SqlValue::Null => Value::Null,
        SqlValue::Boolean(b) => {
            ensure!(
                data_type.is_boolean(),
                ColumnTypeMismatchSnafu {
                    column_name,
                    expect: data_type.clone(),
                    actual: ConcreteDataType::boolean_datatype(),
                }
            );

            (*b).into()
        }
        SqlValue::DoubleQuotedString(s) | SqlValue::SingleQuotedString(s) => {
            parse_string_to_value(column_name, s.to_owned(), data_type)?
        }
        SqlValue::HexStringLiteral(s) => parse_hex_string(s)?,
        _ => todo!("Other sql value"),
    })
}

fn parse_column_default_constraint(
    column_name: &str,
    data_type: &ConcreteDataType,
    opts: &[ColumnOptionDef],
) -> Result<Option<ColumnDefaultConstraint>> {
    if let Some(opt) = opts
        .iter()
        .find(|o| matches!(o.option, ColumnOption::Default(_)))
    {
        let default_constraint = match &opt.option {
            ColumnOption::Default(Expr::Value(v)) => {
                ColumnDefaultConstraint::Value(sql_value_to_value(column_name, data_type, v)?)
            }
            ColumnOption::Default(Expr::Function(func)) => {
                // Always use lowercase for function expression
                ColumnDefaultConstraint::Function(format!("{func}").to_lowercase())
            }
            ColumnOption::Default(expr) => {
                return UnsupportedDefaultValueSnafu {
                    column_name,
                    expr: expr.clone(),
                }
                .fail();
            }
            _ => unreachable!(),
        };

        Ok(Some(default_constraint))
    } else {
        Ok(None)
    }
}

// TODO(yingwen): Make column nullable by default, and checks invalid case like
// a column is not nullable but has a default value null.
/// Create a `ColumnSchema` from `ColumnDef`.
pub fn column_def_to_schema(column_def: &ColumnDef, is_time_index: bool) -> Result<ColumnSchema> {
    let is_nullable = column_def
        .options
        .iter()
        .all(|o| !matches!(o.option, ColumnOption::NotNull))
        && !is_time_index;

    let name = column_def.name.value.clone();
    let data_type = sql_data_type_to_concrete_data_type(&column_def.data_type)?;
    let default_constraint =
        parse_column_default_constraint(&name, &data_type, &column_def.options)?;

    ColumnSchema::new(name, data_type, is_nullable)
        .with_time_index(is_time_index)
        .with_default_constraint(default_constraint)
        .context(error::InvalidDefaultSnafu {
            column: &column_def.name.value,
        })
}

/// Convert `ColumnDef` in sqlparser to `ColumnDef` in gRPC proto.
pub fn sql_column_def_to_grpc_column_def(col: ColumnDef) -> Result<api::v1::ColumnDef> {
    let name = col.name.value.clone();
    let data_type = sql_data_type_to_concrete_data_type(&col.data_type)?;

    let is_nullable = col
        .options
        .iter()
        .all(|o| !matches!(o.option, ColumnOption::NotNull));

    let default_constraint = parse_column_default_constraint(&name, &data_type, &col.options)?
        .map(ColumnDefaultConstraint::try_into) // serialize default constraint to bytes
        .transpose()
        .context(SerializeColumnDefaultConstraintSnafu)?;

    let data_type = ColumnDataTypeWrapper::try_from(data_type)
        .context(ConvertToGrpcDataTypeSnafu)?
        .datatype() as i32;
    Ok(api::v1::ColumnDef {
        name,
        datatype: data_type,
        is_nullable,
        default_constraint: default_constraint.unwrap_or_default(),
    })
}

pub fn sql_data_type_to_concrete_data_type(data_type: &SqlDataType) -> Result<ConcreteDataType> {
    match data_type {
        SqlDataType::BigInt(_) => Ok(ConcreteDataType::int64_datatype()),
        SqlDataType::Int(_) => Ok(ConcreteDataType::int32_datatype()),
        SqlDataType::SmallInt(_) => Ok(ConcreteDataType::int16_datatype()),
        SqlDataType::Char(_)
        | SqlDataType::Varchar(_)
        | SqlDataType::Text
        | SqlDataType::String => Ok(ConcreteDataType::string_datatype()),
        SqlDataType::Float(_) => Ok(ConcreteDataType::float32_datatype()),
        SqlDataType::Double => Ok(ConcreteDataType::float64_datatype()),
        SqlDataType::Boolean => Ok(ConcreteDataType::boolean_datatype()),
        SqlDataType::Date => Ok(ConcreteDataType::date_datatype()),
        SqlDataType::Varbinary(_) => Ok(ConcreteDataType::binary_datatype()),
        SqlDataType::Custom(obj_name, _) => match &obj_name.0[..] {
            [type_name] => {
                if type_name
                    .value
                    .eq_ignore_ascii_case(DateTimeType::default().name())
                {
                    Ok(ConcreteDataType::datetime_datatype())
                } else {
                    error::SqlTypeNotSupportedSnafu {
                        t: data_type.clone(),
                    }
                    .fail()
                }
            }
            _ => error::SqlTypeNotSupportedSnafu {
                t: data_type.clone(),
            }
            .fail(),
        },
        SqlDataType::Timestamp(_, _) => Ok(ConcreteDataType::timestamp_millisecond_datatype()),
        _ => error::SqlTypeNotSupportedSnafu {
            t: data_type.clone(),
        }
        .fail(),
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches::assert_matches;

    use api::v1::ColumnDataType;
    use common_time::timestamp::TimeUnit;
    use datatypes::types::BooleanType;
    use datatypes::value::OrderedFloat;

    use super::*;
    use crate::ast::{Ident, TimezoneInfo};
    use crate::statements::ColumnOption;

    fn check_type(sql_type: SqlDataType, data_type: ConcreteDataType) {
        assert_eq!(
            data_type,
            sql_data_type_to_concrete_data_type(&sql_type).unwrap()
        );
    }

    #[test]
    pub fn test_sql_data_type_to_concrete_data_type() {
        check_type(
            SqlDataType::BigInt(None),
            ConcreteDataType::int64_datatype(),
        );
        check_type(SqlDataType::Int(None), ConcreteDataType::int32_datatype());
        check_type(
            SqlDataType::SmallInt(None),
            ConcreteDataType::int16_datatype(),
        );
        check_type(SqlDataType::Char(None), ConcreteDataType::string_datatype());
        check_type(
            SqlDataType::Varchar(None),
            ConcreteDataType::string_datatype(),
        );
        check_type(SqlDataType::Text, ConcreteDataType::string_datatype());
        check_type(SqlDataType::String, ConcreteDataType::string_datatype());
        check_type(
            SqlDataType::Float(None),
            ConcreteDataType::float32_datatype(),
        );
        check_type(SqlDataType::Double, ConcreteDataType::float64_datatype());
        check_type(SqlDataType::Boolean, ConcreteDataType::boolean_datatype());
        check_type(SqlDataType::Date, ConcreteDataType::date_datatype());
        check_type(
            SqlDataType::Custom(ObjectName(vec![Ident::new("datetime")]), vec![]),
            ConcreteDataType::datetime_datatype(),
        );
        check_type(
            SqlDataType::Timestamp(None, TimezoneInfo::None),
            ConcreteDataType::timestamp_millisecond_datatype(),
        );
        check_type(
            SqlDataType::Varbinary(None),
            ConcreteDataType::binary_datatype(),
        );
    }

    #[test]
    fn test_sql_number_to_value() {
        let v = sql_number_to_value(&ConcreteDataType::float64_datatype(), "3.0").unwrap();
        assert_eq!(Value::Float64(OrderedFloat(3.0)), v);

        let v = sql_number_to_value(&ConcreteDataType::int32_datatype(), "999").unwrap();
        assert_eq!(Value::Int32(999), v);

        let v = sql_number_to_value(&ConcreteDataType::string_datatype(), "999");
        assert!(v.is_err(), "parse value error is: {v:?}");
    }

    #[test]
    fn test_sql_value_to_value() {
        let sql_val = SqlValue::Null;
        assert_eq!(
            Value::Null,
            sql_value_to_value("a", &ConcreteDataType::float64_datatype(), &sql_val).unwrap()
        );

        let sql_val = SqlValue::Boolean(true);
        assert_eq!(
            Value::Boolean(true),
            sql_value_to_value("a", &ConcreteDataType::boolean_datatype(), &sql_val).unwrap()
        );

        let sql_val = SqlValue::Number("3.0".to_string(), false);
        assert_eq!(
            Value::Float64(OrderedFloat(3.0)),
            sql_value_to_value("a", &ConcreteDataType::float64_datatype(), &sql_val).unwrap()
        );

        let sql_val = SqlValue::Number("3.0".to_string(), false);
        let v = sql_value_to_value("a", &ConcreteDataType::boolean_datatype(), &sql_val);
        assert!(v.is_err());
        assert!(format!("{v:?}")
            .contains("Fail to parse number 3.0, invalid column type: Boolean(BooleanType)"));

        let sql_val = SqlValue::Boolean(true);
        let v = sql_value_to_value("a", &ConcreteDataType::float64_datatype(), &sql_val);
        assert!(v.is_err());
        assert!(
            format!("{v:?}").contains(
                "column_name: \"a\", expect: Float64(Float64Type), actual: Boolean(BooleanType)"
            ),
            "v is {v:?}",
        );

        let sql_val = SqlValue::HexStringLiteral("48656c6c6f20776f726c6421".to_string());
        let v = sql_value_to_value("a", &ConcreteDataType::binary_datatype(), &sql_val).unwrap();
        assert_eq!(Value::Binary(Bytes::from(b"Hello world!".as_slice())), v);

        let sql_val = SqlValue::HexStringLiteral("9AF".to_string());
        let v = sql_value_to_value("a", &ConcreteDataType::binary_datatype(), &sql_val);
        assert!(v.is_err());
        assert!(
            format!("{v:?}").contains("odd number of digits"),
            "v is {v:?}"
        );

        let sql_val = SqlValue::HexStringLiteral("AG".to_string());
        let v = sql_value_to_value("a", &ConcreteDataType::binary_datatype(), &sql_val);
        assert!(v.is_err());
        assert!(format!("{v:?}").contains("invalid character"), "v is {v:?}",);
    }

    #[test]
    pub fn test_parse_date_literal() {
        let value = sql_value_to_value(
            "date",
            &ConcreteDataType::date_datatype(),
            &SqlValue::DoubleQuotedString("2022-02-22".to_string()),
        )
        .unwrap();
        assert_eq!(ConcreteDataType::date_datatype(), value.data_type());
        if let Value::Date(d) = value {
            assert_eq!("2022-02-22", d.to_string());
        } else {
            unreachable!()
        }
    }

    #[test]
    pub fn test_parse_datetime_literal() {
        let value = sql_value_to_value(
            "datetime_col",
            &ConcreteDataType::datetime_datatype(),
            &SqlValue::DoubleQuotedString("2022-02-22 00:01:03".to_string()),
        )
        .unwrap();
        assert_eq!(ConcreteDataType::datetime_datatype(), value.data_type());
        if let Value::DateTime(d) = value {
            assert_eq!("2022-02-22 00:01:03", d.to_string());
        } else {
            unreachable!()
        }
    }

    #[test]
    pub fn test_parse_illegal_datetime_literal() {
        assert!(sql_value_to_value(
            "datetime_col",
            &ConcreteDataType::datetime_datatype(),
            &SqlValue::DoubleQuotedString("2022-02-22 00:01:61".to_string()),
        )
        .is_err());
    }

    #[test]
    fn test_parse_timestamp_literal() {
        match parse_string_to_value(
            "timestamp_col",
            "2022-02-22T00:01:01+08:00".to_string(),
            &ConcreteDataType::timestamp_millisecond_datatype(),
        )
        .unwrap()
        {
            Value::Timestamp(ts) => {
                assert_eq!(1645459261000, ts.value());
                assert_eq!(TimeUnit::Millisecond, ts.unit());
            }
            _ => {
                unreachable!()
            }
        }

        match parse_string_to_value(
            "timestamp_col",
            "2022-02-22T00:01:01+08:00".to_string(),
            &ConcreteDataType::timestamp_datatype(TimeUnit::Second),
        )
        .unwrap()
        {
            Value::Timestamp(ts) => {
                assert_eq!(1645459261, ts.value());
                assert_eq!(TimeUnit::Second, ts.unit());
            }
            _ => {
                unreachable!()
            }
        }

        match parse_string_to_value(
            "timestamp_col",
            "2022-02-22T00:01:01+08:00".to_string(),
            &ConcreteDataType::timestamp_datatype(TimeUnit::Microsecond),
        )
        .unwrap()
        {
            Value::Timestamp(ts) => {
                assert_eq!(1645459261000000, ts.value());
                assert_eq!(TimeUnit::Microsecond, ts.unit());
            }
            _ => {
                unreachable!()
            }
        }

        match parse_string_to_value(
            "timestamp_col",
            "2022-02-22T00:01:01+08:00".to_string(),
            &ConcreteDataType::timestamp_datatype(TimeUnit::Nanosecond),
        )
        .unwrap()
        {
            Value::Timestamp(ts) => {
                assert_eq!(1645459261000000000, ts.value());
                assert_eq!(TimeUnit::Nanosecond, ts.unit());
            }
            _ => {
                unreachable!()
            }
        }

        assert!(parse_string_to_value(
            "timestamp_col",
            "2022-02-22T00:01:01+08".to_string(),
            &ConcreteDataType::timestamp_datatype(TimeUnit::Nanosecond),
        )
        .is_err());
    }

    #[test]
    pub fn test_parse_column_default_constraint() {
        let bool_value = sqlparser::ast::Value::Boolean(true);

        let opts = vec![
            ColumnOptionDef {
                name: None,
                option: ColumnOption::Default(Expr::Value(bool_value)),
            },
            ColumnOptionDef {
                name: None,
                option: ColumnOption::NotNull,
            },
        ];

        let constraint =
            parse_column_default_constraint("coll", &ConcreteDataType::Boolean(BooleanType), &opts)
                .unwrap();

        assert_matches!(
            constraint,
            Some(ColumnDefaultConstraint::Value(Value::Boolean(true)))
        );
    }

    #[test]
    pub fn test_sql_column_def_to_grpc_column_def() {
        // test basic
        let column_def = ColumnDef {
            name: "col".into(),
            data_type: SqlDataType::Double,
            collation: None,
            options: vec![],
        };

        let grpc_column_def = sql_column_def_to_grpc_column_def(column_def).unwrap();

        assert_eq!("col", grpc_column_def.name);
        assert!(grpc_column_def.is_nullable); // nullable when options are empty
        assert_eq!(ColumnDataType::Float64 as i32, grpc_column_def.datatype);
        assert!(grpc_column_def.default_constraint.is_empty());

        // test not null
        let column_def = ColumnDef {
            name: "col".into(),
            data_type: SqlDataType::Double,
            collation: None,
            options: vec![ColumnOptionDef {
                name: None,
                option: ColumnOption::NotNull,
            }],
        };

        let grpc_column_def = sql_column_def_to_grpc_column_def(column_def).unwrap();
        assert!(!grpc_column_def.is_nullable);
    }

    #[test]
    pub fn test_column_def_to_schema() {
        let column_def = ColumnDef {
            name: "col".into(),
            data_type: SqlDataType::Double,
            collation: None,
            options: vec![],
        };

        let column_schema = column_def_to_schema(&column_def, false).unwrap();

        assert_eq!("col", column_schema.name);
        assert_eq!(
            ConcreteDataType::float64_datatype(),
            column_schema.data_type
        );
        assert!(column_schema.is_nullable());
        assert!(!column_schema.is_time_index());

        let column_schema = column_def_to_schema(&column_def, true).unwrap();

        assert_eq!("col", column_schema.name);
        assert_eq!(
            ConcreteDataType::float64_datatype(),
            column_schema.data_type
        );
        assert!(!column_schema.is_nullable());
        assert!(column_schema.is_time_index());

        let column_def = ColumnDef {
            name: "col2".into(),
            data_type: SqlDataType::String,
            collation: None,
            options: vec![ColumnOptionDef {
                name: None,
                option: ColumnOption::NotNull,
            }],
        };

        let column_schema = column_def_to_schema(&column_def, false).unwrap();

        assert_eq!("col2", column_schema.name);
        assert_eq!(ConcreteDataType::string_datatype(), column_schema.data_type);
        assert!(!column_schema.is_nullable());
        assert!(!column_schema.is_time_index());
    }
}
