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
use datafusion_common::record_batch::RecordBatch as DfRecordBatch;
use datafusion_common::DataFusionError;
use datatypes::arrow::error::{ArrowError, Result as ArrowResult};
use datatypes::schema::{Schema, SchemaRef};
use futures::ready;
use snafu::ResultExt;

use crate::error::{self, Result};
use crate::{
    DfSendableRecordBatchStream, RecordBatch, RecordBatchStream, SendableRecordBatchStream, Stream,
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
                Ok(recordbatch) => Poll::Ready(Some(Ok(recordbatch.df_recordbatch))),
                Err(e) => Poll::Ready(Some(Err(ArrowError::External("".to_owned(), Box::new(e))))),
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
            Poll::Ready(Some(df_recordbatch)) => Poll::Ready(Some(Ok(RecordBatch {
                schema: self.schema(),
                df_recordbatch: df_recordbatch.context(error::PollStreamSnafu)?,
            }))),
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
    Inited(std::result::Result<DfSendableRecordBatchStream, DataFusionError>),
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
                    self.state = AsyncRecordBatchStreamAdapterState::Inited(ready!(Pin::new(
                        stream_future
                    )
                    .poll(cx)));
                    continue;
                }
                AsyncRecordBatchStreamAdapterState::Inited(stream) => match stream {
                    Ok(stream) => {
                        return Poll::Ready(ready!(Pin::new(stream).poll_next(cx)).map(|df| {
                            Ok(RecordBatch {
                                schema: self.schema(),
                                df_recordbatch: df.context(error::PollStreamSnafu)?,
                            })
                        }));
                    }
                    Err(e) => {
                        return Poll::Ready(Some(
                            error::CreateRecordBatchesSnafu {
                                reason: format!("Read error {:?} from stream", e),
                            }
                            .fail()
                            .map_err(|e| e.into()),
                        ))
                    }
                },
            }
        }
    }

    // This is not supported for lazy stream.
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}
