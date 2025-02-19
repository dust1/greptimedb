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

//! Vector helper functions, inspired by databend Series mod

use std::any::Any;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, StringArray};
use arrow::compute;
use arrow::compute::kernels::comparison;
use arrow::datatypes::{DataType as ArrowDataType, TimeUnit};
use datafusion_common::ScalarValue;
use snafu::{OptionExt, ResultExt};

use crate::data_type::ConcreteDataType;
use crate::error::{self, Result};
use crate::scalars::{Scalar, ScalarVectorBuilder};
use crate::value::{ListValue, ListValueRef};
use crate::vectors::{
    BinaryVector, BooleanVector, ConstantVector, DateTimeVector, DateVector, Float32Vector,
    Float64Vector, Int16Vector, Int32Vector, Int64Vector, Int8Vector, ListVector,
    ListVectorBuilder, MutableVector, NullVector, StringVector, TimestampMicrosecondVector,
    TimestampMillisecondVector, TimestampNanosecondVector, TimestampSecondVector, UInt16Vector,
    UInt32Vector, UInt64Vector, UInt8Vector, Vector, VectorRef,
};

/// Helper functions for `Vector`.
pub struct Helper;

impl Helper {
    /// Get a pointer to the underlying data of this vectors.
    /// Can be useful for fast comparisons.
    /// # Safety
    /// Assumes that the `vector` is  T.
    pub unsafe fn static_cast<T: Any>(vector: &VectorRef) -> &T {
        let object = vector.as_ref();
        debug_assert!(object.as_any().is::<T>());
        &*(object as *const dyn Vector as *const T)
    }

    pub fn check_get_scalar<T: Scalar>(vector: &VectorRef) -> Result<&<T as Scalar>::VectorType> {
        let arr = vector
            .as_any()
            .downcast_ref::<<T as Scalar>::VectorType>()
            .with_context(|| error::UnknownVectorSnafu {
                msg: format!(
                    "downcast vector error, vector type: {:?}, expected vector: {:?}",
                    vector.vector_type_name(),
                    std::any::type_name::<T>(),
                ),
            });
        arr
    }

