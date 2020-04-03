use super::*;
use futures::future::{ok, ready};
use futures::stream::Stream;
use rusoto_s3::ListObjectsV2Output;
use snafu::futures::TryStreamExt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub type ListObjectsV2Result = Result<ListObjectsV2Output, RusotoError<ListObjectsV2Error>>;

/// A stream that can list objects, and (using member functions) delete or copy listed files.
pub struct ListObjects<C, S> {
    s3: C,
    config: Config,
    bucket: String,
    stream: S,
}
impl<C, S> ListObjects<C, S>
where
    C: S3 + Clone + Send + Sync + Unpin + 'static,
    S: Stream<Item = ListObjectsV2Result> + Sized + Send + 'static,
{
    pub fn boxed(self) -> ListObjects<C, Pin<Box<dyn Stream<Item = ListObjectsV2Result> + Send>>> {
        ListObjects {
            s3: self.s3,
            config: self.config,
            bucket: self.bucket,
            stream: self.stream.boxed(),
        }
    }
    pub fn delete_all(self) -> impl Future<Output = Result<(), Error>> {
        // For each ListObjectsV2Output, send a request to delete all the listed objects
        let ListObjects {
            s3,
            config,
            bucket,
            stream,
        } = self;
        stream
            .filter_map(|response| ready(response.map(|r| r.contents).transpose()))
            .map_err(|e| e.into())
            .try_for_each_concurrent(None, move |contents| {
                let s3 = s3.clone();
                let bucket = bucket.clone();
                async move {
                    s3.delete_objects(DeleteObjectsRequest {
                        bucket,
                        delete: Delete {
                            objects: contents
                                .iter()
                                .filter_map(|obj| {
                                    obj.key.as_ref().map(|key| ObjectIdentifier {
                                        key: key.clone(),
                                        version_id: None,
                                    })
                                })
                                .collect::<Vec<_>>(),
                            quiet: None,
                        },
                        ..Default::default()
                    })
                    .map_ok(drop)
                    .map_err(|e| e.into())
                    .await
                }
            })
    }
    /// Flatten into a stream of Objects.
    pub fn flatten(self) -> impl TryStream<Ok = Object, Error = RusotoError<ListObjectsV2Error>> {
        self.stream
            .try_filter_map(|response| ok(response.contents))
            .map_ok(|x| stream::iter(x).map(Ok))
            .try_flatten()
    }

    /// This function exists to provide a stream to copy all objects, for both `copy_all` and
    /// `move_all`. The `String` that is the stream's `Item` is the _source key_. An `Ok` value
    /// thus signals (relevant when used in `move_all`) that a certain key is ready for deletion.
    fn copy_all_stream<F>(
        self,
        mapping: F,
        dest_bucket: Option<String>,
    ) -> impl Stream<Item = Result<String, Error>>
    where
        F: Fn(&str) -> String + Clone + Send + Sync + Unpin + 'static,
    {
        let ListObjects {
            s3,
            config,
            bucket,
            stream,
        } = self;
        let timeout = Arc::new(Mutex::new(TimeoutState::new(config.request.clone())));
        let dest_bucket = dest_bucket.unwrap_or_else(|| bucket.clone());
        stream
            .try_filter_map(|response| ok(response.contents))
            .map_ok(|x| stream::iter(x).map(Ok))
            .try_flatten()
            .try_filter_map(|obj| {
                // Just filter out any object that does not have both of `key` and `size`
                let Object { key, size, .. } = obj;
                ok(key.and_then(|key| size.map(|size| (key, size))))
            })
            .context(err::ListObjectsV2)
            .and_then(move |(key, size)| {
                let (s3, timeout) = (s3.clone(), timeout.clone());
                let request = CopyObjectRequest {
                    copy_source: format!("{}/{}", bucket, key),
                    bucket: dest_bucket.clone(),
                    key: mapping(&key),
                    ..Default::default()
                };
                s3_request(
                    move || {
                        let (s3, request) = (s3.clone(), request.clone());
                        async move {
                            let (s3, request) = (s3.clone(), request.clone());
                            Ok((async move{s3.copy_object(request).context(err::CopyObject).await}, size as u64))
                        }
                    },
                    10,
                    timeout,
                )
                .map_ok(|_| key)
            })
    }

    /// Copy all listed objects, to a different S3 location as defined in `mapping` and
    /// `dest_bucket`.
    /// If `other_bucket` is not provided, copy to same bucket
    pub fn copy_all<F>(
        self,
        mapping: F,
        dest_bucket: Option<String>,
    ) -> impl Future<Output = Result<(), Error>>
    where
        F: Fn(&str) -> String + Clone + Send + Sync + Unpin + 'static,
    {
        self.copy_all_stream(mapping, dest_bucket)
            .try_for_each(|_| async { Ok(()) })
    }
    // TODO: Is it possible to change copy_all so that we can move_all by just chaining copy_all
    // and delete_all? Then copy_all would need to return a stream of old keys, but does that make
    // sense in general?
    // For now, this is code duplication.
    pub fn move_all<F>(
        self,
        mapping: F,
        dest_bucket: Option<String>,
    ) -> impl Future<Output = Result<(), Error>>
    where
        F: Fn(&str) -> String + Clone + Send + Sync + Unpin + 'static,
    {
        let src_bucket = self.bucket.clone();
        let timeout = Arc::new(Mutex::new(TimeoutState::new(self.config.request.clone())));
        let s3 = self.s3.clone();
        self.copy_all_stream(mapping, dest_bucket)
            .and_then(move |src_key| {
                let delete_request = DeleteObjectRequest {
                    bucket: src_bucket.clone(),
                    key: src_key,
                    ..Default::default()
                };
                let (s3, timeout) = (s3.clone(), timeout.clone());
                s3_request(
                    move || {
                        let (s3, delete_request) = (s3.clone(), delete_request.clone());
                        async move {
                            let (s3, delete_request) = (s3.clone(), delete_request.clone());
                            Ok((
                                async move {
                                    s3.delete_object(delete_request)
                                        .context(err::DeleteObject)
                                        .await
                                },
                                0,
                            ))
                        }
                    },
                    10,
                    timeout,
                )
                .map_ok(drop)
            })
            .try_for_each(|_| async { Ok(()) })
    }
}

