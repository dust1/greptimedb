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

use common_error::prelude::*;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("Illegal Flight messages, reason: {}", reason))]
    IllegalFlightMessages {
        reason: String,
        backtrace: Backtrace,
    },

    #[snafu(display("Missing {}, expected {}, actual {}", name, expected, actual))]
    MissingResult {
        name: String,
        expected: usize,
        actual: usize,
    },

    #[snafu(display("Missing result header"))]
    MissingHeader,

    #[snafu(display("Tonic internal error, addr: {}, source: {}", addr, source))]
    TonicStatus {
        addr: String,
        source: tonic::Status,
        backtrace: Backtrace,
    },

    #[snafu(display("Error occurred on the data node, code: {}, msg: {}", code, msg))]
    Datanode { code: u32, msg: String },

    #[snafu(display("Failed to convert FlightData, source: {}", source))]
    ConvertFlightData {
        #[snafu(backtrace)]
        source: common_grpc::Error,
    },

    #[snafu(display("Column datatype error, source: {}", source))]
    ColumnDataType {
        #[snafu(backtrace)]
        source: api::error::Error,
    },

    #[snafu(display("Illegal GRPC client state: {}", err_msg))]
    IllegalGrpcClientState {
        err_msg: String,
        backtrace: Backtrace,
    },

    #[snafu(display("Missing required field in protobuf, field: {}", field))]
    MissingField { field: String, backtrace: Backtrace },

    #[snafu(display(
        "Failed to create gRPC channel, peer address: {}, source: {}",
        addr,
        source
    ))]
    CreateChannel {
        addr: String,
        #[snafu(backtrace)]
        source: common_grpc::error::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl ErrorExt for Error {
    fn status_code(&self) -> StatusCode {
        match self {
            Error::IllegalFlightMessages { .. }
            | Error::MissingResult { .. }
            | Error::MissingHeader { .. }
            | Error::TonicStatus { .. }
            | Error::Datanode { .. }
            | Error::ColumnDataType { .. }
            | Error::MissingField { .. } => StatusCode::Internal,
            Error::CreateChannel { source, .. } | Error::ConvertFlightData { source } => {
                source.status_code()
            }
            Error::IllegalGrpcClientState { .. } => StatusCode::Unexpected,
        }
    }

    fn backtrace_opt(&self) -> Option<&Backtrace> {
        ErrorCompat::backtrace(self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