    pub fn check_get<T: 'static + Vector>(vector: &VectorRef) -> Result<&T> {
        let arr = vector
            .as_any()
            .downcast_ref::<T>()
            .with_context(|| error::UnknownVectorSnafu {
                msg: format!(
                    "downcast vector error, vector type: {:?}, expected vector: {:?}",
                    vector.vector_type_name(),
                    std::any::type_name::<T>(),
                ),
            });
        arr
    }

    pub fn check_get_mutable_vector<T: 'static + MutableVector>(
        vector: &mut dyn MutableVector,
    ) -> Result<&mut T> {
        let ty = vector.data_type();
        let arr = vector
            .as_mut_any()
            .downcast_mut()
            .with_context(|| error::UnknownVectorSnafu {
                msg: format!(
                    "downcast vector error, vector type: {:?}, expected vector: {:?}",
                    ty,
                    std::any::type_name::<T>(),
                ),
            });
        arr
    }

    pub fn check_get_scalar_vector<T: Scalar>(
        vector: &VectorRef,
    ) -> Result<&<T as Scalar>::VectorType> {
        let arr = vector
            .as_any()
            .downcast_ref::<<T as Scalar>::VectorType>()
            .with_context(|| error::UnknownVectorSnafu {
                msg: format!(
                    "downcast vector error, vector type: {:?}, expected vector: {:?}",
                    vector.vector_type_name(),
                    std::any::type_name::<T>(),
                ),
            });
        arr
    }

    /// Try to cast an arrow scalar value into vector
    pub fn try_from_scalar_value(value: ScalarValue, length: usize) -> Result<VectorRef> {
        let vector = match value {
            ScalarValue::Null => ConstantVector::new(Arc::new(NullVector::new(1)), length),
            ScalarValue::Boolean(v) => {
                ConstantVector::new(Arc::new(BooleanVector::from(vec![v])), length)
            }
            ScalarValue::Float32(v) => {
                ConstantVector::new(Arc::new(Float32Vector::from(vec![v])), length)
            }
            ScalarValue::Float64(v) => {
                ConstantVector::new(Arc::new(Float64Vector::from(vec![v])), length)
            }
            ScalarValue::Int8(v) => {
                ConstantVector::new(Arc::new(Int8Vector::from(vec![v])), length)
            }
            ScalarValue::Int16(v) => {
                ConstantVector::new(Arc::new(Int16Vector::from(vec![v])), length)
            }
            ScalarValue::Int32(v) => {
                ConstantVector::new(Arc::new(Int32Vector::from(vec![v])), length)
            }
            ScalarValue::Int64(v) => {
                ConstantVector::new(Arc::new(Int64Vector::from(vec![v])), length)
            }
            ScalarValue::UInt8(v) => {
                ConstantVector::new(Arc::new(UInt8Vector::from(vec![v])), length)
            }
            ScalarValue::UInt16(v) => {
                ConstantVector::new(Arc::new(UInt16Vector::from(vec![v])), length)
            }
            ScalarValue::UInt32(v) => {
                ConstantVector::new(Arc::new(UInt32Vector::from(vec![v])), length)
            }
            ScalarValue::UInt64(v) => {
                ConstantVector::new(Arc::new(UInt64Vector::from(vec![v])), length)
            }
            ScalarValue::Utf8(v) | ScalarValue::LargeUtf8(v) => {
                ConstantVector::new(Arc::new(StringVector::from(vec![v])), length)
            }
            ScalarValue::Binary(v)
            | ScalarValue::LargeBinary(v)
            | ScalarValue::FixedSizeBinary(_, v) => {
                ConstantVector::new(Arc::new(BinaryVector::from(vec![v])), length)
            }
            ScalarValue::List(v, field) => {
                let item_type = ConcreteDataType::try_from(field.data_type())?;
                let mut builder = ListVectorBuilder::with_type_capacity(item_type.clone(), 1);
                if let Some(values) = v {
                    let values = values
                        .into_iter()
                        .map(ScalarValue::try_into)
                        .collect::<Result<_>>()?;
                    let list_value = ListValue::new(Some(Box::new(values)), item_type);
                    builder.push(Some(ListValueRef::Ref { val: &list_value }));
                } else {
                    builder.push(None);
                }
                let list_vector = builder.to_vector();
                ConstantVector::new(list_vector, length)
            }
            ScalarValue::Date32(v) => {
                ConstantVector::new(Arc::new(DateVector::from(vec![v])), length)
            }
            ScalarValue::Date64(v) => {
                ConstantVector::new(Arc::new(DateTimeVector::from(vec![v])), length)
            }
            ScalarValue::TimestampSecond(v, _) => {
                // Timezone is unimplemented now.
                ConstantVector::new(Arc::new(TimestampSecondVector::from(vec![v])), length)
            }
            ScalarValue::TimestampMillisecond(v, _) => {
                // Timezone is unimplemented now.
                ConstantVector::new(Arc::new(TimestampMillisecondVector::from(vec![v])), length)
            }
            ScalarValue::TimestampMicrosecond(v, _) => {
                // Timezone is unimplemented now.
                ConstantVector::new(Arc::new(TimestampMicrosecondVector::from(vec![v])), length)
            }
            ScalarValue::TimestampNanosecond(v, _) => {
                // Timezone is unimplemented now.
                ConstantVector::new(Arc::new(TimestampNanosecondVector::from(vec![v])), length)
            }
            ScalarValue::Decimal128(_, _, _)
            | ScalarValue::IntervalYearMonth(_)
            | ScalarValue::IntervalDayTime(_)
            | ScalarValue::IntervalMonthDayNano(_)
            | ScalarValue::Struct(_, _)
            | ScalarValue::Dictionary(_, _)
            | ScalarValue::Time32Second(_)
            | ScalarValue::Time32Millisecond(_)
            | ScalarValue::Time64Microsecond(_)
            | ScalarValue::Time64Nanosecond(_) => {
                return error::ConversionSnafu {
                    from: format!("Unsupported scalar value: {value}"),
                }
                .fail()
            }
        };

        Ok(Arc::new(vector))
    }

    /// Try to cast an arrow array into vector
    ///
    /// # Panics
    /// Panic if given arrow data type is not supported.
    pub fn try_into_vector(array: impl AsRef<dyn Array>) -> Result<VectorRef> {
        Ok(match array.as_ref().data_type() {
            ArrowDataType::Null => Arc::new(NullVector::try_from_arrow_array(array)?),
            ArrowDataType::Boolean => Arc::new(BooleanVector::try_from_arrow_array(array)?),
            ArrowDataType::LargeBinary => Arc::new(BinaryVector::try_from_arrow_array(array)?),
            ArrowDataType::Int8 => Arc::new(Int8Vector::try_from_arrow_array(array)?),
            ArrowDataType::Int16 => Arc::new(Int16Vector::try_from_arrow_array(array)?),
            ArrowDataType::Int32 => Arc::new(Int32Vector::try_from_arrow_array(array)?),
            ArrowDataType::Int64 => Arc::new(Int64Vector::try_from_arrow_array(array)?),
            ArrowDataType::UInt8 => Arc::new(UInt8Vector::try_from_arrow_array(array)?),
            ArrowDataType::UInt16 => Arc::new(UInt16Vector::try_from_arrow_array(array)?),
            ArrowDataType::UInt32 => Arc::new(UInt32Vector::try_from_arrow_array(array)?),
            ArrowDataType::UInt64 => Arc::new(UInt64Vector::try_from_arrow_array(array)?),
            ArrowDataType::Float32 => Arc::new(Float32Vector::try_from_arrow_array(array)?),
            ArrowDataType::Float64 => Arc::new(Float64Vector::try_from_arrow_array(array)?),
            ArrowDataType::Utf8 => Arc::new(StringVector::try_from_arrow_array(array)?),
            ArrowDataType::Date32 => Arc::new(DateVector::try_from_arrow_array(array)?),
            ArrowDataType::Date64 => Arc::new(DateTimeVector::try_from_arrow_array(array)?),
            ArrowDataType::List(_) => Arc::new(ListVector::try_from_arrow_array(array)?),
            ArrowDataType::Timestamp(unit, _) => match unit {
                TimeUnit::Second => Arc::new(TimestampSecondVector::try_from_arrow_array(array)?),
                TimeUnit::Millisecond => {
                    Arc::new(TimestampMillisecondVector::try_from_arrow_array(array)?)
                }
                TimeUnit::Microsecond => {
                    Arc::new(TimestampMicrosecondVector::try_from_arrow_array(array)?)
                }
                TimeUnit::Nanosecond => {
                    Arc::new(TimestampNanosecondVector::try_from_arrow_array(array)?)
                }
            },
            ArrowDataType::Float16
            | ArrowDataType::Time32(_)
            | ArrowDataType::Time64(_)
            | ArrowDataType::Duration(_)
            | ArrowDataType::Interval(_)
            | ArrowDataType::Binary
            | ArrowDataType::FixedSizeBinary(_)
            | ArrowDataType::LargeUtf8
            | ArrowDataType::LargeList(_)
            | ArrowDataType::FixedSizeList(_, _)
            | ArrowDataType::Struct(_)
            | ArrowDataType::Union(_, _, _)
            | ArrowDataType::Dictionary(_, _)
            | ArrowDataType::Decimal128(_, _)
            | ArrowDataType::Decimal256(_, _)
            | ArrowDataType::Map(_, _) => {
                unimplemented!("Arrow array datatype: {:?}", array.as_ref().data_type())
            }
        })
    }

    /// Try to cast slice of `arrays` to vectors.
    pub fn try_into_vectors(arrays: &[ArrayRef]) -> Result<Vec<VectorRef>> {
        arrays.iter().map(Self::try_into_vector).collect()
    }

    /// Perform SQL like operation on `names` and a scalar `s`.
    pub fn like_utf8(names: Vec<String>, s: &str) -> Result<VectorRef> {
        let array = StringArray::from(names);

        let filter = comparison::like_utf8_scalar(&array, s).context(error::ArrowComputeSnafu)?;

        let result = compute::filter(&array, &filter).context(error::ArrowComputeSnafu)?;
        Helper::try_into_vector(result)
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::{
        ArrayRef, BooleanArray, Date32Array, Date64Array, Float32Array, Float64Array, Int16Array,
        Int32Array, Int64Array, Int8Array, LargeBinaryArray, ListArray, NullArray,
        TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
        TimestampSecondArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
    };
    use arrow::datatypes::{Field, Int32Type};
    use common_time::{Date, DateTime};

    use super::*;
    use crate::value::Value;
    use crate::vectors::ConcreteDataType;

    #[test]
    fn test_try_into_vectors() {
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(Int32Array::from(vec![1])),
            Arc::new(Int32Array::from(vec![2])),
            Arc::new(Int32Array::from(vec![3])),
        ];
        let vectors = Helper::try_into_vectors(&arrays);
        assert!(vectors.is_ok());
        let vectors = vectors.unwrap();
        vectors.iter().for_each(|v| assert_eq!(1, v.len()));
        assert_eq!(Value::Int32(1), vectors[0].get(0));
        assert_eq!(Value::Int32(2), vectors[1].get(0));
        assert_eq!(Value::Int32(3), vectors[2].get(0));
    }

    #[test]
    fn test_try_into_date_vector() {
        let vector = DateVector::from(vec![Some(1), Some(2), None]);
        let arrow_array = vector.to_arrow_array();
        assert_eq!(&ArrowDataType::Date32, arrow_array.data_type());
        let vector_converted = Helper::try_into_vector(arrow_array).unwrap();
        assert_eq!(vector.len(), vector_converted.len());
        for i in 0..vector_converted.len() {
            assert_eq!(vector.get(i), vector_converted.get(i));
        }
    }

    #[test]
    fn test_try_from_scalar_date_value() {
        let vector = Helper::try_from_scalar_value(ScalarValue::Date32(Some(42)), 3).unwrap();
        assert_eq!(ConcreteDataType::date_datatype(), vector.data_type());
        assert_eq!(3, vector.len());
        for i in 0..vector.len() {
            assert_eq!(Value::Date(Date::new(42)), vector.get(i));
        }
    }

    #[test]
    fn test_try_from_scalar_datetime_value() {
        let vector = Helper::try_from_scalar_value(ScalarValue::Date64(Some(42)), 3).unwrap();
        assert_eq!(ConcreteDataType::datetime_datatype(), vector.data_type());
        assert_eq!(3, vector.len());
        for i in 0..vector.len() {
            assert_eq!(Value::DateTime(DateTime::new(42)), vector.get(i));
        }
    }

    #[test]
    fn test_try_from_list_value() {
        let value = ScalarValue::List(
            Some(vec![
                ScalarValue::Int32(Some(1)),
                ScalarValue::Int32(Some(2)),
            ]),
            Box::new(Field::new("item", ArrowDataType::Int32, true)),
        );
        let vector = Helper::try_from_scalar_value(value, 3).unwrap();
        assert_eq!(
            ConcreteDataType::list_datatype(ConcreteDataType::int32_datatype()),
            vector.data_type()
        );
        assert_eq!(3, vector.len());
        for i in 0..vector.len() {
            let v = vector.get(i);
            let items = v.as_list().unwrap().unwrap().items().as_ref().unwrap();
            assert_eq!(vec![Value::Int32(1), Value::Int32(2)], **items);
        }
    }

    #[test]
    fn test_like_utf8() {
        fn assert_vector(expected: Vec<&str>, actual: &VectorRef) {
            let actual = actual.as_any().downcast_ref::<StringVector>().unwrap();
            assert_eq!(*actual, StringVector::from(expected));
        }

        let names: Vec<String> = vec!["greptime", "hello", "public", "world"]
            .into_iter()
            .map(|x| x.to_string())
            .collect();

        let ret = Helper::like_utf8(names.clone(), "%ll%").unwrap();
        assert_vector(vec!["hello"], &ret);

        let ret = Helper::like_utf8(names.clone(), "%time").unwrap();
        assert_vector(vec!["greptime"], &ret);

        let ret = Helper::like_utf8(names.clone(), "%ld").unwrap();
        assert_vector(vec!["world"], &ret);

        let ret = Helper::like_utf8(names, "%").unwrap();
        assert_vector(vec!["greptime", "hello", "public", "world"], &ret);
    }

    fn check_try_into_vector(array: impl Array + 'static) {
        let array: ArrayRef = Arc::new(array);
        let vector = Helper::try_into_vector(array.clone()).unwrap();
        assert_eq!(&array, &vector.to_arrow_array());
    }

    #[test]
    fn test_try_into_vector() {
        check_try_into_vector(NullArray::new(2));
        check_try_into_vector(BooleanArray::from(vec![true, false]));
        check_try_into_vector(LargeBinaryArray::from(vec![
            "hello".as_bytes(),
            "world".as_bytes(),
        ]));
        check_try_into_vector(Int8Array::from(vec![1, 2, 3]));
        check_try_into_vector(Int16Array::from(vec![1, 2, 3]));
        check_try_into_vector(Int32Array::from(vec![1, 2, 3]));
        check_try_into_vector(Int64Array::from(vec![1, 2, 3]));
        check_try_into_vector(UInt8Array::from(vec![1, 2, 3]));
        check_try_into_vector(UInt16Array::from(vec![1, 2, 3]));
        check_try_into_vector(UInt32Array::from(vec![1, 2, 3]));
        check_try_into_vector(UInt64Array::from(vec![1, 2, 3]));
        check_try_into_vector(Float32Array::from(vec![1.0, 2.0, 3.0]));
        check_try_into_vector(Float64Array::from(vec![1.0, 2.0, 3.0]));
        check_try_into_vector(StringArray::from(vec!["hello", "world"]));
        check_try_into_vector(Date32Array::from(vec![1, 2, 3]));
        check_try_into_vector(Date64Array::from(vec![1, 2, 3]));
        let data = vec![None, Some(vec![Some(6), Some(7)])];
        let list_array = ListArray::from_iter_primitive::<Int32Type, _, _>(data);
        check_try_into_vector(list_array);
        check_try_into_vector(TimestampSecondArray::from(vec![1, 2, 3]));
        check_try_into_vector(TimestampMillisecondArray::from(vec![1, 2, 3]));
        check_try_into_vector(TimestampMicrosecondArray::from(vec![1, 2, 3]));
        check_try_into_vector(TimestampNanosecondArray::from(vec![1, 2, 3]));
    }
}