impl<C, S> Stream for ListObjects<C, S>
where
    S: Stream<Item = Result<ListObjectsV2Output, Error>> + Sized + Send + Unpin,
    C: Unpin,
{
    type Item = Result<ListObjectsV2Output, Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.stream).poll_next(cx)
    }
}

impl<S: S3 + Clone + Send + Sync + 'static> S3Algo<S> {
    /// List all objects with a certain prefix
    pub fn list_prefix(
        &self,
        bucket: String,
        prefix: String,
    ) -> ListObjects<S, impl Stream<Item = ListObjectsV2Result> + Sized + Send> {
        let bucket1 = bucket.clone();
        self.list_objects(bucket, move || ListObjectsV2Request {
            bucket: bucket1.clone(),
            prefix: Some(prefix.clone()),
            ..Default::default()
        })
    }

    /// List all objects given a request factory.
    /// Paging is taken care of, so you need not fill in `continuation_token` in the
    /// `ListObjectsV2Request`.
    ///
    /// `bucket` is only needed for eventual further operations on `ListObjects`.
    pub fn list_objects<F>(
        &self,
        bucket: String,
        request_factory: F,
    ) -> ListObjects<S, impl Stream<Item = ListObjectsV2Result> + Sized + Send>
    where
        F: Fn() -> ListObjectsV2Request + Send + Sync + Clone,
    {
        let s3_1 = self.s3.clone();
        let stream = futures::stream::unfold(
            // Initial state = (next continuation token, first request)
            (None, true),
            // Transformation
            //    - the stream will yield ListObjectsV2Output
            //      and stop when there is nothing left to list
            move |(cont, first)| {
                let (s3, request_factory) = (s3_1.clone(), request_factory.clone());
                async move {
                    if let (&None, false) = (&cont, first) {
                        None
                    } else {
                        let result = s3
                            .list_objects_v2(ListObjectsV2Request {
                                continuation_token: cont,
                                ..request_factory()
                            })
                            .await;
                        let next_cont = if let Ok(ref response) = result {
                            response.next_continuation_token.clone()
                        } else {
                            None
                        };
                        Some((result, (next_cont, false)))
                    }
                }
            },
        );
        ListObjects {
            s3: self.s3.clone(),
            config: self.config.clone(),
            stream,
            bucket,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test::rand_string;
    #[tokio::test]
    async fn test_s3_delete_files() {
        // Minio does paging at 10'000 fles, so we need more than that.
        // It means this test will take a minutes or two.
        let s3 = testing_s3_client();
        let algo = S3Algo::new(s3);
        let dir = rand_string(14);
        const N_FILES: usize = 11_000;
        let files = (0..N_FILES).map(move |i| ObjectSource::Data {
            data: vec![1, 2, 3],
            key: format!("{}/{}.file", dir, i),
        });
        algo.upload_files(
            "test-bucket".into(),
            files,
            |result| async move {
                if result.seq % 100 == 0 {
                    println!("{} files uploaded", result.seq);
                }
            },
            PutObjectRequest::default,
        )
        .await
        .unwrap();

        // Delete all
        algo.list_prefix("test-bucket".into(), String::new())
            .delete_all()
            .await
            .unwrap();

        // List
        let count = algo
            .list_prefix("test-bucket".into(), String::new())
            .flatten()
            .try_fold(0usize, |acc, _| ok(acc + 1))
            .await
            .unwrap();

        assert_eq!(count, 0);
    }
}
