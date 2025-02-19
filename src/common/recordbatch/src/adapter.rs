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

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use datafusion::arrow::datatypes::SchemaRef as DfSchemaRef;
use datafusion::physical_plan::RecordBatchStream as DfRecordBatchStream;
use datafusion_common::DataFusionError;
use datatypes::arrow::error::{ArrowError, Result as ArrowResult};
use datatypes::schema::{Schema, SchemaRef};
use futures::ready;
use snafu::ResultExt;

use crate::error::{self, Result};
use crate::{
    DfRecordBatch, DfSendableRecordBatchStream, RecordBatch, RecordBatchStream,
    SendableRecordBatchStream, Stream,
};

type FutureStream = Pin<
    Box<
        dyn std::future::Future<
                Output = std::result::Result<DfSendableRecordBatchStream, DataFusionError>,
            > + Send,
    >,
>;

/// Greptime SendableRecordBatchStream -> DataFusion RecordBatchStream
pub struct DfRecordBatchStreamAdapter {
    stream: SendableRecordBatchStream,
}

impl DfRecordBatchStreamAdapter {
    pub fn new(stream: SendableRecordBatchStream) -> Self {
        Self { stream }
    }
}

impl DfRecordBatchStream for DfRecordBatchStreamAdapter {
    fn schema(&self) -> DfSchemaRef {
        self.stream.schema().arrow_schema().clone()
    }
}

impl Stream for DfRecordBatchStreamAdapter {
    type Item = ArrowResult<DfRecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(recordbatch)) => match recordbatch {
                Ok(recordbatch) => Poll::Ready(Some(Ok(recordbatch.into_df_record_batch()))),
                Err(e) => Poll::Ready(Some(Err(ArrowError::ExternalError(Box::new(e))))),
            },
            Poll::Ready(None) => Poll::Ready(None),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

/// DataFusion SendableRecordBatchStream -> Greptime RecordBatchStream
pub struct RecordBatchStreamAdapter {
    schema: SchemaRef,
    stream: DfSendableRecordBatchStream,
}

impl RecordBatchStreamAdapter {
    pub fn try_new(stream: DfSendableRecordBatchStream) -> Result<Self> {
        let schema =
            Arc::new(Schema::try_from(stream.schema()).context(error::SchemaConversionSnafu)?);
        Ok(Self { schema, stream })
    }
}

impl RecordBatchStream for RecordBatchStreamAdapter {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

impl Stream for RecordBatchStreamAdapter {
    type Item = Result<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(df_record_batch)) => {
                let df_record_batch = df_record_batch.context(error::PollStreamSnafu)?;
                Poll::Ready(Some(RecordBatch::try_from_df_record_batch(
                    self.schema(),
                    df_record_batch,
                )))
            }
            Poll::Ready(None) => Poll::Ready(None),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

enum AsyncRecordBatchStreamAdapterState {
    Uninit(FutureStream),
    Ready(DfSendableRecordBatchStream),
    Failed,
}

pub struct AsyncRecordBatchStreamAdapter {
    schema: SchemaRef,
    state: AsyncRecordBatchStreamAdapterState,
}

impl AsyncRecordBatchStreamAdapter {
    pub fn new(schema: SchemaRef, stream: FutureStream) -> Self {
        Self {
            schema,
            state: AsyncRecordBatchStreamAdapterState::Uninit(stream),
        }
    }
}

impl RecordBatchStream for AsyncRecordBatchStreamAdapter {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

impl Stream for AsyncRecordBatchStreamAdapter {
    type Item = Result<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match &mut self.state {
                AsyncRecordBatchStreamAdapterState::Uninit(stream_future) => {
                    match ready!(Pin::new(stream_future).poll(cx)) {
                        Ok(stream) => {
                            self.state = AsyncRecordBatchStreamAdapterState::Ready(stream);
                            continue;
                        }
                        Err(e) => {
                            self.state = AsyncRecordBatchStreamAdapterState::Failed;
                            return Poll::Ready(Some(
                                Err(e).context(error::InitRecordbatchStreamSnafu),
                            ));
                        }
                    };
                }
                AsyncRecordBatchStreamAdapterState::Ready(stream) => {
                    return Poll::Ready(ready!(Pin::new(stream).poll_next(cx)).map(|x| {
                        let df_record_batch = x.context(error::PollStreamSnafu)?;
                        RecordBatch::try_from_df_record_batch(self.schema(), df_record_batch)
                    }))
                }
                AsyncRecordBatchStreamAdapterState::Failed => return Poll::Ready(None),
            }
        }
    }

