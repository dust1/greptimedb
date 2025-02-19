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

use std::any::Any;

use common_error::prelude::{ErrorCompat, ErrorExt, Snafu, StatusCode};
use snafu::Backtrace;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("Failed to serialize data, source: {}", source))]
    Serialize {
        source: serde_json::Error,
        backtrace: Backtrace,
    },

    #[snafu(display("Failed to deserialize data, source: {}, json: {}", source, json))]
    Deserialize {
        source: serde_json::Error,
        backtrace: Backtrace,
        json: String,
    },

    #[snafu(display("Failed to convert datafusion type: {}", from))]
    Conversion { from: String, backtrace: Backtrace },

    #[snafu(display("Bad array access, Index out of bounds: {}, size: {}", index, size))]
    BadArrayAccess {
        index: usize,
        size: usize,
        backtrace: Backtrace,
    },

    #[snafu(display("Unknown vector, {}", msg))]
    UnknownVector { msg: String, backtrace: Backtrace },

    #[snafu(display("Unsupported arrow data type, type: {:?}", arrow_type))]
    UnsupportedArrowType {
        arrow_type: arrow::datatypes::DataType,
        backtrace: Backtrace,
    },

    #[snafu(display("Timestamp column {} not found", name,))]
    TimestampNotFound { name: String, backtrace: Backtrace },

    #[snafu(display(
        "Failed to parse version in schema meta, value: {}, source: {}",
        value,
        source
    ))]
    ParseSchemaVersion {
        value: String,
        source: std::num::ParseIntError,
        backtrace: Backtrace,
    },

    #[snafu(display("Invalid timestamp index: {}", index))]
    InvalidTimestampIndex { index: usize, backtrace: Backtrace },

    #[snafu(display("Duplicate timestamp index, exists: {}, new: {}", exists, new))]
    DuplicateTimestampIndex {
        exists: usize,
        new: usize,
        backtrace: Backtrace,
    },

    #[snafu(display("{}", msg))]
    CastType { msg: String, backtrace: Backtrace },

    #[snafu(display("Arrow failed to compute, source: {}", source))]
    ArrowCompute {
        source: arrow::error::ArrowError,
        backtrace: Backtrace,
    },

    #[snafu(display("Unsupported column default constraint expression: {}", expr))]
    UnsupportedDefaultExpr { expr: String, backtrace: Backtrace },

    #[snafu(display("Default value should not be null for non null column"))]
    NullDefault { backtrace: Backtrace },

    #[snafu(display("Incompatible default value type, reason: {}", reason))]
    DefaultValueType {
        reason: String,
        backtrace: Backtrace,
    },

    #[snafu(display("Duplicated metadata for {}", key))]
    DuplicateMeta { key: String, backtrace: Backtrace },

    #[snafu(display("Failed to convert value into scalar value, reason: {}", reason))]
    ToScalarValue {
        reason: String,
        backtrace: Backtrace,
    },
}

impl ErrorExt for Error {
    fn status_code(&self) -> StatusCode {
        // Inner encoding and decoding error should not be exposed to users.
        StatusCode::Internal
    }

    fn backtrace_opt(&self) -> Option<&Backtrace> {
        ErrorCompat::backtrace(self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use snafu::ResultExt;

    use super::*;

    #[test]
    pub fn test_error() {
        let mut map = HashMap::new();
        map.insert(true, 1);
        map.insert(false, 2);

        let result = serde_json::to_string(&map).context(SerializeSnafu);
        assert!(result.is_err(), "serialize result is: {result:?}");
        let err = serde_json::to_string(&map)
            .context(SerializeSnafu)
            .err()
            .unwrap();
        assert!(err.backtrace_opt().is_some());
        assert_eq!(StatusCode::Internal, err.status_code());
    }
}