    // This is not supported for lazy stream.
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

#[cfg(test)]
mod test {
    use common_error::mock::MockError;
    use common_error::prelude::{BoxedError, StatusCode};
    use datatypes::prelude::ConcreteDataType;
    use datatypes::schema::ColumnSchema;
    use datatypes::vectors::Int32Vector;

    use super::*;
    use crate::RecordBatches;

    #[tokio::test]
    async fn test_async_recordbatch_stream_adaptor() {
        struct MaybeErrorRecordBatchStream {
            items: Vec<Result<RecordBatch>>,
        }

        impl RecordBatchStream for MaybeErrorRecordBatchStream {
            fn schema(&self) -> SchemaRef {
                unimplemented!()
            }
        }

        impl Stream for MaybeErrorRecordBatchStream {
            type Item = Result<RecordBatch>;

            fn poll_next(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if let Some(batch) = self.items.pop() {
                    Poll::Ready(Some(Ok(batch?)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        fn new_future_stream(
            maybe_recordbatches: Result<Vec<Result<RecordBatch>>>,
        ) -> FutureStream {
            Box::pin(async move {
                maybe_recordbatches
                    .map(|items| {
                        Box::pin(DfRecordBatchStreamAdapter::new(Box::pin(
                            MaybeErrorRecordBatchStream { items },
                        ))) as _
                    })
                    .map_err(|e| DataFusionError::External(Box::new(e)))
            })
        }

        let schema = Arc::new(Schema::new(vec![ColumnSchema::new(
            "a",
            ConcreteDataType::int32_datatype(),
            false,
        )]));
        let batch1 = RecordBatch::new(
            schema.clone(),
            vec![Arc::new(Int32Vector::from_slice(&[1])) as _],
        )
        .unwrap();
        let batch2 = RecordBatch::new(
            schema.clone(),
            vec![Arc::new(Int32Vector::from_slice(&[2])) as _],
        )
        .unwrap();

        let success_stream = new_future_stream(Ok(vec![Ok(batch1.clone()), Ok(batch2.clone())]));
        let adapter = AsyncRecordBatchStreamAdapter::new(schema.clone(), success_stream);
        let collected = RecordBatches::try_collect(Box::pin(adapter)).await.unwrap();
        assert_eq!(
            collected,
            RecordBatches::try_new(schema.clone(), vec![batch2.clone(), batch1.clone()]).unwrap()
        );

        let poll_err_stream = new_future_stream(Ok(vec![
            Ok(batch1.clone()),
            Err(error::Error::External {
                source: BoxedError::new(MockError::new(StatusCode::Unknown)),
            }),
        ]));
        let adapter = AsyncRecordBatchStreamAdapter::new(schema.clone(), poll_err_stream);
        let result = RecordBatches::try_collect(Box::pin(adapter)).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "Failed to poll stream, source: External error: External error, source: Unknown"
        );

        let failed_to_init_stream = new_future_stream(Err(error::Error::External {
            source: BoxedError::new(MockError::new(StatusCode::Internal)),
        }));
        let adapter = AsyncRecordBatchStreamAdapter::new(schema.clone(), failed_to_init_stream);
        let result = RecordBatches::try_collect(Box::pin(adapter)).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "Failed to init Recordbatch stream, source: External error: External error, source: Internal"
        );
    }
}
